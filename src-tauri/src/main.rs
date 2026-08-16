// DeepSeek Harness - Tauri 桌面壳
// 启动 npx @deepseek-ai/dsh web,窗口加载 127.0.0.1:3080

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{WebviewUrl, WebviewWindowBuilder};

const DSH_PORT: u16 = 3080;
const DSH_URL: &str = "http://127.0.0.1:3080";
const BOOT_TIMEOUT_SECS: u64 = 180; // 首次 npx 下载 dsh 较慢,给 3 分钟

struct DshChild(Mutex<Option<Child>>);

/// 检查 dsh web 是否已在跑(URL 可达)
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

/// 日志文件路径(写到用户目录,方便排查)
fn log_path() -> std::path::PathBuf {
    let dir = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    dir.join(".dsh-app-launcher.log")
}

/// 拉起 npx @deepseek-ai/dsh web
fn spawn_dsh() -> Option<Child> {
    let log = log_path();

    // 打开日志文件(子进程的 stdout/stderr 都写到这里)
    let log_file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("无法创建日志文件 {:?}: {}", log, e);
            return None;
        }
    };

    let mut cmd = if cfg!(target_os = "windows") {
        // Windows: npx 是 npx.cmd,用 cmd /C 包一层
        let mut c = Command::new("cmd");
        c.args(["/C", "npx", "@deepseek-ai/dsh", "web"]);
        c
    } else {
        // macOS / Linux: 用 login shell 执行,确保 PATH 完整
        // (GUI app 的默认 PATH 不含 nvm/homebrew,直接 spawn npx 会失败)
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
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    match cmd.spawn() {
        Ok(child) => {
            // 写一行标记到日志开头
            let _ = std::fs::write(
                &log,
                format!(
                    "[{}] dsh 启动中, PID={}\n",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    child.id()
                ),
            );
            // 注意:上面 OpenOptions truncate 了文件,这里重新写;
            // 子进程的 stdout/stderr 句柄已经在 truncate 之前获取,
            // 所以子进程写入会追加到我们写的标记后面。
            Some(child)
        }
        Err(e) => {
            let msg = format!(
                "[{}] 启动 dsh 失败: {}\n\
                 请确认已安装 Node.js,且 npx 命令可用。\n\
                 手动测试: npx @deepseek-ai/dsh web\n",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                e
            );
            let _ = std::fs::write(&log, &msg);
            None
        }
    }
}

/// 生成超时时的错误 HTML(显示在窗口里,而不是空白加载页)
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
<p>3. 查看日志文件:<br><code>~/.dsh-app-launcher.log</code></p>
</div>
</body></html>"#,
        reason = reason
    )
}

fn main() {
    // 已有实例在跑?没就拉一个
    let mut child = if dsh_running() {
        None
    } else {
        spawn_dsh()
    };
    // 在 move 之前记录是否启动了子进程
    let had_child = child.is_some();

    tauri::Builder::default()
        .manage(DshChild(Mutex::new(child.take())))
        .setup(move |app| {
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
            // 后台线程:轮询 dsh 就绪后导航并显示窗口
            tauri::async_runtime::spawn(async move {
                let deadline =
                    Instant::now() + Duration::from_secs(BOOT_TIMEOUT_SECS);

                loop {
                    if Instant::now() > deadline {
                        // 超时:显示错误页面
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
                        // 就绪:导航到 dsh 并显示
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
