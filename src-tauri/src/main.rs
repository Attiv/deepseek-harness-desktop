// DeepSeek Harness - Tauri 桌面壳
// 启动 npx @deepseek-ai/dsh web,窗口加载 127.0.0.1:3080
// 支持导出/导入 dsh 配置(settings/credentials/skills/profiles/storages)

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

const DSH_PORT: u16 = 3080;
const DSH_URL: &str = "http://127.0.0.1:3080";
const BOOT_TIMEOUT_SECS: u64 = 180;

struct DshChild(Mutex<Option<Child>>);

/// 当前生效的快捷键(handler 动态读取,可在运行时更改)
struct CurrentShortcut(Mutex<Shortcut>);

/// 读取快捷键配置(~/.dsh/settings.yaml 里的 app-shortcut 字段)
/// 返回 Tauri 快捷键字符串,如 "Ctrl+Shift+D" 或 "Cmd+Shift+D"
fn read_shortcut() -> String {
    let default = if cfg!(target_os = "macos") {
        "Cmd+Shift+D".to_string()
    } else {
        "Ctrl+Shift+D".to_string()
    };

    let settings = dsh_home().join("settings.yaml");
    let content = match fs::read_to_string(&settings) {
        Ok(c) => c,
        Err(_) => return default,
    };

    // 简单解析 yaml 里的 app-shortcut 行
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("app-shortcut:") {
            let val = trimmed["app-shortcut:".len()..].trim();
            let val = val.trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                return val.to_string();
            }
        }
    }
    default
}

/// 写入快捷键配置到 ~/.dsh/settings.yaml
fn write_shortcut(shortcut: &str) -> Result<(), String> {
    let settings = dsh_home().join("settings.yaml");
    let content = fs::read_to_string(&settings).map_err(|e| e.to_string())?;

    // 检查是否已有 app-shortcut 行
    let mut found = false;
    let mut new_lines: Vec<String> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("app-shortcut:") {
            // 替换已有的快捷键
            let indent = line.len() - trimmed.len();
            new_lines.push(format!("{}app-shortcut: \"{}\"", " ".repeat(indent), shortcut));
            found = true;
        } else {
            new_lines.push(line.to_string());
        }
    }

    // 没找到就追加到末尾
    if !found {
        new_lines.push(format!("app-shortcut: \"{}\"", shortcut));
    }

    fs::write(&settings, new_lines.join("\n") + "\n").map_err(|e| e.to_string())
}

/// 获取 DSH 配置目录 (~/.dsh)
fn dsh_home() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    home.join(".dsh")
}

fn log_path() -> PathBuf {
    dsh_home().join(".dsh-app-launcher.log")
}

/// 需要导出的配置项(相对于 ~/.dsh)
const EXPORT_ITEMS: &[&str] = &[
    "settings.yaml",
    ".credentials.yaml",
    ".anonymous-user-id",
    "skills",
    "storages",
];

/// 需要导出的 profile 配置(不含 node_modules)
const PROFILE_FILES: &[&str] = &[
    "cordis.patch.yml",
    "cordis.yml",
    "package.json",
    "pnpm-workspace.yaml",
];

