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

const DSH_PORT: u16 = 3080;
const DSH_URL: &str = "http://127.0.0.1:3080";
const BOOT_TIMEOUT_SECS: u64 = 180;

struct DshChild(Mutex<Option<Child>>);

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
    let mut child = if dsh_running() {
        None
    } else {
        spawn_dsh()
    };
    let had_child = child.is_some();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(move |app| {
            // 构建原生菜单:配置 > [导出(不含Keys), 导出(含Keys), 导入]
            let export_no_cred = MenuItemBuilder::with_id("export-no-cred", "导出配置(不含 API Keys)")
                .build(app)?;
            let export_with_cred = MenuItemBuilder::with_id("export-cred", "导出配置(含 API Keys)")
                .build(app)?;
            let import_item = MenuItemBuilder::with_id("import", "导入配置…")
                .build(app)?;

            let config_submenu = SubmenuBuilder::new(app, "配置")
                .item(&export_no_cred)
                .item(&export_with_cred)
                .separator()
                .item(&import_item)
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
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                if window.label() == "main" {
                    std::process::exit(0);
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("构建 Tauri 应用失败")
        .run(|_app, _event| {});
}
