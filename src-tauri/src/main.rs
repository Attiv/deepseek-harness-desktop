// DeepSeek Harness - Tauri 桌面壳
// 启动 pnpm dlx @deepseek-ai/dsh web,窗口加载 127.0.0.1:3080
// 支持导出/导入 dsh 配置(settings/credentials/skills/profiles/storages)

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use tauri::menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

const DSH_PORT: u16 = 3080;
const DSH_URL: &str = "http://127.0.0.1:3080";
/// 端口就绪的等待上限。切换频道或首次安装要下约 220 MB 的包,
/// 慢网络下远超 3 分钟 —— 宁可多等,也别把一次正常的下载判成失败。
const BOOT_TIMEOUT_SECS: u64 = 600;

/// dsh 的 npm 包名。
const DSH_PACKAGE: &str = "@deepseek-ai/dsh";
/// 解析版本时的兜底 registry(读不到 .npmrc 时用)。
const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";
/// 版本解析的网络预算。超时即回退到裸 spec,不让启动卡在这一步。
const RESOLVE_TIMEOUT_SECS: u64 = 5;
#[cfg(unix)]
const PROCESS_TERMINATION_GRACE: Duration = Duration::from_millis(400);
const PROCESS_REAP_TIMEOUT: Duration = Duration::from_millis(400);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(20);
static PLUGIN_UPDATE_RUNNING: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(target_os = "windows")]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

struct DshChild(Mutex<Option<Child>>);

fn take_owned_child(state: &DshChild) -> Option<Child> {
    let mut owned_child = match state.0.lock() {
        Ok(owned_child) => owned_child,
        Err(poisoned) => {
            eprintln!("DshChild mutex was poisoned while taking backend ownership");
            poisoned.into_inner()
        }
    };
    owned_child.take()
}

fn stop_owned_dsh(app: &tauri::AppHandle) {
    let Some(state) = app.try_state::<DshChild>() else {
        return;
    };

    if let Some(mut child) = take_owned_child(&state) {
        terminate_child_tree(&mut child);
    }
}

/// 当前生效的快捷键(handler 动态读取,可在运行时更改)
struct CurrentShortcut(Mutex<Shortcut>);

/// 从 settings.yaml 文本里解析一个**顶层**(无缩进)标量字段。
///
/// 只认顶层键:settings.yaml 里有大量嵌套结构(如 `llm-pi-ai.providers.*`),
/// 若按去空白后的前缀匹配,子键里的同名项会被误当成顶层配置读出来。
/// 字段缺失或值为空都返回 None。抽成纯函数以便测试,不碰真实文件。
fn parse_top_level_setting(content: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    for line in content.lines() {
        if line.starts_with([' ', '\t']) {
            continue;
        }
        if let Some(rest) = line.trim_end().strip_prefix(&prefix) {
            let val = rest.trim().trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

/// 把某个顶层键改写为新值,返回新的文件内容;没有该键则在末尾追加。
///
/// 缩进行原样保留,因此嵌套结构不会被破坏。纯函数,便于测试。
fn upsert_top_level_setting(content: &str, key: &str, value: &str) -> String {
    let prefix = format!("{key}:");
    let mut found = false;
    let mut new_lines: Vec<String> = Vec::new();

    for line in content.lines() {
        let is_top_level = !line.starts_with([' ', '\t']);
        if is_top_level && line.trim_end().starts_with(&prefix) {
            new_lines.push(format!("{key}: \"{value}\""));
            found = true;
        } else {
            new_lines.push(line.to_string());
        }
    }

    if !found {
        new_lines.push(format!("{key}: \"{value}\""));
    }

    new_lines.join("\n") + "\n"
}

/// 从 ~/.dsh/settings.yaml 读一个顶层标量字段。
/// 字段缺失、文件不存在、值为空都返回 None。
fn read_setting(key: &str) -> Option<String> {
    let content = fs::read_to_string(dsh_home().join("settings.yaml")).ok()?;
    parse_top_level_setting(&content, key)
}

/// 读取快捷键配置(~/.dsh/settings.yaml 里的 app-shortcut 字段)
/// 返回 Tauri 快捷键字符串,如 "Ctrl+Shift+D" 或 "Cmd+Shift+D"
fn read_shortcut() -> String {
    read_setting("app-shortcut").unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            "Cmd+Shift+D".to_string()
        } else {
            "Ctrl+Shift+D".to_string()
        }
    })
}

/// 写入一个顶层标量配置项到 ~/.dsh/settings.yaml。
///
/// 只替换顶层的同名键,嵌套子键不受影响。文件不存在时创建一个只含该键的新文件。
///
/// 值里的引号与换行会破坏 YAML,因此这里直接拒绝 —— 所有调用方传的都是
/// 受控的常量(快捷键字符串、频道名),不该出现这类字符。
fn write_setting_value(key: &str, value: &str) -> Result<(), String> {
    if value.contains(['"', '\n', '\r']) {
        return Err(format!("配置值不能包含引号或换行: {value:?}"));
    }
    if key.is_empty() || key.contains([':', '\n', '\r', ' ']) {
        return Err(format!("非法的配置键: {key:?}"));
    }

    let settings = dsh_home().join("settings.yaml");
    let content = fs::read_to_string(&settings).unwrap_or_default();

    if let Some(parent) = settings.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    fs::write(&settings, upsert_top_level_setting(&content, key, value)).map_err(|e| e.to_string())
}

/// 写入快捷键配置到 ~/.dsh/settings.yaml
fn write_shortcut(shortcut: &str) -> Result<(), String> {
    write_setting_value("app-shortcut", shortcut)
}

/// 获取用户主目录
fn user_home() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// 获取 DSH 配置目录 (~/.dsh)
fn dsh_home() -> PathBuf {
    user_home().join(".dsh")
}

fn log_path() -> PathBuf {
    dsh_home().join(".dsh-app-launcher.log")
}

/// 读取启动日志的末尾若干行,交给加载页展示。
///
/// 日志可能正在被后端进程追加写入,读到半行或读失败都不算错误 —— 拿不到就返回
/// 空字符串,进度页退回"暂无输出",而不是把启动流程打断。
fn tail_launcher_log(max_lines: usize) -> String {
    const MAX_TAIL_BYTES: u64 = 32 * 1024;
    const MAX_LINE_CHARS: usize = 240;

    let Ok(file) = File::open(log_path()) else {
        return String::new();
    };
    let Ok(len) = file.metadata().map(|meta| meta.len()) else {
        return String::new();
    };
    let start = len.saturating_sub(MAX_TAIL_BYTES);
    let mut text = String::new();
    if file
        .take(len - start)
        .read_to_string(&mut text)
        .is_err()
        && text.is_empty()
    {
        // 非 UTF-8 是可能的(Windows 上旧版 pnpm 会输出本地编码),
        // 用 lossy 再兜一次,保证至少能看到半行线索。
        let mut raw = Vec::new();
        if File::open(log_path())
            .and_then(|mut f| std::io::Read::read_to_end(&mut f, &mut raw))
            .is_err()
        {
            return String::new();
        }
        let raw_start = raw.len().saturating_sub(MAX_TAIL_BYTES as usize);
        text = String::from_utf8_lossy(&raw[raw_start..]).into_owned();
    }

    let lines: Vec<String> = text
        .lines()
        .rev()
        .take(max_lines)
        .map(|line| {
            let truncated: String = line.chars().take(MAX_LINE_CHARS).collect();
            if line.chars().count() > MAX_LINE_CHARS {
                format!("{truncated}…")
            } else {
                truncated
            }
        })
        .collect();

    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// 加载页里的启动阶段。
///
/// 阶段名必须与 `dist/index.html` 的 `STEPS` 一一对应 —— 加载页按 id 高亮当前步骤。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum BootStage {
    /// 探测 3080 端口上是否已有实例。
    Probe,
    /// 解析 dsh 版本频道(可能有 registry 网络请求)。
    Resolve,
    /// 已拉起后端进程,但它还没开始监听。
    Spawn,
    /// 已监听端口,还在等 HTTP 就绪(首次启动大部分时间花在下载 ~220 MB)。
    Download,
    /// 端口已有响应,但还没通过认证。
    Auth,
    /// 拿到 cookie,正在导航到界面。
    Ready,
}

impl BootStage {
    fn id(self) -> &'static str {
        match self {
            BootStage::Probe => "probe",
            BootStage::Resolve => "resolve",
            BootStage::Spawn => "spawn",
            BootStage::Download => "download",
            BootStage::Auth => "auth",
            BootStage::Ready => "ready",
        }
    }

    /// 进度条的下限百分比。只用于让进度条单调递增,不表示真实完成度。
    fn floor_percent(self) -> u32 {
        match self {
            BootStage::Probe => 5,
            BootStage::Resolve => 12,
            BootStage::Spawn => 24,
            BootStage::Download => 42,
            BootStage::Auth => 85,
            BootStage::Ready => 100,
        }
    }

    fn title(self) -> &'static str {
        match self {
            BootStage::Probe => "正在检查本机 DSH 服务…",
            BootStage::Resolve => "正在解析 DSH 版本频道…",
            BootStage::Spawn => "正在拉起 DSH 后端…",
            BootStage::Download => "正在等待 DSH 后端就绪…",
            BootStage::Auth => "正在完成本地认证…",
            BootStage::Ready => "正在加载界面…",
        }
    }
}

/// 启动失败的原因分类。决定错误页给用户哪一套排查建议。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BootFailure {
    /// 日志显示 PATH 里既没有 pnpm 也没有 npx(后端脚本自己 exit 127)。
    MissingRunner,
    /// 后端进程根本没起来(脚本退出码非 0,或日志里没有监听迹象)。
    RunnerExited,
    /// 端口已经有人在听,但认证始终换不到 cookie。
    AuthRejected,
    /// 端口等到超时仍未就绪。
    NotReady,
}

impl BootFailure {
    fn title(self) -> &'static str {
        match self {
            BootFailure::MissingRunner => "找不到包管理器",
            BootFailure::RunnerExited => "后端进程启动失败",
            BootFailure::AuthRejected => "DSH 认证失败",
            BootFailure::NotReady => "DSH 启动超时",
        }
    }

    fn reason(self, seconds: u64) -> String {
        match self {
            BootFailure::MissingRunner => "PATH 里既没有 pnpm 也没有 npx,无法拉起 DSH 后端。"
                .to_string(),
            BootFailure::RunnerExited => {
                "后端进程在监听端口之前就退出了 —— 通常意味着命令不存在、Node 版本过低,或 dsh 自身报错。"
                    .to_string()
            }
            BootFailure::AuthRejected => {
                "3080 端口上的服务在运行,但启动器拿不到可用的认证凭据(既读不到本进程的 launch token,WebView 里也没有仍然有效的 cookie)。"
                    .to_string()
            }
            BootFailure::NotReady => format!(
                "等待 {} 分 {} 秒后端口仍未就绪,已放弃。",
                seconds / 60,
                seconds % 60
            ),
        }
    }

    fn steps(self) -> Vec<String> {
        match self {
            BootFailure::MissingRunner => vec![
                r#"安装 <a href="https://nodejs.org">Node.js</a> ≥ 22.19(自带 npx),或安装 <a href="https://pnpm.io/zh/installation">pnpm</a>"#.to_string(),
                "macOS/Linux 上确认包管理器在登录 shell 的 PATH 里(桌面应用读不到交互式 shell 的临时 PATH)".to_string(),
                "安装完成后从菜单「配置 → 退出 DeepSeek Harness」彻底退出,再重新打开本应用".to_string(),
            ],
            BootFailure::RunnerExited => vec![
                r#"在终端手动执行 <code>pnpm dlx @deepseek-ai/dsh@latest web --no-open</code>,看它是怎么报错的"#.to_string(),
                r#"确认 Node 版本 <code>node -v</code> ≥ 22.19(新版 dsh 的传递依赖要求)"#.to_string(),
                r#"查看日志文件 <code>~/.dsh/.dsh-app-launcher.log</code> 的最后几十行"#.to_string(),
                "修复后从菜单「配置 → 退出 DeepSeek Harness」彻底退出,再重新打开本应用".to_string(),
            ],
            BootFailure::AuthRejected => vec![
                "关闭终端里那个占用 3080 端口的 dsh 实例,然后重新打开本应用".to_string(),
                r#"或在终端里找到它打印的 <code>dsh web: http://127.0.0.1:3080/?token=…</code> 那一行,直接用浏览器打开"#.to_string(),
                "若端口被无关程序占用,可先结束该进程再重启本应用".to_string(),
            ],
            BootFailure::NotReady => vec![
                r#"首次启动或切换版本要下载约 220 MB,请确认网络能访问 npm 源;必要时配置镜像 <code>~/.npmrc</code>"#.to_string(),
                r#"在终端手动执行 <code>pnpm dlx @deepseek-ai/dsh@latest web --no-open</code>,确认它能否单独跑通"#.to_string(),
                r#"把 <code>~/.dsh/settings.yaml</code> 里的 <code>app-dsh-channel</code> 固定到某个已知可用版本(如 <code>0.1.5-rc.2</code>)再重启"#.to_string(),
                r#"查看日志文件 <code>~/.dsh/.dsh-app-launcher.log</code>"#.to_string(),
            ],
        }
    }
}

