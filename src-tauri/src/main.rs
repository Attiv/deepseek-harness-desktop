// DeepSeek Harness - Tauri 桌面壳
// 启动 npx @deepseek-ai/dsh web,窗口加载 127.0.0.1:3080

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

const DSH_PORT: u16 = 3080;
const DSH_URL: &str = "http://127.0.0.1:3080";
const BOOT_TIMEOUT_SECS: u64 = 120;

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

/// 拉起 npx @deepseek-ai/dsh web(隐藏窗口)
fn spawn_dsh() -> Option<Child> {
    // Windows 上 npx 是 npx.cmd
    let mut cmd = if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/C", "npx", "@deepseek-ai/dsh", "web"]);
        c
    } else {
        let mut c = Command::new("npx");
        c.args(["@deepseek-ai/dsh", "web"]);
        c
    };

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // CREATE_NO_WINDOW = 0x08000000
        ;

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    match cmd.spawn() {
        Ok(child) => Some(child),
        Err(e) => {
            eprintln!("启动 dsh 失败: {}", e);
            None
        }
    }
}

fn main() {
    // 已有实例在跑?没就拉一个
    let mut child = if dsh_running() {
        None
    } else {
        spawn_dsh()
    };

    tauri::Builder::default()
        .manage(DshChild(Mutex::new(child.take())))
        .setup(|app| {
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
                        // 超时:仍然显示窗口(至少展示加载页)
                        let _ = window.show();
                        break;
                    }
                    if dsh_running() {
                        // 就绪:导航到 dsh 并显示
                        let _ = window
                            .navigate(DSH_URL.parse().unwrap_or_else(|_| {
                                format!("http://127.0.0.1:{}", DSH_PORT)
                                    .parse()
                                    .unwrap()
                            }));
                        // 给 dsh 前端一点渲染时间
                        std::thread::sleep(Duration::from_millis(400));
                        let _ = window.show();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(700));
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            // 主窗口关闭时退出整个应用(连带子进程)
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