/// 检查 dsh web 是否已在跑
fn dsh_running() -> bool {
    let url = format!("http://127.0.0.1:{}", DSH_PORT);
    match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => match c.get(&url).send() {
            Ok(r) => r.status().as_u16() < 500,
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// 拉起 npx @deepseek-ai/dsh web
fn spawn_dsh() -> Option<Child> {
    let log = log_path();
    let log_file = match fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log)
    {
        Ok(f) => f,
        Err(_) => return None,
    };

    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/C", "npx", "@deepseek-ai/dsh", "web"]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args([
            "-c",
            "source ~/.zshrc 2>/dev/null || source ~/.bashrc 2>/dev/null || true; npx @deepseek-ai/dsh web",
        ]);
        c
    };

    cmd.stdin(Stdio::null())
        .stdout(std::process::Stdio::from(log_file.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(log_file));

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    match cmd.spawn() {
        Ok(child) => {
            let _ = fs::write(
                &log,
                format!(
                    "[{}] dsh 启动中, PID={}\n",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    child.id()
                ),
            );
            Some(child)
        }
        Err(e) => {
            let _ = fs::write(
                &log,
                format!("[{}] 启动 dsh 失败: {}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), e),
            );
            None
        }
    }
}

/// 递归添加文件/目录到 zip
fn add_to_zip<W: Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    base: &Path,
    rel: &Path,
    include_credentials: bool,
) -> Result<(), String> {
    let full = base.join(rel);
    if !full.exists() {
        return Ok(());
    }

    if !include_credentials && rel.to_str() == Some(".credentials.yaml") {
        return Ok(());
    }

    if full.is_dir() {
        for entry in fs::read_dir(&full).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            let new_rel = rel.join(&name);
            add_to_zip(zip, base, &new_rel, include_credentials)?;
        }
    } else {
        let rel_str = rel.to_string_lossy();
        if rel_str.contains("node_modules") || rel_str.contains("sessions/") || rel_str.contains("/target") {
            return Ok(());
        }

        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(rel_str.as_ref(), options)
            .map_err(|e| e.to_string())?;

        let mut file = File::open(&full).map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        zip.write_all(&buf).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Tauri 命令:设置快捷键(从前端 invoke 调用)
#[tauri::command]
fn set_shortcut_cmd(app: tauri::AppHandle, shortcut: String) -> Result<String, String> {
    let s = shortcut.trim();
    if s.is_empty() {
        return Err("快捷键不能为空".to_string());
    }
    let new_sc: Shortcut = s.parse().map_err(|e| format!("快捷键格式错误: {}", e))?;

    // 注销旧快捷键
    let old_str = read_shortcut();
    if let Ok(old_sc) = old_str.parse::<Shortcut>() {
        let _ = app.global_shortcut().unregister(old_sc);
    }

    // 注册新快捷键
    app.global_shortcut().register(new_sc)
        .map_err(|e| format!("注册快捷键失败: {}", e))?;

    // 写入配置文件
    write_shortcut(s)?;

    // 更新内存中的当前快捷键(handler 会动态读取)
    if let Some(state) = app.try_state::<CurrentShortcut>() {
        *state.0.lock().unwrap() = new_sc;
    }

    Ok(format!("快捷键已设为: {} (立即生效)", s))
}

/// 导出配置
fn do_export(app: &tauri::AppHandle, include_credentials: bool) -> Result<String, String> {
    let dsh = dsh_home();
    let window = app.get_webview_window("main").ok_or("找不到主窗口")?;

    let save_path = window
        .dialog()
        .file()
        .set_title("导出 DSH 配置")
        .add_filter("ZIP 文件", &["zip"])
        .set_file_name("dsh-config.zip")
        .blocking_save_file();

    let save_path = match save_path {
        Some(p) => p.into_path().map_err(|e| e.to_string())?,
        None => return Ok("cancelled".to_string()),
    };

    let file = File::create(&save_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);

    for item in EXPORT_ITEMS {
        let rel = PathBuf::from(item);
        add_to_zip(&mut zip, &dsh, &rel, include_credentials)?;
    }

    let profiles_dir = dsh.join("profiles");
    if profiles_dir.exists() {
        for entry in fs::read_dir(&profiles_dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let profile_name = entry.file_name();
            let profile_dir = entry.path();

            for pf in PROFILE_FILES {
                let pf_path = profile_dir.join(pf);
                if pf_path.exists() {
                    let rel = PathBuf::from("profiles").join(&profile_name).join(pf);
                    add_to_zip(&mut zip, &dsh, &rel, true)?;
                }
            }
        }
    }

    zip.start_file("_export-manifest.json", zip::write::SimpleFileOptions::default())
        .map_err(|e| e.to_string())?;
    let manifest = serde_json::json!({
        "version": 1,
        "exported_at": chrono::Local::now().to_rfc3339(),
        "include_credentials": include_credentials,
    });
    zip.write_all(serde_json::to_string_pretty(&manifest).unwrap().as_bytes())
        .map_err(|e| e.to_string())?;

    zip.finish().map_err(|e| e.to_string())?;

    Ok(format!("配置已导出到:\n{}", save_path.display()))
}

/// 导入配置
fn do_import(app: &tauri::AppHandle) -> Result<String, String> {
    let dsh = dsh_home();
    let window = app.get_webview_window("main").ok_or("找不到主窗口")?;

    let open_path = window
        .dialog()
        .file()
        .set_title("导入 DSH 配置")
        .add_filter("ZIP 文件", &["zip"])
        .blocking_pick_file();

    let open_path = match open_path {
        Some(p) => p.into_path().map_err(|e| e.to_string())?,
        None => return Ok("cancelled".to_string()),
    };

    let file = File::open(&open_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;

    fs::create_dir_all(&dsh).map_err(|e| e.to_string())?;

    let mut extracted = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().to_string();

        if name == "_export-manifest.json" {
            continue;
        }

        let out_path = dsh.join(&name);

        // 防 zip slip
        let canonical_dsh = dsh.canonicalize().unwrap_or_else(|_| dsh.clone());
        if !out_path.starts_with(&canonical_dsh) {
            continue;
        }

        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| e.to_string())?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut out_file = File::create(&out_path).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out_file).map_err(|e| e.to_string())?;
            extracted += 1;
        }
    }

    Ok(format!("已导入 {} 个配置文件。\n重启 DSH 后生效。", extracted))
}