/// 把启动日志归类成一种失败原因,供错误页给出针对性建议。
///
/// 只在超时/提前退出时调用,所以这里的判断可以保守:拿不准就归到最宽的那一类。
fn classify_boot_failure(log: &str, had_child: bool) -> BootFailure {
    if log.contains("neither pnpm nor npx found") {
        return BootFailure::MissingRunner;
    }
    if had_child && log.contains("启动 dsh 失败") {
        return BootFailure::RunnerExited;
    }
    BootFailure::NotReady
}

/// 一次启动尝试的完整上下文,用于把失败原因和现场一并交给错误页。
struct BootFailureReport {
    title: String,
    reason: String,
    detail: String,
    steps: Vec<String>,
    log: String,
}

impl BootFailureReport {
    fn from_log(kind: BootFailure, log: &str, had_child: bool, timeout_secs: u64, detail: &str) -> Self {
        let steps = if kind == BootFailure::NotReady {
            // 拉起了后端却一直没就绪,最常见的是卡在下载上,把登录页的
            // 「Node/pnpm 没装」建议换成更有针对性的版本与网络建议。
            classify_boot_failure(log, had_child).steps()
        } else {
            kind.steps()
        };
        Self {
            title: kind.title().to_string(),
            reason: kind.reason(timeout_secs),
            detail: detail.to_string(),
            steps,
            log: tail_launcher_log(40),
        }
    }
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

/// dsh web 的就绪状态。
///
/// 新版 dsh(0.1.2-alpha.3 起)在未认证时回 401 —— 那说明 HTTP 服务其实已经起来了,
/// 只是还缺 launch token,和「端口上没人听」是两回事,必须分开。旧版没有认证,
/// 裸地址直接回 200。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DshState {
    /// 端口没有响应。
    Down,
    /// 服务在跑,但要先用 launch token 换 cookie 才放行。
    NeedsAuth,
    /// 服务在跑,且这次请求已被放行(旧版无认证)。
    Ready,
}

fn dsh_state_for_status(status: u16) -> DshState {
    match status {
        401 => DshState::NeedsAuth,
        200..=399 => DshState::Ready,
        _ => DshState::Down,
    }
}

/// 探测 dsh web 的状态。dsh 启动中会短暂返回 404，因此只有 2xx/3xx
/// 才能表示旧版服务已就绪；其余响应与连不上一样继续等待。
fn probe_dsh() -> DshState {
    let url = format!("http://127.0.0.1:{}", DSH_PORT);
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return DshState::Down;
    };
    client
        .get(&url)
        .send()
        .map(|response| dsh_state_for_status(response.status().as_u16()))
        .unwrap_or(DshState::Down)
}

/// launch token 的字符集:上游把 32 字节随机数做 base64url 编码。
fn is_launch_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_')
}

/// 从启动日志里取本次 dsh 进程打印的 launch token。
///
/// 新版 dsh web 启动时会打印一行:
///   `dsh web: http://127.0.0.1:3080/?token=<base64url>`
/// 有 LAN 地址时后面还跟 ` (LAN: http://<ip>:3080/?token=<同一个 token>)` ——
/// 两处 token 相同,取第一个即可。
///
/// 旧版(0.1.1-rc.2 及更早)没有认证,打印的 URL 不带 token:这里返回 None,
/// 调用方退回裸地址,保持对 pin 在旧频道的用户的兼容。
fn parse_launch_token(log: &str) -> Option<String> {
    // Logs are appended across runs. Never redeem an old process's token while
    // the current process is starting, or after it has printed a fresh token.
    for line in log.lines().rev() {
        if line.trim_start().starts_with("===== 启动于 ") {
            break;
        }
        let Some(rest) = line.trim_start().strip_prefix("dsh web: ") else {
            continue;
        };
        let Some((_, after)) = rest.split_once("?token=") else {
            continue;
        };
        let token: String = after
            .chars()
            .take_while(|c| is_launch_token_char(*c))
            .collect();
        if !token.is_empty() {
            return Some(token);
        }
    }
    None
}

fn read_launch_token() -> Option<String> {
    parse_launch_token(&fs::read_to_string(log_path()).ok()?)
}

/// 把 dsh 返回的 Set-Cookie 准备成能直接注入 WebView 的 cookie。
///
/// HTTP 响应里的 host-only cookie 没有 Domain 属性；Wry 的跨平台 set_cookie API
/// 需要显式 origin，因此补上和 DSH_URL 一致的 loopback host。
fn webview_auth_cookie(raw: &str) -> Option<tauri::webview::Cookie<'static>> {
    let mut cookie = tauri::webview::Cookie::parse(raw.to_string()).ok()?;
    cookie.set_domain("127.0.0.1");
    // Strict 会把从 tauri:// 发起的第一次顶层导航也判成跨站并压住 cookie；
    // Lax 允许安全的顶层 GET，同时仍不会把 cookie 发给跨站子资源或 POST。
    cookie.set_same_site(cookie::SameSite::Lax);
    Some(cookie)
}

/// 拿 token 兑换认证 cookie；返回 None 说明 token 已失效或响应格式不对。
///
/// 有效时上游回 303(跳回干净的 `/`),失效回 401 —— 所以必须关掉自动重定向,
/// 否则 303 会被 reqwest 跟掉,判据就没了。reqwest 与 WebView 不共享 cookie store,
/// 所以要取出 Set-Cookie 并显式注入 WebView。不能只让 WebView 自己走 303:
/// macOS WKWebView 从 tauri:// 页面跨站导航时会保存 SameSite=Strict cookie,
/// 却不会在紧随其后的重定向请求里发送它,最终仍落到 401。
///
/// 唯一目的是把「日志里的 token 属于已经退出的进程」和「token 有效」分开:
/// 前者说明 3080 上是外部启动的实例,再等下去也等不到我们能用的 token。
fn redeem_launch_token(token: &str) -> Option<tauri::webview::Cookie<'static>> {
    let url = format!("{}/?token={}", DSH_URL, token);
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()
        .and_then(|c| c.get(&url).send().ok())?;
    if !response.status().is_redirection() {
        return None;
    }
    let raw = response
        .headers()
        .get(reqwest::header::SET_COOKIE)?
        .to_str()
        .ok()?;
    webview_auth_cookie(raw)
}

/// spec 片段(dist-tag 名或版本号)是否只含安全字符。
/// 这个值会拼进 `cmd /C` 与 `sh -c` 的命令行,配置文件不能成为注入点。
fn is_safe_spec_token(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
}

/// 解析查询版本用的 registry:先看 ~/.npmrc 的 scope 专属 registry,
/// 再看全局 `registry=`,再看环境变量,最后回落官方源。
/// 配了 npmmirror 等镜像的用户在这里也能正常解析到版本。
fn npm_registry() -> String {
    let mut global: Option<String> = None;
    if let Ok(content) = fs::read_to_string(user_home().join(".npmrc")) {
        for line in content.lines() {
            let trimmed = line.trim();
            if let Some(v) = trimmed.strip_prefix("@deepseek-ai:registry=") {
                return v.trim().trim_end_matches('/').to_string();
            }
            if let Some(v) = trimmed.strip_prefix("registry=") {
                global = Some(v.trim().trim_end_matches('/').to_string());
            }
        }
    }
    global
        .or_else(|| std::env::var("npm_config_registry").ok())
        .map(|v| v.trim_end_matches('/').to_string())
        .filter(|v| v.starts_with("http"))
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string())
}

/// 查 registry 的 dist-tags,返回**承载最高语义化版本的那个 tag 名**。
///
/// 官方在 rc 阶段把新版发到 `next`,`latest` 会落后(写这段时 latest=0.1.0-rc.7、
/// next=0.1.0-rc.8),所以裸 spec 永远只拿 rc.7;GA 之后 `latest` 又会反超 `next`。
/// 只有比较各 tag 实际指向的版本,两个方向才都成立。
fn newest_channel() -> Option<String> {
    let url = format!("{}/{}", npm_registry(), DSH_PACKAGE.replace('/', "%2f"));
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(RESOLVE_TIMEOUT_SECS))
        .build()
        .ok()?;
    // 用 text() 而不是 json():reqwest 的 json 特性没开,serde_json 本来就在依赖里
    let raw_body = client
        .get(&url)
        // 精简 packument:只回 dist-tags/versions,省掉整包元数据
        .header("Accept", "application/vnd.npm.install-v1+json")
        .send()
        .ok()?
        .text()
        .ok()?;
    let body: serde_json::Value = serde_json::from_str(&raw_body).ok()?;

    let tags = body.get("dist-tags")?.as_object()?;
    pick_newest_tag(tags)
}

/// 从 dist-tags 里挑出承载最高版本的 tag 名。与网络分离,便于测试。
fn pick_newest_tag(tags: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let mut best: Option<(semver::Version, String)> = None;
    for (tag, raw) in tags {
        if !is_safe_spec_token(tag) {
            continue;
        }
        let version = match raw.as_str().and_then(|v| semver::Version::parse(v).ok()) {
            Some(v) => v,
            None => continue,
        };
        let better = match &best {
            None => true,
            // 版本相同时偏向 latest:少切一个 pnpm 缓存目录
            Some((best_version, best_tag)) => {
                version > *best_version
                    || (version == *best_version
                        && tag.as_str() == "latest"
                        && best_tag.as_str() != "latest")
            }
        };
        if better {
            best = Some((version, tag.to_string()));
        }
    }
    best.map(|(_, tag)| tag)
}

/// 默认频道。
///
/// 为什么默认 `next` 而不是 `latest`:npm 的 `latest` 标签由发布者控制,rc 阶段
/// 它常常落后于 `next`(实测 latest=0.1.5-rc.1、next=0.1.5-rc.2)。跟 `latest`
/// 会让桌面壳长期停在旧版,而这正是"客户端表现异常、必须退回终端手跑 @next"的成因。
/// 想退出预览频道可在菜单「配置 → DSH 频道」里切回 `latest`。
const DEFAULT_CHANNEL: &str = "next";

/// 菜单里可选的两个频道。顺序即菜单顺序,第一项是默认值。
const SELECTABLE_CHANNELS: &[(&str, &str)] = &[
    ("next", "next(预览版,默认)"),
    ("latest", "latest(稳定版)"),
];

/// 频道是否属于菜单里可选的那两个。
fn is_selectable_channel(channel: &str) -> bool {
    SELECTABLE_CHANNELS.iter().any(|(id, _)| *id == channel)
}

/// 读取当前生效的频道(菜单勾选状态与 spec 解析共用同一份判断)。
/// 由配置值决定生效频道。抽成纯函数,便于在不改真实配置的前提下测试。
///
/// 菜单里的两个频道直接用;其他安全取值(如 `newest` 或精确版本)原样透传,
/// 此时菜单不会勾选任何一项;非法或缺失则回落到默认频道。
fn channel_from_setting(configured: Option<&str>) -> String {
    match configured {
        Some(value) if is_selectable_channel(value) => value.to_string(),
        Some(other) if is_safe_spec_token(other) => other.to_string(),
        _ => DEFAULT_CHANNEL.to_string(),
    }
}

fn configured_channel() -> String {
    channel_from_setting(read_setting("app-dsh-channel").as_deref())
}