/// 超时错误 HTML
fn error_html(reason: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>启动失败</title>
<style>
html,body{{margin:0;height:100%;background:#1a1a2e;color:#e0e0e0;
font-family:-apple-system,"Segoe UI",system-ui,sans-serif;display:flex;
align-items:center;justify-content:center;flex-direction:column;gap:16px;padding:40px}}
h1{{font-size:20px;color:#ff6b6b;margin:0}}
p{{font-size:14px;line-height:1.6;opacity:.85;max-width:520px;text-align:left}}
code{{background:#16213e;padding:2px 6px;border-radius:3px;font-size:13px}}
.box{{background:#16213e;padding:20px;border-radius:8px;max-width:560px;width:100%}}
</style></head>
<body>
<div class="box">
<h1>⚠️ DSH 启动失败</h1>
<p><strong>原因:</strong>{reason}</p>
<p><strong>排查步骤:</strong></p>
<p>1. 确认已安装 <a href="https://nodejs.org" style="color:#58a6ff">Node.js</a></p>
<p>2. 在终端手动执行测试:<br><code>npx @deepseek-ai/dsh web</code></p>
<p>3. 查看日志文件:<br><code>~/.dsh/.dsh-app-launcher.log</code></p>
</div>
</body></html>"#,
        reason = reason
    )
}

fn main() {
    // 单实例锁:如果已有实例在跑,显示已有窗口然后退出
    let _single = tauri_plugin_single_instance::init(|app, _args, _cwd| {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.set_focus();
        }
    });

    let mut child = if dsh_running() {
        None
    } else {
        spawn_dsh()
    };
    let had_child = child.is_some();

    // 读取用户配置的快捷键
    let shortcut_str = read_shortcut();
    let shortcut: Shortcut = shortcut_str
        .parse()
        .unwrap_or_else(|_| {
            if cfg!(target_os = "macos") {
                "Cmd+Shift+D".parse().unwrap()
            } else {
                "Ctrl+Shift+D".parse().unwrap()
            }
        });

    tauri::Builder::default()
        .plugin(_single)
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![set_shortcut_cmd])
        .manage(CurrentShortcut(Mutex::new(shortcut)))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, sc, event| {
                    if event.state != ShortcutState::Pressed {
                        return;
                    }
                    // 从 state 动态读取当前快捷键(支持运行时更改)
                    let current = if let Some(state) = app.try_state::<CurrentShortcut>() {
                        *state.0.lock().unwrap()
                    } else {
                        return;
                    };
                    if *sc == current {
                        if let Some(window) = app.get_webview_window("main") {
                            if window.is_visible().unwrap_or(false) {
                                let _ = window.hide();
                            } else {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                    }
                })
                .build(),
        )
        .setup(move |app| {
            // 注册全局快捷键
            app.global_shortcut().register(shortcut)
                .map_err(|e| format!("注册快捷键失败: {}", e))?;

            // 构建原生菜单
            let export_no_cred = MenuItemBuilder::with_id("export-no-cred", "导出配置(不含 API Keys)")
                .build(app)?;
            let export_with_cred = MenuItemBuilder::with_id("export-cred", "导出配置(含 API Keys)")
                .build(app)?;
            let import_item = MenuItemBuilder::with_id("import", "导入配置…")
                .build(app)?;
            let quit_item = MenuItemBuilder::with_id("quit", "退出 DeepSeek Harness")
                .build(app)?;
            let toggle_item = MenuItemBuilder::with_id("toggle", format!("显示/隐藏窗口 ({})", shortcut_str))
                .build(app)?;
            let set_shortcut_item = MenuItemBuilder::with_id("set-shortcut", "设置快捷键…")
                .build(app)?;

            let config_submenu = SubmenuBuilder::new(app, "配置")
                .item(&toggle_item)
                .item(&set_shortcut_item)
                .separator()
                .item(&export_no_cred)
                .item(&export_with_cred)
                .separator()
                .item(&import_item)
                .separator()
                .item(&quit_item)
                .build()?;

            let menu = MenuBuilder::new(app).item(&config_submenu).build()?;
            app.set_menu(menu)?;

            // 创建主窗口
            let main_window = WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::App("index.html".into()),
            )
            .title("DeepSeek Harness")
            .inner_size(1280.0, 840.0)
            .min_inner_size(900.0, 600.0)
            .center()
            .visible(false)
            .build()?;

            let window = main_window.clone();
            tauri::async_runtime::spawn(async move {
                let deadline = Instant::now() + Duration::from_secs(BOOT_TIMEOUT_SECS);

                loop {
                    if Instant::now() > deadline {
                        let reason = if had_child {
                            "npx 已启动但 dsh web 长时间未就绪(超过 180 秒)。可能是首次下载较慢,或 dsh 启动报错。"
                        } else {
                            "无法启动 npx 进程。请确认已安装 Node.js。"
                        };
                        let html = error_html(reason);
                        let _ = window.eval(&format!(
                            "document.documentElement.innerHTML = {};",
                            serde_json::json!(html)
                        ));
                        let _ = window.show();
                        break;
                    }
                    if dsh_running() {
                        let _ = window.navigate(
                            DSH_URL
                                .parse()
                                .unwrap_or_else(|_| {
                                    format!("http://127.0.0.1:{}", DSH_PORT)
                                        .parse()
                                        .unwrap()
                                }),
                        );
                        std::thread::sleep(Duration::from_millis(500));
                        let _ = window.show();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(700));
                }
            });

            Ok(())
        })
        .on_menu_event(move |app, event| {
            match event.id().as_ref() {
                "toggle" => {
                    if let Some(window) = app.get_webview_window("main") {
                        if window.is_visible().unwrap_or(false) {
                            let _ = window.hide();
                        } else {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    return;
                }
                "set-shortcut" => {
                    let current = read_shortcut();
                    let prompt_msg = format!(
                        "当前快捷键: {}\n\n请输入新快捷键组合:\n(留空取消,设置后立即生效)\n\n格式: Ctrl+Q, Alt+Space, Cmd+Shift+H",
                        current
                    );
                    if let Some(window) = app.get_webview_window("main") {
                        let js = format!(
                            r#"(function(){{
                                var v = prompt({});
                                if(v && v.trim()){{
                                    window.__TAURI_INTERNALS__.invoke('set_shortcut_cmd', {{shortcut: v.trim()}})
                                        .then(function(msg){{ alert(msg); }})
                                        .catch(function(e){{ alert('错误: ' + e); }});
                                }}
                            }})()"#,
                            serde_json::to_string(&prompt_msg).unwrap()
                        );
                        let _ = window.eval(&js);
                    }
                    return;
                }
                "quit" => {
                    app.exit(0);
                    return;
                }
                _ => {}
            }

            let msg = match event.id().as_ref() {
                "export-no-cred" => do_export(app, false),
                "export-cred" => do_export(app, true),
                "import" => do_import(app),
                _ => return,
            };

            if let Ok(result) = msg {
                if result != "cancelled" {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.eval(&format!(
                            "alert({});",
                            serde_json::to_string(&result).unwrap()
                        ));
                    }
                }
            } else if let Err(e) = msg {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.eval(&format!(
                        "alert('错误: ' + {});",
                        serde_json::to_string(&e).unwrap()
                    ));
                }
            }
        })
        .manage(DshChild(Mutex::new(child.take())))
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    // 隐藏窗口而不是退出,这样快捷键能重新唤回
                    // 真正退出通过菜单「退出」或系统托盘
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("构建 Tauri 应用失败")
        .run(|_app, _event| {});
}