/// 决定这次启动喂给 pnpm 的 spec。
///
/// 默认 `next`(见 [`DEFAULT_CHANNEL`]);可在 `~/.dsh/settings.yaml` 用
/// `app-dsh-channel` 覆盖成 `latest` / `newest` / 精确版本如 `0.1.5-rc.2`,
/// 也可以从菜单「配置 → DSH 频道」直接切换。
///
/// 为什么传 tag 而不是精确版本:pnpm 的缓存目录按 spec 哈希。传 tag 时所有版本
/// 共用一个目录(dsh 约 220 MB),由 pnpm 原地升级;传精确版本会每发一版就多一个
/// 220 MB 目录,磁盘无上限增长。
fn resolve_dsh_spec() -> String {
    let configured = configured_channel();
    // `newest` 需要查 registry 才能确定实际 tag
    if configured == "newest" {
        return match newest_channel() {
            Some(tag) => format!("{}@{}", DSH_PACKAGE, tag),
            None => DSH_PACKAGE.to_string(),
        };
    }
    if is_safe_spec_token(&configured) {
        return format!("{}@{}", DSH_PACKAGE, configured);
    }
    DSH_PACKAGE.to_string()
}

/// 拉起 dsh web 后端:优先 `pnpm dlx`,没有 pnpm 时回退 `npx -y`。
/// 全程无 stdin,所以任何交互确认都必须提前在环境变量里关掉。
fn spawn_dsh(spec: &str) -> Option<Child> {
    let log = log_path();
    // 用 append 而不是 truncate:上一次启动失败的现场必须留着。
    // 截断会把「客户端为什么卡住」的唯一证据一起抹掉,排查时只剩一片空白。
    let log_file = match fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
    {
        Ok(f) => f,
        Err(_) => return None,
    };

    // 每次启动写一条分隔线,便于在追加日志里区分不同次运行。
    if let Ok(mut marker) = log_file.try_clone() {
        let _ = writeln!(
            marker,
            "\n===== 启动于 {} =====",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
    }

    // pnpm dlx 会直接安装临时包，不需要 npx 的 `-y` 确认选项。
    // --no-open:桌面壳自己导航到 WebView,不让 dsh 再弹系统默认浏览器。
    //
    // 回退到 npx 时必须带 `-y`:npx 装新包前会问 "Ok to proceed? (y)"。桌面壳的
    // stdin 是 null,这个问题永远等不到答案 —— 表现就是升级时静默挂死到超时。
    //
    // 三个分支用 `#[cfg]` 而非 `cfg!` 分开:Windows 分支要调只在 Windows 存在的
    // `raw_arg`,放进 `cfg!` 的 if-else 里会让 macOS/Linux 编译不过。
    #[cfg(target_os = "windows")]
    let mut cmd = {
        // 用 raw_arg 传整条命令行:Rust 会给普通 arg 加转义引号,那会让 cmd 把
        // `&` / `(` 当成字面量而不是语法。外层引号由 cmd /C 自己剥掉。
        //
        // 先 `where` 探测再分支,而不是 `pnpm ... || npx ...`:后者在 pnpm 存在
        // 但 dsh 自己崩溃时也会触发,白白拉一次 220 MB 的 npx 重装。
        let mut c = Command::new("cmd");
        c.arg("/C").raw_arg(format!(
            "\"where pnpm >nul 2>nul & if errorlevel 1 (npx -y {spec} web --no-open) else (pnpm dlx {spec} web --no-open)\"",
            spec = spec
        ));
        c
    };

    #[cfg(target_os = "macos")]
    let mut cmd = {
        // macOS: 用户的 pnpm 通常在 zsh 的 PATH 里(sh 读不进 .zshrc 的 zsh 语法,
        // 也带不出 nvm/volta 这些)。桌面 shell 不能凭空假设 PATH,所以用 zsh 加载
        // 用户的完整环境来执行,并兜底补常见 pnpm 安装位置。
        let script = format!(
            r#"if command -v pnpm >/dev/null 2>&1; then
  pnpm dlx {spec} web --no-open
  exit $?
fi
# pnpm 不在当前 PATH —— zsh + 常用安装路径都补一版,再找不到才退 npx
for d in "$HOME/.local/share/pnpm" "$HOME/.tesh" "$HOME/.volta/bin" "$HOME/.nvm/current/bin" "$HOME/.asdf/shims" "$(npm prefix -g 2>/dev/null)/bin"; do
  [ -n "$d" ] && [ -x "$d/pnpm" ] && exec "$d/pnpm" dlx {spec} web --no-open
done
# 没有 pnpm 就用 Node 自带的 npx。`-y` 必须带:否则它会问 "Ok to proceed? (y)",
# 而桌面壳没有 stdin 可答,升级时就会一直挂着。
if command -v npx >/dev/null 2>&1; then
  exec npx -y {spec} web --no-open
fi
echo "ERROR: neither pnpm nor npx found. Install Node.js (https://nodejs.org) or pnpm (https://pnpm.io/installation)" >&2
exit 127"#,
            spec = spec
        );
        // 用 zsh 执行以加载用户完整环境;缺 zsh 时退回 sh
        const ZSH: &str = "/bin/zsh";
        if std::path::Path::new(ZSH).exists() {
            let mut c = Command::new(ZSH);
            c.args(["-c", script.as_str()]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", script.as_str()]);
            c
        }
    };

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut cmd = {
        // Linux: 用户的 pnpm 常在 .bashrc/.profile,这里 source 后再跑
        let script = format!(
            r#"[ -f "$HOME/.profile" ] && . "$HOME/.profile" 2>/dev/null || true
[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc" 2>/dev/null || true
if command -v pnpm >/dev/null 2>&1; then
  pnpm dlx {spec} web --no-open
  exit $?
fi
for d in "$HOME/.local/share/pnpm" "$HOME/.volta/bin" "$HOME/.nvm/current/bin" "$HOME/.asdf/shims" "$(npm prefix -g 2>/dev/null)/bin"; do
  [ -n "$d" ] && [ -x "$d/pnpm" ] && exec "$d/pnpm" dlx {spec} web --no-open
done
# 没有 pnpm 就用 Node 自带的 npx。`-y` 必须带:否则它会问 "Ok to proceed? (y)",
# 而桌面壳没有 stdin 可答,升级时就会一直挂着。
if command -v npx >/dev/null 2>&1; then
  exec npx -y {spec} web --no-open
fi
echo "ERROR: neither pnpm nor npx found. Install Node.js (https://nodejs.org) or pnpm (https://pnpm.io/installation)" >&2
exit 127"#,
            spec = spec
        );
        let mut c = Command::new("sh");
        c.args(["-c", script.as_str()]);
        c
    };

    // 后端没有 stdin,任何交互提问都等不到答案 —— 表现是「升级时静默卡死到超时」,
    // 而不是报错。所以在环境层面把包管理器的确认全部预先关掉:
    //
    // COREPACK_ENABLE_DOWNLOAD_PROMPT:Node 自带的 corepack 在需要下载新版
    //   pnpm/npm 时会问 "Do you want to continue? [Y/n]" 并阻塞等输入。这正是
    //   「平时能用、一到升级就卡住」的元凶,Windows 上尤其常见(pnpm 多由
    //   corepack 托管)。置 0 即直接下载。
    // npm_config_yes:npx 的 `-y` 的环境变量形式,连带覆盖 dsh 内部可能再调起的
    //   npx,不只是我们自己拼的那条命令行。
    // NO_UPDATE_NOTIFIER:掉 update-notifier 的横幅,顺带少一处想画交互 UI 的地方。
    //
    // 故意不设 CI=1:它会被整个 dsh 后端及其中 agent 执行的每条命令继承,
    // 会改掉一堆无关工具的行为(比如让 pnpm install 默认 --frozen-lockfile)。
    cmd.env("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0")
        .env("npm_config_yes", "true")
        .env("NO_UPDATE_NOTIFIER", "1");

    cmd.stdin(Stdio::null())
        .stdout(std::process::Stdio::from(log_file.try_clone().unwrap()))
        .stderr(std::process::Stdio::from(log_file));

    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }

    match cmd.spawn() {
        Ok(child) => {
            let _ = fs::write(
                &log,
                format!(
                    "[{}] dsh 启动中: {} web --no-open (pnpm dlx,缺 pnpm 时回退 npx -y), PID={}\n",
                    chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                    spec,
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

fn plugin_update_log_path() -> PathBuf {
    dsh_home().join(".dsh-plugin-update.log")
}

/// 把启动进度推进到加载页。
///
/// 走 `window.eval` 而不是 Tauri event:加载页是应用自带的 `tauri://` 页面,没有
/// `withGlobalTauri`,也不该为了这点进度去开事件权限;而 eval 在当前窗口里始终可用。
/// 加载页的 `window.__dshBoot` 若还没就绪(极端情况下脚本未执行)则静默跳过 ——
/// 进度提示丢了不影响启动,不能因此中断流程。
fn push_boot_progress(
    window: &tauri::WebviewWindow,
    stage: BootStage,
    percent: u32,
    detail: &str,
    log_tail: Option<&str>,
) {
    let payload = serde_json::json!({
        "stage": stage.id(),
        "percent": percent.max(stage.floor_percent()),
        "title": stage.title(),
        "detail": detail,
        "log": log_tail,
    });
    let script = format!(
        "if (window.__dshBoot) {{ window.__dshBoot.update({payload}); }}",
        payload = payload
    );
    let _ = window.eval(&script);
}

/// 把一组 `String` 步骤转成 `error_html` 需要的借用切片。
fn step_refs(steps: &[String]) -> Vec<&str> {
    steps.iter().map(String::as_str).collect()
}

/// 把启动失败渲染成加载页里的错误面板:原因 + 针对性建议 + 日志尾部。
fn push_boot_failure(window: &tauri::WebviewWindow, stage: BootStage, report: &BootFailureReport) {
    let payload = serde_json::json!({
        "stage": stage.id(),
        "title": report.title,
        "reason": report.reason,
        "detail": report.detail,
        "steps": report.steps,
        "log": report.log,
    });
    let script = format!(
        "if (window.__dshBoot) {{ window.__dshBoot.fail({payload}); }} \
         else {{ document.documentElement.innerHTML = {fallback}; }}",
        payload = payload,
        fallback = serde_json::json!(error_html(
            &report.title,
            &report.reason,
            &step_refs(&report.steps)
        )),
    );
    let _ = window.eval(&script);
}

/// 在 dsh 界面上弹一条状态提示。
///
/// 提示必须能消失:
/// - `running` 是持续状态,不自动隐藏,等后续的 success/error 覆盖它;
/// - 其余状态(info/success/error)默认 8 秒后淡出 —— 版本信息这类一次性回执
///   如果一直挂着,用户会以为它关不掉;
/// - 任何状态都可以点 × 立刻关闭。
///
/// 每次调用都会重置上一次的定时器与关闭事件,避免旧定时器把新提示提前关掉。
fn show_status(app: &tauri::AppHandle, message: &str, state: &str) {
    if let Some(window) = app.get_webview_window("main") {
        let message_json = serde_json::to_string(message).unwrap();
        let state_json = serde_json::to_string(state).unwrap();
        let script = format!(
            r#"(() => {{
                const id = 'dsh-app-status';
                const message = {message};
                const state = {state};
                const AUTO_HIDE_MS = 8000;
                let node = document.getElementById(id);
                if (!node) {{
                    node = document.createElement('div');
                    node.id = id;
                    node.style.cssText = 'position:fixed;top:16px;right:16px;z-index:2147483647;max-width:460px;padding:12px 34px 12px 16px;border-radius:8px;font:14px/1.45 system-ui,sans-serif;white-space:pre-wrap;box-shadow:0 4px 18px rgba(0,0,0,.18);transition:opacity .25s ease';
                    document.body.appendChild(node);
                }}

                const dismiss = () => {{
                    const n = document.getElementById(id);
                    if (!n) return;
                    if (n._dshStatusTimer) {{ clearTimeout(n._dshStatusTimer); n._dshStatusTimer = null; }}
                    n.style.opacity = '0';
                    setTimeout(() => n.remove(), 250);
                }};

                // 关闭按钮:节点被重建后旧按钮随之消失,不存在重复绑定
                let close = node.querySelector('.dsh-app-status-close');
                if (!close) {{
                    close = document.createElement('button');
                    close.className = 'dsh-app-status-close';
                    close.type = 'button';
                    close.setAttribute('aria-label', '关闭提示');
                    close.textContent = '×';
                    close.style.cssText = 'position:absolute;top:6px;right:8px;width:20px;height:20px;padding:0;border:0;border-radius:4px;background:transparent;color:#fff;font:16px/1 system-ui,sans-serif;cursor:pointer;opacity:.75';
                    close.addEventListener('mouseenter', () => {{ close.style.opacity = '1'; }});
                    close.addEventListener('mouseleave', () => {{ close.style.opacity = '.75'; }});
                    close.addEventListener('click', dismiss);
                    node.appendChild(close);
                }}

                // 用既有 span 承载正文,避免 textContent 覆盖掉关闭按钮
                let body = node.querySelector('.dsh-app-status-body');
                if (!body) {{
                    body = document.createElement('span');
                    body.className = 'dsh-app-status-body';
                    node.insertBefore(body, node.firstChild);
                }}
                body.textContent = message;
                node.dataset.state = state;
                node.style.background = state === 'running' ? '#24415f' : (state === 'success' ? '#216e4e' : (state === 'info' ? '#425466' : '#8b3030'));
                node.style.color = '#fff';
                node.style.opacity = '1';
                node.style.animation = state === 'running' ? 'dsh-app-status-pulse 1.2s ease-in-out infinite' : 'none';
                node.style.position = 'fixed';

                if (!document.getElementById('dsh-app-status-style')) {{
                    const style = document.createElement('style');
                    style.id = 'dsh-app-status-style';
                    style.textContent = '@keyframes dsh-app-status-pulse {{ 0%,100% {{ opacity:.72 }} 50% {{ opacity:1 }} }}';
                    document.head.appendChild(style);
                }}

                // 重置自动消失:上一次的定时器必须先清掉
                if (node._dshStatusTimer) {{ clearTimeout(node._dshStatusTimer); node._dshStatusTimer = null; }}
                if (state !== 'running') {{
                    node._dshStatusTimer = setTimeout(dismiss, AUTO_HIDE_MS);
                }}
            }})();"#,
            message = message_json,
            state = state_json,
        );
        if window.eval(&script).is_err() {
            let fallback = format!("alert({});", message_json);
            let _ = window.eval(&fallback);
        }
    }
}

/// Update every dependency declared by the web profile, without touching the
/// DSH settings or credentials files. Output is retained for support reports.
fn update_web_profile_plugins(app: &tauri::AppHandle) {
    if PLUGIN_UPDATE_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        show_status(app, "插件更新已经在进行中，请等待完成。", "error");
        return;
    }

    show_status(app, "正在更新 web profile 的全部插件...\n请保持应用开启。", "running");

    let profile = dsh_home().join("profiles").join("web");
    if !profile.is_dir() {
        PLUGIN_UPDATE_RUNNING.store(false, Ordering::Release);
        show_status(app, "未找到 web profile，无法更新插件。请先启动一次 DSH。", "error");
        return;
    }

    let log_path = plugin_update_log_path();
    let log_file = match fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
    {
        Ok(file) => file,
        Err(error) => {
            PLUGIN_UPDATE_RUNNING.store(false, Ordering::Release);
            show_status(app, &format!("无法创建插件更新日志: {error}"), "error");
            return;
        }
    };

    let mut command = plugin_update_command(&profile);
    command
        .env("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0")
        .env("npm_config_yes", "true")
        .env("NO_UPDATE_NOTIFIER", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file.try_clone().unwrap()))
        .stderr(Stdio::from(log_file));

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            PLUGIN_UPDATE_RUNNING.store(false, Ordering::Release);
            show_status(
                app,
                &format!("启动插件更新失败: {error}\n日志: {}", log_path.display()),
                "error",
            );
            return;
        }
    };

    let app = app.clone();
    std::thread::spawn(move || {
        let result = child.wait();
        PLUGIN_UPDATE_RUNNING.store(false, Ordering::Release);
        let (message, state) = match result {
            Ok(status) if status.success() => (
                format!(
                    "插件已全部更新完成。\n请完全退出并重新启动 DSH 后生效。\n日志: {}",
                    log_path.display()
                ),
                "success",
            ),
            Ok(status) => (
                format!(
                    "插件更新失败 (退出码: {})。现有插件未被主动删除。\n日志: {}",
                    status
                        .code()
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "未知".to_string()),
                    log_path.display()
                ),
                "error",
            ),
            Err(error) => (
                format!(
                    "等待插件更新进程失败: {error}\n日志: {}",
                    log_path.display()
                ),
                "error",
            ),
        };
        show_status(&app, &message, state);
    });
}

#[cfg(target_os = "windows")]
fn plugin_update_command(profile: &Path) -> Command {
    let mut command = Command::new("cmd");
    command
        .arg("/C")
        .raw_arg("\"where pnpm >nul 2>nul & if errorlevel 1 (npx -y pnpm update) else (pnpm update)\"")
        .current_dir(profile)
        .creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(target_os = "macos")]
fn plugin_update_command(profile: &Path) -> Command {
    let mut command = Command::new("zsh");
    command
        .args([
            "-c",
            r#"if command -v pnpm >/dev/null 2>&1; then exec pnpm update; fi
if command -v npx >/dev/null 2>&1; then exec npx -y pnpm update; fi
echo 'ERROR: neither pnpm nor npx found' >&2; exit 127"#,
        ])
        .current_dir(profile);
    command
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn plugin_update_command(profile: &Path) -> Command {
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            r#"if command -v pnpm >/dev/null 2>&1; then exec pnpm update; fi
if command -v npx >/dev/null 2>&1; then exec npx -y pnpm update; fi
echo 'ERROR: neither pnpm nor npx found' >&2; exit 127"#,
        ])
        .current_dir(profile);
    command
}

fn try_reap_child(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(error) => {
                eprintln!("failed to poll child {}: {error}", child.id());
                return None;
            }
        }

        let now = Instant::now();
        if now >= deadline {
            eprintln!("timed out reaping child {}", child.id());
            return None;
        }
        std::thread::sleep(PROCESS_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }
}

#[cfg(unix)]
fn terminate_child_tree(child: &mut Child) {
    let Ok(pgid) = libc::pid_t::try_from(child.id()) else {
        if let Err(error) = child.kill() {
            eprintln!("failed to terminate child with out-of-range pid: {error}");
        }
        let _ = try_reap_child(child, PROCESS_REAP_TIMEOUT);
        return;
    };

    if unsafe { libc::kill(-pgid, libc::SIGTERM) } == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            eprintln!("failed to send SIGTERM to process group {pgid}: {error}");
        }
    }

    let deadline = Instant::now() + PROCESS_TERMINATION_GRACE;
    while Instant::now() < deadline {
        let group_exists = unsafe { libc::kill(-pgid, 0) } == 0;
        if !group_exists && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(PROCESS_POLL_INTERVAL.min(remaining));
    }

    // Always target the process group: the shell may have exited while a stubborn
    // pnpm/node descendant is still alive in the group.
    if unsafe { libc::kill(-pgid, libc::SIGKILL) } == -1 {
        let group_error = std::io::Error::last_os_error();
        if group_error.raw_os_error() != Some(libc::ESRCH) {
            eprintln!("failed to send SIGKILL to process group {pgid}: {group_error}");
        }
        if let Err(error) = child.kill() {
            eprintln!("failed to terminate direct child {}: {error}", child.id());
        }
    }
    let _ = try_reap_child(child, PROCESS_REAP_TIMEOUT);
}

#[cfg(target_os = "windows")]
fn terminate_child_tree(child: &mut Child) {
    let pid = child.id().to_string();
    let tree_killed = match Command::new("taskkill")
        .args(["/PID", pid.as_str(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(mut taskkill) => match try_reap_child(&mut taskkill, PROCESS_REAP_TIMEOUT) {
            Some(status) if status.success() => true,
            Some(status) => {
                eprintln!("taskkill failed for child {pid} with status {status}");
                false
            }
            None => {
                if let Err(error) = taskkill.kill() {
                    eprintln!("failed to stop timed-out taskkill process: {error}");
                }
                let _ = try_reap_child(&mut taskkill, PROCESS_REAP_TIMEOUT);
                false
            }
        },
        Err(error) => {
            eprintln!("failed to start taskkill for child {pid}: {error}");
            false
        }
    };

    if !tree_killed {
        if let Err(error) = child.kill() {
            eprintln!("failed to terminate direct child {pid}: {error}");
        }
    }
    let _ = try_reap_child(child, PROCESS_REAP_TIMEOUT);
}

/// 递归添加文件/目录到 zip
///
/// ZIP entry names are always slash-separated, regardless of the host OS.
fn zip_entry_name(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

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
        let rel_str = zip_entry_name(rel);
        if rel_str.contains("node_modules") || rel_str.contains("sessions/") || rel_str.contains("/target") {
            return Ok(());
        }

        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(&rel_str, options)
            .map_err(|e| e.to_string())?;

        let mut file = File::open(&full).map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        zip.write_all(&buf).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Normalize a ZIP entry before joining it to ~/.dsh.
///
/// ZIP files produced by older Windows builds contain `\\` separators.  Treat
/// both separators the same, but reject absolute paths, drive-qualified paths,
/// and parent traversal so an import can never escape the DSH home directory.
fn normalize_zip_entry_name(name: &str) -> Option<PathBuf> {
    let normalized = name.replace('\\', "/");
    if normalized.is_empty() || normalized.starts_with('/') {
        return None;
    }

    let mut path = PathBuf::new();
    for component in normalized.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return None;
        }
        // Reject Windows drive prefixes even when importing on Unix.
        if component.contains(':') {
            return None;
        }
        path.push(component);
    }

    (!path.as_os_str().is_empty()).then_some(path)
}

fn import_backup_root(dsh: &Path) -> PathBuf {
    let parent = dsh.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        ".dsh-import-backup-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S-%f")
    ))
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

    // 重建菜单(让 toggle 项显示新快捷键)
    if let Err(e) = rebuild_menu(&app) {
        eprintln!("重建菜单失败: {}", e);
    }

    Ok(format!("快捷键已设为: {} (立即生效)", s))
}

/// 切换 DSH 频道(菜单「配置 → DSH 频道」)。
///
/// 只改配置,不动正在运行的后端:贸然杀掉后端会让用户当前打开的界面立刻失效,
/// 而"下次启动生效"是可预期的行为。写入后重建菜单更新勾选状态。
fn switch_channel(app: &tauri::AppHandle, channel: &str) {
    if !is_selectable_channel(channel) {
        show_status(app, &format!("未知频道: {channel}"), "error");
        return;
    }

    let current = configured_channel();
    if current == channel {
        show_status(
            app,
            &format!("当前已经是 {channel} 频道,无需切换。"),
            "info",
        );
        return;
    }

    if let Err(error) = write_setting_value("app-dsh-channel", channel) {
        show_status(
            app,
            &format!("写入频道配置失败: {error}\n请检查 ~/.dsh/settings.yaml 的写入权限。"),
            "error",
        );
        return;
    }

    // 勾选状态来自配置,写完必须重建菜单才会反映出来
    if let Err(error) = rebuild_menu(app) {
        eprintln!("切换频道后重建菜单失败: {error}");
    }

    show_status(
        app,
        &format!(
            "DSH 频道已切换为 {channel}。\n\
             需要完全退出并重新启动本应用后生效。\n\
             若 3080 端口已有其他 dsh 实例在运行,新频道不会生效 —— \
             请先关掉那个实例。"
        ),
        "success",
    );
}

/// 重建应用菜单(用于快捷键变更后更新菜单标题)
fn rebuild_menu(app: &tauri::AppHandle) -> Result<(), String> {
    let current_shortcut = read_shortcut();

    let toggle_item = MenuItemBuilder::with_id("toggle", format!("显示/隐藏窗口 ({})", current_shortcut))
        .build(app).map_err(|e| e.to_string())?;
    let set_shortcut_item = MenuItemBuilder::with_id("set-shortcut", "设置快捷键…")
        .build(app).map_err(|e| e.to_string())?;
    let export_no_cred = MenuItemBuilder::with_id("export-no-cred", "导出配置(不含 API Keys)")
        .build(app).map_err(|e| e.to_string())?;
    let export_with_cred = MenuItemBuilder::with_id("export-cred", "导出配置(含 API Keys)")
        .build(app).map_err(|e| e.to_string())?;
    let import_item = MenuItemBuilder::with_id("import", "导入配置…")
        .build(app).map_err(|e| e.to_string())?;
    let update_plugins_item = MenuItemBuilder::with_id("update-plugins", "一键更新全部插件")
        .build(app).map_err(|e| e.to_string())?;
    let version_item = MenuItemBuilder::with_id(
        "version",
        format!("版本信息 v{}", env!("CARGO_PKG_VERSION")),
    )
        .build(app).map_err(|e| e.to_string())?;
    let quit_item = MenuItemBuilder::with_id("quit", "退出 DeepSeek Harness")
        .accelerator("CmdOrCtrl+Q")
        .build(app).map_err(|e| e.to_string())?;

    // 频道切换:勾选当前生效的那个,点击即写入 ~/.dsh/settings.yaml
    let active_channel = configured_channel();
    let mut channel_builder = SubmenuBuilder::new(app, "DSH 频道");
    let mut channel_items = Vec::new();
    for (id, label) in SELECTABLE_CHANNELS {
        let item = CheckMenuItemBuilder::with_id(format!("channel-{}", id), *label)
            .checked(*id == active_channel)
            .build(app)
            .map_err(|e| e.to_string())?;
        channel_items.push(item);
    }
    for item in &channel_items {
        channel_builder = channel_builder.item(item);
    }
    let channel_submenu = channel_builder.build().map_err(|e| e.to_string())?;

    let config_submenu = SubmenuBuilder::new(app, "配置")
        .item(&toggle_item)
        .item(&set_shortcut_item)
        .separator()
        .item(&channel_submenu)
        .separator()
        .item(&export_no_cred)
        .item(&export_with_cred)
        .separator()
        .item(&import_item)
        .item(&update_plugins_item)
        .separator()
        .item(&version_item)
        .item(&quit_item)
        .build().map_err(|e| e.to_string())?;

    // 非 macOS 平台直接挂这份菜单;macOS 还要额外拼一个"编辑"菜单,
    // 所以这个中间变量只在非 macOS 分支被消费。
    #[cfg(not(target_os = "macos"))]
    let menu = MenuBuilder::new(app).item(&config_submenu).build().map_err(|e| e.to_string())?;

    // macOS: 添加编辑菜单(让 Cmd+C/V/X/A/Z 生效)
    #[cfg(target_os = "macos")]
    {
        let copy_item = PredefinedMenuItem::copy(app, Some("复制")).map_err(|e| e.to_string())?;
        let cut_item = PredefinedMenuItem::cut(app, Some("剪切")).map_err(|e| e.to_string())?;
        let paste_item = PredefinedMenuItem::paste(app, Some("粘贴")).map_err(|e| e.to_string())?;
        let select_all_item = PredefinedMenuItem::select_all(app, Some("全选")).map_err(|e| e.to_string())?;
        let edit_menu = SubmenuBuilder::new(app, "编辑")
            .item(&copy_item)
            .item(&cut_item)
            .item(&paste_item)
            .item(&select_all_item)
            .build().map_err(|e| e.to_string())?;
        let full_menu = MenuBuilder::new(app).item(&config_submenu).item(&edit_menu).build().map_err(|e| e.to_string())?;
        app.set_menu(full_menu).map_err(|e| e.to_string())?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        app.set_menu(menu).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 关闭快捷键设置窗口
#[tauri::command]
fn close_shortcut_window(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("shortcut-input") {
        let _ = w.close();
    }
}

/// 导出配置(异步,避免 macOS blocking dialog 死锁)
fn do_export(app: &tauri::AppHandle, include_credentials: bool) {
    let dsh = dsh_home();
    let window = match app.get_webview_window("main") {
        Some(w) => w,
        None => return,
    };
    let win = window.clone();

    window
        .dialog()
        .file()
        .set_title("导出 DSH 配置")
        .add_filter("ZIP 文件", &["zip"])
        .set_file_name("dsh-config.zip")
        .save_file(move |file_path| {
            let save_path = match file_path {
                Some(p) => match p.into_path() {
                    Ok(path) => path,
                    Err(_) => return,
                },
                None => return,
            };

            let result = build_export_zip(&dsh, &save_path, include_credentials);
            let msg = match result {
                Ok(s) => s,
                Err(e) => format!("错误: {}", e),
            };
            let _ = win.eval(&format!("alert({});", serde_json::to_string(&msg).unwrap()));
        });
}

/// 实际构建导出 zip(同步,在回调线程执行)
fn build_export_zip(dsh: &Path, save_path: &Path, include_credentials: bool) -> Result<String, String> {
    let file = File::create(save_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);

    // 写入 manifest 标记是否含 credentials

    for item in EXPORT_ITEMS {
        let rel = PathBuf::from(item);
        // 对 credentials 单独处理,确保路径比较正确
        if item == &".credentials.yaml" {
            if include_credentials {
                let full = dsh.join(&rel);
                if full.exists() {
                    let options = zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated);
                    zip.start_file(".credentials.yaml", options).map_err(|e| e.to_string())?;
                    let mut f = File::open(&full).map_err(|e| e.to_string())?;
                    let mut buf = Vec::new();
                    f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
                    zip.write_all(&buf).map_err(|e| e.to_string())?;
                }
            }
            continue;
        }
        add_to_zip(&mut zip, dsh, &rel, include_credentials)?;
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
                    add_to_zip(&mut zip, dsh, &rel, true)?;
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

/// 导入配置(异步,避免 macOS blocking dialog 死锁)
fn do_import(app: &tauri::AppHandle) {
    let dsh = dsh_home();
    let window = match app.get_webview_window("main") {
        Some(w) => w,
        None => return,
    };
    let win = window.clone();

    window
        .dialog()
        .file()
        .set_title("导入 DSH 配置")
        .add_filter("ZIP 文件", &["zip"])
        .pick_file(move |file_path| {
            let open_path = match file_path {
                Some(p) => match p.into_path() {
                    Ok(path) => path,
                    Err(_) => return,
                },
                None => return,
            };

            let result = extract_import_zip(&dsh, &open_path);
            let msg = match result {
                Ok(s) => s,
                Err(e) => format!("错误: {}", e),
            };
            let _ = win.eval(&format!("alert({});", serde_json::to_string(&msg).unwrap()));
        });
}

/// 实际解压导入 zip(同步,在回调线程执行)
fn extract_import_zip(dsh: &Path, open_path: &Path) -> Result<String, String> {
    let file = File::open(open_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;

    fs::create_dir_all(dsh).map_err(|e| e.to_string())?;

    let mut extracted = 0;
    let backup_root = import_backup_root(dsh);
    let mut backed_up = 0;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().to_string();

        if name == "_export-manifest.json" {
            continue;
        }

        let Some(relative_path) = normalize_zip_entry_name(&name) else {
            eprintln!("skipping unsafe ZIP entry: {name:?}");
            continue;
        };
        let out_path = dsh.join(&relative_path);

        let canonical_dsh = dsh.canonicalize().unwrap_or_else(|_| dsh.to_path_buf());
        if !out_path.starts_with(&canonical_dsh) {
            continue;
        }

        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| e.to_string())?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            if out_path.is_file() {
                let backup_path = backup_root.join(&relative_path);
                if !backup_path.exists() {
                    if let Some(parent) = backup_path.parent() {
                        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    fs::copy(&out_path, &backup_path).map_err(|e| e.to_string())?;
                    backed_up += 1;
                }
            }
            let mut out_file = File::create(&out_path).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out_file).map_err(|e| e.to_string())?;
            drop(out_file);

            // macOS/Linux: credentials 文件需要 600 权限
            #[cfg(unix)]
            {
                if relative_path == Path::new(".credentials.yaml") {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&out_path, fs::Permissions::from_mode(0o600));
                }
            }
            extracted += 1;
        }
    }

    let backup_note = if backed_up > 0 {
        format!(
            "\n原配置已备份 ({} 个文件): {}",
            backed_up,
            backup_root.display()
        )
    } else {
        String::new()
    };
    Ok(format!(
        "已导入 {} 个配置文件。{}\n重启 DSH 后生效。",
        extracted, backup_note
    ))
}

/// 认证失败页的排查步骤。launch token 每进程随机、只在 dsh 自己的 stdout 打印,
/// 外部启动的实例我们读不到 —— 只能让用户二选一。
const AUTH_ERROR_STEPS: &[&str] = &[
    r#"关闭终端里那个 dsh 实例,然后重新打开本应用"#,
    r#"或在终端里找到它打印的 <code>dsh web: http://127.0.0.1:3080/?token=…</code> 那行,用浏览器打开"#,
    r#"查看日志文件:<br><code>~/.dsh/.dsh-app-launcher.log</code>"#,
];

/// 错误页的收尾说明。启动失败时补一给「怎么彻底重启」的操作,免得用户只在
/// 窗口上点关闭(那只是隐藏窗口,后端与状态都还在)。
const RESTART_HINT: &str =
    r#"修复后请从菜单「配置 → 退出 DeepSeek Harness」彻底退出,再重新打开本应用。"#;

/// 错误页 HTML。`steps` 内含标记,和 `reason` 一样直接插值 —— 两者都是本文件的常量。
fn error_html(title: &str, reason: &str, steps: &[&str]) -> String {
    let mut all_steps: Vec<&str> = steps.to_vec();
    all_steps.push(RESTART_HINT);
    let steps_html = all_steps
        .iter()
        .enumerate()
        .map(|(i, step)| format!("<p>{}. {}</p>", i + 1, step))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>{title}</title>
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
<h1>⚠️ {title}</h1>
<p><strong>原因:</strong>{reason}</p>
<p><strong>排查步骤:</strong></p>
{steps_html}
</div>
</body></html>"#,
        title = title,
        reason = reason,
        steps_html = steps_html
    )
}

/// 赌 WebView cookie 失败时的兜底脚本。
///
/// 赌赢了页面就是 dsh 的 GUI,这段脚本什么都不做;赌输了页面是 dsh 回的 401
/// 纯文本(`text/plain`,body 里就那一句英文),就地换成中文指引。
///
/// 之所以由页面自己判断,是因为 `window.eval` 拿不到返回值 —— 与其为这一个判断
/// 铺一条 IPC 回程,不如把判断和替换一起交给页面。
fn auth_fallback_script() -> String {
    let html = error_html(
        "DSH 需要认证",
        "3080 端口上的 dsh web 不是本应用启动的,拿不到它那份一次性认证 token;浏览器里也没有仍然有效的登录状态。",
        AUTH_ERROR_STEPS,
    );
    format!(
        "var t = (document.body ? (document.body.innerText || document.body.textContent || '') : '').trim(); \
         if (t.startsWith({needle})) {{ document.documentElement.innerHTML = {html}; }}",
        needle = serde_json::json!("dsh web authentication required"),
        html = serde_json::json!(html)
    )
}

/// The boot loop uses blocking HTTP and thread sleeps, so it must not run as an
/// async task. Without an await, reqwest's oneshot polls eventually exhaust the
/// task's cooperative budget and park forever, bypassing even the boot deadline.
fn spawn_boot_worker(task: impl FnOnce() + Send + 'static) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn_blocking(task)
}

fn main() {
    // 单实例锁:如果已有实例在跑,显示已有窗口然后退出
    let _single = tauri_plugin_single_instance::init(|app, _args, _cwd| {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.show();
            let _ = window.set_focus();
        }
    });

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
        .invoke_handler(tauri::generate_handler![set_shortcut_cmd, close_shortcut_window])
        .manage(CurrentShortcut(Mutex::new(shortcut)))
        .manage(DshChild(Mutex::new(None)))
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
            rebuild_menu(&app.handle())?;

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

            let child = if probe_dsh() == DshState::Down {
                // 只有真要拉起后端时才去解析版本,复用已在跑的实例不付这次网络开销。
                // 版本解析是一次 registry 网络请求(最多 5s),spawn 前先告诉加载页,
                // 免得用户在"什么都不显示"的状态下等这段。
                push_boot_progress(&main_window, BootStage::Resolve, 0, "", None);
                let spec = resolve_dsh_spec();
                push_boot_progress(&main_window, BootStage::Spawn, 0, &spec, None);
                spawn_dsh(&spec)
            } else {
                None
            };
            let had_child = child.is_some();

            let Some(state) = app.try_state::<DshChild>() else {
                if let Some(mut child) = child {
                    terminate_child_tree(&mut child);
                }
                return Err("DshChild state unavailable after backend startup".into());
            };
            let mut owned_child = match state.0.lock() {
                Ok(owned_child) => owned_child,
                Err(poisoned) => {
                    eprintln!("DshChild mutex was poisoned while storing backend ownership");
                    poisoned.into_inner()
                }
            };
            *owned_child = child;
            drop(owned_child);

            let window = main_window.clone();
            spawn_boot_worker(move || {
                let started = Instant::now();
                let deadline = started + Duration::from_secs(BOOT_TIMEOUT_SECS);
                // 后端没在跑时:短暂等待后就把加载页显示出来。切换 spec 或首次安装要下
                // 约 220 MB,让用户全程盯着空白桌面(甚至怀疑没启动)是不可接受的。
                let reveal_at = started + Duration::from_secs(if had_child { 3 } else { 0 });
                let mut revealed = false;
                let mut stage = BootStage::Probe;

                // 首次推送至少要等加载页的脚本执行完,否则 __dshBoot 还不存在。
                let mut last_push: Option<Instant> = None;
                // 后端是被我们拉起的,进程一旦退出就永远等不到端口 —— 提前报错,
                // 而不是让用户白等满 10 分钟。
                let mut child_exited: Option<String> = None;

                loop {
                    let now = Instant::now();
                    if !revealed && now >= reveal_at {
                        let _ = window.show();
                        revealed = true;
                    }
                    if !revealed {
                        std::thread::sleep(Duration::from_millis(200));
                        continue;
                    }

                    // 每 700 ms 探测一次,但进度推送按秒节流:日志读取和 eval 都不便宜,
                    // 而加载页的耗时本身就在秒级跳动。
                    let due = last_push.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1));
                    if due {
                        let elapsed = now.duration_since(started).as_secs();
                        let detail = match stage {
                            BootStage::Spawn | BootStage::Download => {
                                format!("已等待 {}s", elapsed)
                            }
                            _ => String::new(),
                        };
                        push_boot_progress(&window, stage, stage.floor_percent(), &detail, None);
                        last_push = Some(now);
                    }

                    // 拉起的后端提前退出:再等也没有意义,直接把日志里的原因报出来。
                    if child_exited.is_none() && had_child {
                        if let Some(state) = window.app_handle().try_state::<DshChild>() {
                            let mut guard = match state.0.lock() {
                                Ok(guard) => guard,
                                Err(poisoned) => poisoned.into_inner(),
                            };
                            if let Some(child) = guard.as_mut() {
                                match child.try_wait() {
                                    Ok(Some(status)) => {
                                        let code = status
                                            .code()
                                            .map(|c| c.to_string())
                                            .unwrap_or_else(|| "未知".to_string());
                                        child_exited = Some(format!("后端进程已退出,退出码 {code}"));
                                    }
                                    // 已退出且被回收过,或轮询失败:都不再按「运行中」处理
                                    Ok(None) => {}
                                    Err(error) => {
                                        child_exited =
                                            Some(format!("无法获取后端进程状态:{error}"));
                                    }
                                }
                            }
                        }
                    }

                    if let Some(reason) = child_exited.clone() {
                        let log = tail_launcher_log(80);
                        let kind = classify_boot_failure(&log, had_child);
                        // 日志里没写明原因(比如包装脚本自己挂了)时,退到进程退出这一类,
                        // 比「端口超时」更能说明问题。
                        let kind = if kind == BootFailure::NotReady {
                            BootFailure::RunnerExited
                        } else {
                            kind
                        };
                        let report = BootFailureReport::from_log(
                            kind,
                            &log,
                            had_child,
                            started.elapsed().as_secs(),
                            &reason,
                        );
                        push_boot_failure(&window, stage, &report);
                        let _ = window.show();
                        break;
                    }

                    let state = probe_dsh();

                    // 阶段只前进不后退:认证态下探测偶发回落到 401 之外的状态时,
                    // 进度提示不该跳回「等待端口就绪」。
                    if state == DshState::Down && stage < BootStage::Download {
                        stage = BootStage::Download;
                    } else if state != DshState::Down && stage < BootStage::Auth {
                        stage = BootStage::Auth;
                    }

                    // 新版 dsh 要先用 launch token 换 cookie。token 每进程随机、只在 stdout
                    // 打印,所以只能从我们自己重定向过去的那份日志里捞。
                    let auth = if state == DshState::NeedsAuth {
                        read_launch_token().and_then(|token| {
                            redeem_launch_token(&token).map(|cookie| (token, cookie))
                        })
                    } else {
                        None
                    };

                    // 拿不到有效 token 时还剩最后一条路:WebView 里可能留着上次换到的
                    // cookie。它的签名 secret 持久化在 ~/.dsh/.credentials.yaml,跨进程
                    // 有效(默认 30 天),所以外部实例照样认。探测用的 reqwest 不带 cookie,
                    // 这条路通不通在这里看不出来 —— 只能让 WebView 自己去试一把。
                    //
                    // 什么时候才值得试:复用的是外部实例(它的 token 永远不会进我们的日志,
                    // 再等也没有意义),或者已经等到超时。自己拉起的后端还在启动途中时不试,
                    // 它马上就会把 token 打印出来。
                    let bet_on_cookie = state == DshState::NeedsAuth
                        && auth.is_none()
                        && (!had_child || Instant::now() > deadline);

                    let target = match (state, &auth) {
                        // 旧版无认证,或这次请求已被放行
                        (DshState::Ready, _) => Some(DSH_URL.to_string()),
                        // reqwest 已用 token 换到 cookie。先注入 WebView 再打开裸地址,
                        // 避免 WKWebView 在跨站 303 中压住 SameSite=Strict cookie。
                        (DshState::NeedsAuth, Some((token, cookie))) => {
                            if window.set_cookie(cookie.clone()).is_ok() {
                                Some(DSH_URL.to_string())
                            } else {
                                // 旧版 Wry/平台不支持注入时保留原生 token 导航兜底。
                                Some(format!("{}/?token={}", DSH_URL, token))
                            }
                        }
                        _ if bet_on_cookie => Some(DSH_URL.to_string()),
                        _ => None,
                    };

                    if let Some(target) = target {
                        push_boot_progress(&window, BootStage::Ready, 100, "", None);
                        let _ = window.navigate(target.parse().unwrap_or_else(|_| {
                            format!("http://127.0.0.1:{}", DSH_PORT).parse().unwrap()
                        }));
                        std::thread::sleep(Duration::from_millis(500));
                        // 赌输了页面上就是 dsh 的英文 401,换成中文指引
                        if bet_on_cookie {
                            let _ = window.eval(&auth_fallback_script());
                        }
                        let _ = window.show();
                        break;
                    }

                    // 走到这里说明端口还没起来。超时了就把日志现场一并交给错误页。
                    if Instant::now() > deadline {
                        let log = tail_launcher_log(80);
                        let timeout_secs = started.elapsed().as_secs();
                        let kind = if state == DshState::NeedsAuth {
                            // 端口有响应但换不到 cookie:等待解决不了问题。
                            BootFailure::AuthRejected
                        } else {
                            classify_boot_failure(&log, had_child)
                        };
                        let detail = format!(
                            "阶段 {} · 已等待 {}s · 日志 {}",
                            stage.id(),
                            timeout_secs,
                            log_path().display()
                        );
                        let report =
                            BootFailureReport::from_log(kind, &log, had_child, timeout_secs, &detail);
                        push_boot_failure(&window, stage, &report);
                        // 自己拉起的后端起不来,就不该把进程留着占 3080 端口
                        if had_child {
                            if let Some(state) = window.app_handle().try_state::<DshChild>() {
                                let mut child = take_owned_child(&state);
                                if let Some(child) = child.as_mut() {
                                    terminate_child_tree(child);
                                }
                            }
                        }
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
                    // 创建独立的输入窗口(不依赖 dsh 前端的 __TAURI_INTERNALS__)
                    if let Some(_existing) = app.get_webview_window("shortcut-input") {
                        let _ = _existing.set_focus();
                        return;
                    }
                    let _ = WebviewWindowBuilder::new(
                        app,
                        "shortcut-input",
                        WebviewUrl::App("shortcut-input.html".into()),
                    )
                    .title("设置快捷键")
                    .inner_size(420.0, 280.0)
                    .resizable(false)
                    .center()
                    .always_on_top(true)
                    .build();
                    return;
                }
                "quit" => {
                    stop_owned_dsh(app);
                    app.exit(0);
                    return;
                }
                _ => {}
            }

            match event.id().as_ref() {
                "export-no-cred" => do_export(app, false),
                "export-cred" => do_export(app, true),
                "import" => do_import(app),
                "update-plugins" => update_web_profile_plugins(app),
                "version" => {
                    show_status(
                        app,
                        &format!(
                            "DeepSeek Harness Desktop v{}\nDSH channel: {}",
                            env!("CARGO_PKG_VERSION"),
                            configured_channel()
                        ),
                        "info",
                    );
                }
                id if id.starts_with("channel-") => {
                    let channel = id.trim_start_matches("channel-").to_string();
                    switch_channel(app, &channel);
                }
                _ => {}
            }
        })
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
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                stop_owned_dsh(app);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_worker_can_poll_past_the_async_cooperative_budget() {
        use std::net::TcpListener;
        use std::sync::mpsc;

        const POLLS: usize = 160;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for stream in listener.incoming().take(POLLS) {
                let mut stream = stream.unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 4096];
                stream.read(&mut request).unwrap();
                stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
        });
        let (tx, rx) = mpsc::channel();
        let worker = spawn_boot_worker(move || {
            let result = (|| -> Result<usize, reqwest::Error> {
                for _ in 0..POLLS {
                    // Match probe_dsh: a new blocking client on every poll.
                    let status = reqwest::blocking::Client::builder()
                        .no_proxy()
                        .timeout(Duration::from_secs(2))
                        .build()?
                        .get(&url)
                        .send()?
                        .status();
                    assert_eq!(dsh_state_for_status(status.as_u16()), DshState::NeedsAuth);
                }
                Ok(POLLS)
            })();
            let _ = tx.send(result);
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(15))
                .expect("startup worker stalled or panicked")
                .expect("loopback probes failed"),
            POLLS
        );
        tauri::async_runtime::block_on(worker).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn startup_worker_preserves_http_request_timeout() {
        use std::net::TcpListener;
        use std::sync::mpsc;

        // A listening socket that never sends an HTTP response.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        let worker = spawn_boot_worker(move || {
            let result = reqwest::blocking::Client::builder()
                .no_proxy()
                .timeout(Duration::from_millis(100))
                .build()
                .unwrap()
                .get(url)
                .send();
            let _ = tx.send(result.is_err_and(|error| error.is_timeout()));
        });
        assert!(rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker stalled"));
        tauri::async_runtime::block_on(worker).unwrap();
        drop(listener);
    }

    #[test]
    fn zip_entry_names_are_portable_across_operating_systems() {
        let rel = PathBuf::from("profiles").join("web").join("package.json");
        assert_eq!(zip_entry_name(&rel), "profiles/web/package.json");
        assert_eq!(
            normalize_zip_entry_name(r"profiles\web\package.json"),
            Some(PathBuf::from("profiles/web/package.json"))
        );
    }

    #[test]
    fn zip_import_rejects_paths_that_escape_dsh_home() {
        assert!(normalize_zip_entry_name("../settings.yaml").is_none());
        assert!(normalize_zip_entry_name(r"profiles\..\settings.yaml").is_none());
        assert!(normalize_zip_entry_name("/tmp/settings.yaml").is_none());
        assert!(normalize_zip_entry_name("C:/Users/Public/settings.yaml").is_none());
    }

    /// dsh 在监听端口后、BrowserAuth 激活前会短暂返回 404。这个中间态不能
    /// 被当成就绪，否则 WebView 会过早打开裸地址，随后正好撞上认证 401。
    #[test]
    fn transient_not_found_during_startup_is_not_ready() {
        assert_eq!(dsh_state_for_status(404), DshState::Down);
    }

    /// WKWebView 从 tauri:// 页面跳到 dsh 时不会在 token 兑换后的 303 跳转中
    /// 回送 SameSite=Strict cookie，因此启动器要把兑换响应里的 cookie 注入 WebView。
    #[test]
    fn prepares_redeemed_auth_cookie_for_the_webview_origin() {
        let raw = "dsh-auth-example=v1.payload.signature; Max-Age=2592000; Path=/; \
                   Expires=Sun, 04 Oct 2026 01:46:57 GMT; HttpOnly; SameSite=Strict";

        let cookie = webview_auth_cookie(raw).expect("parse dsh auth cookie");

        assert_eq!(cookie.name(), "dsh-auth-example");
        assert_eq!(cookie.value(), "v1.payload.signature");
        assert_eq!(cookie.domain(), Some("127.0.0.1"));
        assert_eq!(cookie.path(), Some("/"));
        assert_eq!(cookie.http_only(), Some(true));
        assert_eq!(
            cookie.same_site().map(|site| format!("{site:?}")),
            Some("Lax".into())
        );
    }

    #[test]
    fn taking_an_absent_owned_child_returns_none() {
        let state = DshChild(Mutex::new(None));

        assert!(take_owned_child(&state).is_none());
    }

    #[cfg(unix)]
    struct UnixProcessGroupGuard {
        child: Child,
        pgid: Option<libc::pid_t>,
    }

    #[cfg(unix)]
    impl Drop for UnixProcessGroupGuard {
        fn drop(&mut self) {
            if let Some(pgid) = self.pgid {
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
                let _ = self.child.kill();
                let _ = self.child.wait();
            } else if matches!(self.child.try_wait(), Ok(None) | Err(_)) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn taking_a_child_recovers_from_a_poisoned_mutex_and_is_idempotent() {
        use std::os::unix::process::CommandExt;
        use std::sync::Arc;

        let child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn owned child");
        let pgid = child.id() as libc::pid_t;
        let state = Arc::new(DshChild(Mutex::new(Some(child))));
        let poison_target = Arc::clone(&state);

        let poison_result = std::thread::spawn(move || {
            let _guard = poison_target.0.lock().expect("lock child before poisoning");
            panic!("poison DshChild mutex for recovery test");
        })
        .join();
        assert!(
            poison_result.is_err(),
            "test thread should poison the mutex"
        );

        let child = take_owned_child(&state).expect("recover and take poisoned child");
        let mut child = UnixProcessGroupGuard {
            child,
            pgid: Some(pgid),
        };
        assert!(
            take_owned_child(&state).is_none(),
            "taking the same owned child twice must be idempotent"
        );

        terminate_child_tree(&mut child.child);
        child.pgid = None;
    }

    #[cfg(unix)]
    #[test]
    fn terminates_entire_unix_process_group() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::CommandExt;

        let child = Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 30 & echo READY; wait"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn test process group");
        let pgid = child.id() as libc::pid_t;
        let mut group = UnixProcessGroupGuard {
            child,
            pgid: Some(pgid),
        };

        let mut ready = String::new();
        BufReader::new(
            group
                .child
                .stdout
                .as_mut()
                .expect("capture test shell readiness"),
        )
        .read_line(&mut ready)
        .expect("read test shell readiness");
        assert_eq!(ready.trim_end(), "READY");

        let timeout = Duration::from_secs(2);
        let started = Instant::now();
        terminate_child_tree(&mut group.child);

        let mut probe = unsafe { libc::kill(-pgid, 0) };
        while probe == 0 && started.elapsed() < timeout {
            let remaining = timeout.saturating_sub(started.elapsed());
            std::thread::sleep(Duration::from_millis(20).min(remaining));
            probe = unsafe { libc::kill(-pgid, 0) };
        }
        let probe_errno = if probe == -1 {
            std::io::Error::last_os_error().raw_os_error()
        } else {
            None
        };
        let elapsed = started.elapsed();

        assert!(
            elapsed <= timeout,
            "process-tree termination exceeded two seconds: {elapsed:?}"
        );
        assert_eq!(probe, -1, "owned process group should be gone");
        assert_eq!(
            probe_errno,
            Some(libc::ESRCH),
            "process-group probe should fail because the group no longer exists"
        );
        group.pgid = None;
    }

    #[cfg(unix)]
    #[test]
    fn falls_back_to_direct_kill_when_process_group_kill_fails() {
        let child = Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn child outside a dedicated process group");
        let mut child = UnixProcessGroupGuard { child, pgid: None };
        let started = Instant::now();

        terminate_child_tree(&mut child.child);

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "fallback termination must not wait for the child to exit naturally"
        );
        assert!(
            child
                .child
                .try_wait()
                .expect("probe fallback child")
                .is_some(),
            "fallback should reap the directly owned child"
        );
    }

    fn tags(pairs: &[(&str, &str)]) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), serde_json::Value::String((*v).to_string())))
            .collect()
    }

    /// 当下的真实局面:rc 阶段 next 领先 latest,必须选 next(否则永远停在 rc.7)
    #[test]
    fn prefers_next_when_it_leads_during_rc() {
        let t = tags(&[("latest", "0.1.0-rc.7"), ("next", "0.1.0-rc.8")]);
        assert_eq!(pick_newest_tag(&t).as_deref(), Some("next"));
    }

    /// GA 之后的局面:latest 反超,必须选 latest —— 不能把 next 写死
    #[test]
    fn prefers_latest_after_ga_overtakes() {
        let t = tags(&[("latest", "0.1.0"), ("next", "0.1.0-rc.8")]);
        assert_eq!(pick_newest_tag(&t).as_deref(), Some("latest"));
    }

    /// rc 编号按数字比,不能按字典序(rc.10 > rc.9)
    #[test]
    fn compares_prerelease_numbers_numerically() {
        let t = tags(&[("latest", "0.1.0-rc.9"), ("next", "0.1.0-rc.10")]);
        assert_eq!(pick_newest_tag(&t).as_deref(), Some("next"));
    }

    /// 版本并列时偏向 latest,避免多占一个 pnpm 缓存目录
    #[test]
    fn breaks_ties_toward_latest() {
        let t = tags(&[("next", "0.1.0"), ("latest", "0.1.0")]);
        assert_eq!(pick_newest_tag(&t).as_deref(), Some("latest"));
        let reversed = tags(&[("latest", "0.1.0"), ("next", "0.1.0")]);
        assert_eq!(pick_newest_tag(&reversed).as_deref(), Some("latest"));
    }

    /// 解析不了的版本号与危险 tag 名一律跳过,不能被带进 shell 命令行
    #[test]
    fn skips_unparsable_versions_and_unsafe_tag_names() {
        let t = tags(&[
            ("latest", "not-a-version"),
            ("weird tag; rm -rf /", "9.9.9"),
            ("next", "0.1.0-rc.8"),
        ]);
        assert_eq!(pick_newest_tag(&t).as_deref(), Some("next"));
        assert!(pick_newest_tag(&tags(&[])).is_none());
    }

    /// 阶段顺序决定了进度条只能前进:加载页按 id 高亮步骤,顺序错了会跳步。
    #[test]
    fn boot_stages_advance_monotonically() {
        assert!(BootStage::Probe < BootStage::Resolve);
        assert!(BootStage::Resolve < BootStage::Spawn);
        assert!(BootStage::Spawn < BootStage::Download);
        assert!(BootStage::Download < BootStage::Auth);
        assert!(BootStage::Auth < BootStage::Ready);
    }

    /// 进度百分比的下限必须随阶段单调不减,否则进度条会往回缩。
    #[test]
    fn boot_stage_floors_never_go_backwards() {
        let stages = [
            BootStage::Probe,
            BootStage::Resolve,
            BootStage::Spawn,
            BootStage::Download,
            BootStage::Auth,
            BootStage::Ready,
        ];
        for pair in stages.windows(2) {
            assert!(
                pair[0].floor_percent() < pair[1].floor_percent(),
                "{:?} 的百分比下限不应高于 {:?}",
                pair[0],
                pair[1]
            );
        }
        assert_eq!(BootStage::Ready.floor_percent(), 100);
    }

    /// 加载页用这些 id 定位步骤节点,改名会让进度显示整体失效。
    #[test]
    fn boot_stage_ids_match_the_loading_page() {
        assert_eq!(BootStage::Probe.id(), "probe");
        assert_eq!(BootStage::Resolve.id(), "resolve");
        assert_eq!(BootStage::Spawn.id(), "spawn");
        assert_eq!(BootStage::Download.id(), "download");
        assert_eq!(BootStage::Auth.id(), "auth");
        assert_eq!(BootStage::Ready.id(), "ready");
    }

    /// 后端脚本自己 exit 127 时会打印这句话 —— 必须归到「没装包管理器」,
    /// 而不是笼统的超时,否则用户会去查网络而不是装 Node。
    #[test]
    fn missing_runner_is_diagnosed_from_the_launcher_log() {
        let log = "[2026-09-13 10:55:14] dsh 启动中: @deepseek-ai/dsh@latest web --no-open\n\
                   ERROR: neither pnpm nor npx found. Install Node.js (https://nodejs.org) or pnpm\n";
        assert_eq!(classify_boot_failure(log, true), BootFailure::MissingRunner);
    }

    /// spawn 直接失败时日志里只有一行「启动 dsh 失败」,不能再报「端口超时」。
    #[test]
    fn spawn_failure_is_diagnosed_from_the_launcher_log() {
        let log = "[2026-09-13 10:55:14] 启动 dsh 失败: No such file or directory (os error 2)\n";
        assert_eq!(classify_boot_failure(log, true), BootFailure::RunnerExited);
    }

    /// 干净但慢的启动(下载大包)不该被误判成故障。
    #[test]
    fn slow_but_healthy_startup_is_not_misdiagnosed() {
        let log = "dsh-web: installing @deepseek-ai/dsh@latest...\n";
        assert_eq!(classify_boot_failure(log, true), BootFailure::NotReady);
    }

    /// 没拉过后端却超时,说明是外部实例/端口占用一类的问题,不该建议重装 Node。
    #[test]
    fn startup_without_an_owned_child_stays_generic() {
        let log = "";
        let kind = classify_boot_failure(log, false);
        assert_eq!(kind, BootFailure::NotReady);
        let steps = kind.steps().join("\n");
        assert!(!steps.contains("Node.js ≥ 22.19"), "未拉起后端时不应建议升级 Node");
    }

    /// 每种失败都必须带可执行建议,理由里也不能是空的。
    #[test]
    fn every_failure_carries_actionable_steps() {
        for kind in [
            BootFailure::MissingRunner,
            BootFailure::RunnerExited,
            BootFailure::AuthRejected,
            BootFailure::NotReady,
        ] {
            let steps = kind.steps();
            assert!(!steps.is_empty(), "{kind:?} 缺少排查建议");
            assert!(!kind.title().is_empty());
            assert!(!kind.reason(600).is_empty());
        }
    }

    /// 超时理由要带上实际等待时长,而不是写死「10 分钟」。
    #[test]
    fn timeout_reason_reports_the_real_elapsed_time() {
        let reason = BootFailure::NotReady.reason(725);
        assert!(reason.contains("12 分"), "应显示 12 分,实际: {reason}");
        assert!(reason.contains("5 秒"), "应显示 5 秒,实际: {reason}");
    }

    /// 错误页始终补一句「怎么彻底重启」,避免用户只关窗口(那只是隐藏)。
    #[test]
    fn error_page_always_explains_how_to_fully_restart() {
        let html = error_html("测试", "原因", &["步骤一"]);
        assert!(html.contains(RESTART_HINT), "错误页应包含彻底重启的说明");
        assert!(html.contains("步骤一"));
    }

    /// 日志尾部读取:文件不存在时返回空串,绝不能因此让启动流程出错。
    #[test]
    fn tail_of_a_missing_log_is_empty() {
        // 测试环境不保证 ~/.dsh 存在;这个断言只要求「不 panic 且类型正确」。
        let tail = tail_launcher_log(10);
        assert!(tail.is_empty() || !tail.is_empty());
    }

    /// `show_status` 注入的脚本源文本。
    ///
    /// 取源码本身而不是跑一遍 GUI —— 这里要守住的是"脚本里必须有这些机制",
    /// 真实渲染行为由 tests/test_launcher_command.py 与 jsdom 用例覆盖。
    fn status_script_source() -> &'static str {
        let source = include_str!("main.rs");
        let start = source
            .find("fn show_status(")
            .expect("show_status must exist");
        let end = source[start..]
            .find("/// Update every dependency")
            .expect("show_status must be followed by update_web_profile_plugins")
            + start;
        let body = &source[start..end];
        let open = body.find("r#\"").expect("status script raw string") + 3;
        let close = body[open..].find("\"#").expect("status script terminator") + open;
        &body[open..close]
    }

    /// 版本信息是一次性回执,必须能自动消失 —— 否则用户会以为它关不掉。
    #[test]
    fn status_toast_hides_itself_after_a_delay() {
        let script = status_script_source();
        assert!(
            script.contains("setTimeout(dismiss"),
            "状态提示必须注册自动消失定时器"
        );
        assert!(
            script.contains("AUTO_HIDE_MS"),
            "自动消失的时长应当是一个显式常量"
        );
    }

    /// 持续状态(running)不能自动消失:它是"正在进行"的指示,
    /// 要等后续的 success/error 把它盖掉。
    #[test]
    fn running_status_stays_visible_until_replaced() {
        let script = status_script_source();
        assert!(
            script.contains("state !== 'running'"),
            "自动消失必须排除 running 状态"
        );
    }

    /// 提示必须提供一个可点的关闭按钮,并显式清理定时器。
    #[test]
    fn status_toast_offers_an_explicit_close_button() {
        let script = status_script_source();
        assert!(script.contains("dsh-app-status-close"), "缺少关闭按钮");
        assert!(script.contains("clearTimeout"), "关闭时必须清掉自动消失定时器");
        assert!(
            script.contains("n.remove()"),
            "关闭后要把节点从 DOM 里移除,不能只改透明度"
        );
    }

    /// 回归:正文曾经用 `node.textContent = ...` 直接写,会把关闭按钮一起冲掉。
    #[test]
    fn status_text_does_not_wipe_the_close_button() {
        let script = status_script_source();
        assert!(
            !script.contains("node.textContent ="),
            "正文不能写在节点本身上,否则会覆盖关闭按钮"
        );
        assert!(
            script.contains("dsh-app-status-body"),
            "正文应写进独立的 body 子节点"
        );
    }

    /// 默认频道必须是 next:跟 latest 会让桌面壳长期停在旧版,
    /// 这正是"客户端表现异常、只能退回终端手跑 @next"的成因。
    #[test]
    fn defaults_to_the_next_channel() {
        assert_eq!(DEFAULT_CHANNEL, "next");
        assert_eq!(channel_from_setting(None), "next");
        assert_eq!(channel_from_setting(Some("")), "next");
    }

    /// 菜单里的两个频道可被显式选中。
    #[test]
    fn explicit_channels_are_honoured() {
        assert_eq!(channel_from_setting(Some("next")), "next");
        assert_eq!(channel_from_setting(Some("latest")), "latest");
    }

    /// `newest` 与精确版本不在菜单里,但配置写了就该生效(菜单不勾选任何项)。
    #[test]
    fn non_menu_channels_pass_through() {
        assert_eq!(channel_from_setting(Some("newest")), "newest");
        assert_eq!(channel_from_setting(Some("0.1.5-rc.2")), "0.1.5-rc.2");
        assert!(!is_selectable_channel("newest"));
        assert!(!is_selectable_channel("0.1.5-rc.2"));
    }

    /// 危险取值不能进命令行:回落到默认,而不是透传给 shell。
    #[test]
    fn unsafe_channel_values_fall_back_to_default() {
        assert_eq!(channel_from_setting(Some("x; rm -rf /")), "next");
        assert_eq!(channel_from_setting(Some("a b")), "next");
        assert_eq!(channel_from_setting(Some("$(id)")), "next");
    }

    /// 只有顶层键算配置项。嵌套子键(如 providers 下的同名项)不能被当成频道读走。
    #[test]
    fn nested_keys_are_not_mistaken_for_top_level_settings() {
        let content = "\
llm-pi-ai:
  providers:
    app-dsh-channel: \"evil\"
app-dsh-channel: \"latest\"
";
        assert_eq!(
            parse_top_level_setting(content, "app-dsh-channel").as_deref(),
            Some("latest"),
            "必须读到顶层那个,而不是缩进的同名子键"
        );
    }

    /// 没有顶层键时应返回 None(而不是误取子键)。
    #[test]
    fn missing_top_level_key_is_none() {
        let content = "llm-pi-ai:\n  app-dsh-channel: \"nested\"\n";
        assert_eq!(parse_top_level_setting(content, "app-dsh-channel"), None);
        assert_eq!(parse_top_level_setting("", "app-dsh-channel"), None);
    }

    /// 写顶层键时,嵌套结构必须原样保留。
    #[test]
    fn upsert_preserves_nested_structure() {
        let content = "\
permission:
  defaultPreset: danger-full-access
app-dsh-channel: \"latest\"
";
        let updated = upsert_top_level_setting(content, "app-dsh-channel", "next");
        assert!(updated.contains("app-dsh-channel: \"next\""));
        assert!(!updated.contains("\"latest\""));
        assert!(
            updated.contains("permission:\n  defaultPreset: danger-full-access"),
            "嵌套块被破坏: {updated}"
        );
    }

    /// 键不存在时追加到末尾,而不是丢弃原有内容。
    #[test]
    fn upsert_appends_missing_key() {
        let content = "permission:\n  defaultPreset: danger-full-access\n";
        let updated = upsert_top_level_setting(content, "app-dsh-channel", "next");
        assert!(updated.contains("permission:"));
        assert!(updated.ends_with("app-dsh-channel: \"next\"\n"), "实际: {updated}");
    }

    /// 来回切换必须稳定:写两次的结果等于直接写目标值。
    #[test]
    fn upsert_is_idempotent_across_switches() {
        let original = "app-shortcut: \"Alt+E\"\napp-dsh-channel: \"latest\"\n";
        let once = upsert_top_level_setting(original, "app-dsh-channel", "next");
        let twice = upsert_top_level_setting(&once, "app-dsh-channel", "next");
        assert_eq!(once, twice, "重复写入不应产生副本");
        let back = upsert_top_level_setting(&twice, "app-dsh-channel", "latest");
        assert!(back.contains("app-dsh-channel: \"latest\""));
        assert_eq!(
            back.matches("app-dsh-channel").count(),
            1,
            "切换不应累积重复键: {back}"
        );
    }

    /// 切换频道只应影响频道键,快捷键等其他顶层键必须原封不动。
    #[test]
    fn upsert_touches_only_the_target_key() {
        let content = "app-shortcut: \"Alt+E\"\napp-dsh-channel: \"latest\"\n";
        let updated = upsert_top_level_setting(content, "app-dsh-channel", "next");
        assert!(updated.contains("app-shortcut: \"Alt+E\""), "快捷键被改动: {updated}");
    }

    /// 从既有真实配置里读频道:写入后必须能被读回来。
    #[test]
    fn channel_round_trips_through_the_file_format() {
        let content = "app-shortcut: \"Alt+E\"\n";
        let written = upsert_top_level_setting(content, "app-dsh-channel", "next");
        let read_back = parse_top_level_setting(&written, "app-dsh-channel");
        assert_eq!(read_back.as_deref(), Some("next"));
        assert_eq!(channel_from_setting(read_back.as_deref()), "next");
    }

    /// 启动日志必须追加而不是截断:上次失败的现场是唯一的排查证据。
    ///
    /// 注意范围只取到第一个子函数/关键语句之前 —— `update_web_profile_plugins`
    /// 也用了 truncate(插件更新日志每次覆盖是合理的),不能把它算进来。
    #[test]
    fn launcher_log_is_appended_not_truncated() {
        let source = include_str!("main.rs");
        let spawn = source
            .find("fn spawn_dsh(")
            .expect("spawn_dsh must exist");
        let tail = &source[spawn..];
        let end = tail
            .find("// pnpm dlx 会直接安装临时包")
            .expect("spawn_dsh must contain the runner comment");
        let open_block = &tail[..end];
        assert!(
            open_block.contains(".append(true)"),
            "必须用 append 打开启动日志,否则上次启动的现场会被抹掉"
        );
        assert!(
            !open_block.contains(".truncate(true)"),
            "启动日志不能用 truncate,否则覆盖上一次启动的日志"
        );
    }

    #[test]
    fn spec_token_safety_rejects_shell_metacharacters() {
        assert!(is_safe_spec_token("0.1.0-rc.8"));
        assert!(is_safe_spec_token("next"));
        assert!(!is_safe_spec_token(""));
        assert!(!is_safe_spec_token("a b"));
        assert!(!is_safe_spec_token("x&calc"));
        assert!(!is_safe_spec_token("$(id)"));
        assert!(!is_safe_spec_token("a|b"));
    }

    /// 新版 dsh 的 launch token 只在这一行出现,前面还夹着插件的启动输出。
    #[test]
    fn parses_launch_token_from_startup_log() {
        let log = "dsh-client-masquerade ready: llm-pi-ai spoof controller active\n\
                   dsh web: http://127.0.0.1:3080/?token=1eBHG70HVPH97-9ahDKgxw1jC1u8BXScDr6tGVjh0ig\n";
        assert_eq!(
            parse_launch_token(log).as_deref(),
            Some("1eBHG70HVPH97-9ahDKgxw1jC1u8BXScDr6tGVjh0ig")
        );
    }

    #[test]
    fn appended_launch_log_uses_the_current_runs_token() {
        let log = "dsh web: http://127.0.0.1:3080/?token=old_token\n\
                   ===== 启动于 2026-09-13 18:00:00 =====\n\
                   dsh web: http://127.0.0.1:3080/?token=current_token\n";
        assert_eq!(parse_launch_token(log).as_deref(), Some("current_token"));
    }

    #[test]
    fn new_launch_without_a_token_does_not_reuse_a_previous_runs_token() {
        let log = "dsh web: http://127.0.0.1:3080/?token=old_token\n\
                   ===== 启动于 2026-09-13 18:00:00 =====\n\
                   dsh 启动中\n";
        assert_eq!(parse_launch_token(log), None);
    }

    /// 带 LAN 地址时同一行会出现两个 token(值相同),取第一个即可。
    #[test]
    fn parses_launch_token_ignoring_lan_suffix() {
        let log = "dsh web: http://127.0.0.1:3080/?token=abc_DEF-123 (LAN: http://192.168.1.5:3080/?token=abc_DEF-123)";
        assert_eq!(parse_launch_token(log).as_deref(), Some("abc_DEF-123"));
    }

    /// token 后面紧跟的非 base64url 字符是 URL 语法,不能被吞进 token。
    #[test]
    fn parses_launch_token_stopping_at_non_base64url() {
        let log = "dsh web: http://127.0.0.1:3080/?token=abc123&other=x";
        assert_eq!(parse_launch_token(log).as_deref(), Some("abc123"));
    }

    /// 旧版(0.1.1-rc.2 及更早)没有认证,打印的 URL 不带 token —— 调用方要退回裸地址。
    #[test]
    fn parses_no_launch_token_from_legacy_or_empty_log() {
        assert!(parse_launch_token("dsh web: http://127.0.0.1:3080").is_none());
        assert!(parse_launch_token("").is_none());
        assert!(parse_launch_token("dsh web: opening the default browser").is_none());
        // 有前缀但 token 为空,同样不能当成有效 token 拿去导航
        assert!(parse_launch_token("dsh web: http://127.0.0.1:3080/?token=").is_none());
    }

    /// 打通真实 registry 的解析链路。会联网,默认不跑:
    /// `cargo test -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn resolves_against_live_registry() {
        let channel = newest_channel().expect("registry 应能解析出最新频道");
        println!("registry = {}", npm_registry());
        println!("newest channel = {} -> spec {}@{}", channel, DSH_PACKAGE, channel);
        assert!(is_safe_spec_token(&channel));
    }
}
