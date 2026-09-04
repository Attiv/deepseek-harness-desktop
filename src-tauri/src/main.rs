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
use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};
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

/// 从 ~/.dsh/settings.yaml 读一个顶层标量字段(与 write_shortcut 对称的简单行解析)。
/// 字段缺失、文件不存在、值为空都返回 None。
fn read_setting(key: &str) -> Option<String> {
    let content = fs::read_to_string(dsh_home().join("settings.yaml")).ok()?;
    let prefix = format!("{}:", key);
    for line in content.lines() {
        if let Some(rest) = line.trim().strip_prefix(&prefix) {
            let val = rest.trim().trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
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
    for line in log.lines() {
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

/// 决定这次启动喂给 pnpm 的 spec。
///
/// 默认使用官方稳定频道,避免预览版 dsh 与第三方 profile 插件不同步。
/// 可在 ~/.dsh/settings.yaml 覆盖: `app-dsh-channel: newest` | `latest`(默认) |
/// `next` | 精确版本如 `0.1.0-rc.7`。
///
/// 为什么传 tag 而不是精确版本:pnpm 的缓存目录按 spec 哈希。传 tag 时所有版本
/// 共用一个目录(dsh 约 220 MB),由 pnpm 原地升级;传精确版本会每发一版就多一个
/// 220 MB 目录,磁盘无上限增长。
fn resolve_dsh_spec() -> String {
    let configured_channel = read_setting("app-dsh-channel");
    if let Some(pin) = configured_channel.as_deref() {
        if pin != "newest" && is_safe_spec_token(pin) {
            return format!("{}@{}", DSH_PACKAGE, pin);
        }
    }
    let channel = if configured_channel.as_deref() == Some("newest") {
        newest_channel()
    } else {
        Some("latest".to_string())
    };
    match channel {
        Some(tag) => format!("{}@{}", DSH_PACKAGE, tag),
        // 解析失败(离线/私有源/网络受限)就退回裸 spec:它对应的 pnpm 缓存目录
        // 通常早就装好了,能离线秒起,而不是卡在一次注定失败的下载上
        None => DSH_PACKAGE.to_string(),
    }
}

/// 拉起 dsh web 后端:优先 `pnpm dlx`,没有 pnpm 时回退 `npx -y`。
/// 全程无 stdin,所以任何交互确认都必须提前在环境变量里关掉。
fn spawn_dsh(spec: &str) -> Option<Child> {
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

fn notify_plugin_update(app: &tauri::AppHandle, message: &str) {
    if let Some(window) = app.get_webview_window("main") {
        let script = format!("alert({});", serde_json::to_string(message).unwrap());
        let _ = window.eval(&script);
    }
}

/// Update every dependency declared by the web profile, without touching the
/// DSH settings or credentials files. Output is retained for support reports.
fn update_web_profile_plugins(app: &tauri::AppHandle) {
    if PLUGIN_UPDATE_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        notify_plugin_update(app, "插件更新已经在进行中，请等待完成。");
        return;
    }

    let profile = dsh_home().join("profiles").join("web");
    if !profile.is_dir() {
        PLUGIN_UPDATE_RUNNING.store(false, Ordering::Release);
        notify_plugin_update(app, "未找到 web profile，无法更新插件。请先启动一次 DSH。");
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
            notify_plugin_update(app, &format!("无法创建插件更新日志: {error}"));
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
            notify_plugin_update(
                app,
                &format!("启动插件更新失败: {error}\n日志: {}", log_path.display()),
            );
            return;
        }
    };

    let app = app.clone();
    std::thread::spawn(move || {
        let result = child.wait();
        PLUGIN_UPDATE_RUNNING.store(false, Ordering::Release);
        let message = match result {
            Ok(status) if status.success() => format!(
                "插件已全部更新完成。\n请完全退出并重新启动 DSH 后生效。\n日志: {}",
                log_path.display()
            ),
            Ok(status) => format!(
                "插件更新失败 (退出码: {})。现有插件未被主动删除。\n日志: {}",
                status
                    .code()
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "未知".to_string()),
                log_path.display()
            ),
            Err(error) => format!(
                "等待插件更新进程失败: {error}\n日志: {}",
                log_path.display()
            ),
        };
        notify_plugin_update(&app, &message);
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
    let quit_item = MenuItemBuilder::with_id("quit", "退出 DeepSeek Harness")
        .accelerator("CmdOrCtrl+Q")
        .build(app).map_err(|e| e.to_string())?;

    let config_submenu = SubmenuBuilder::new(app, "配置")
        .item(&toggle_item)
        .item(&set_shortcut_item)
        .separator()
        .item(&export_no_cred)
        .item(&export_with_cred)
        .separator()
        .item(&import_item)
        .item(&update_plugins_item)
        .separator()
        .item(&quit_item)
        .build().map_err(|e| e.to_string())?;

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

/// 启动失败页的排查步骤。
const BOOT_ERROR_STEPS: &[&str] = &[
    r#"确认已安装 <a href="https://nodejs.org" style="color:#58a6ff">Node.js</a>"#,
    r#"在终端手动执行测试:<br><code>pnpm dlx @deepseek-ai/dsh web</code>"#,
    r#"查看日志文件:<br><code>~/.dsh/.dsh-app-launcher.log</code>"#,
];

/// 认证失败页的排查步骤。launch token 每进程随机、只在 dsh 自己的 stdout 打印,
/// 外部启动的实例我们读不到 —— 只能让用户二选一。
const AUTH_ERROR_STEPS: &[&str] = &[
    r#"关闭终端里那个 dsh 实例,然后重新打开本应用"#,
    r#"或在终端里找到它打印的 <code>dsh web: http://127.0.0.1:3080/?token=…</code> 那行,用浏览器打开"#,
    r#"查看日志文件:<br><code>~/.dsh/.dsh-app-launcher.log</code>"#,
];

/// 错误页 HTML。`steps` 内含标记,和 `reason` 一样直接插值 —— 两者都是本文件的常量。
fn error_html(title: &str, reason: &str, steps: &[&str]) -> String {
    let steps_html = steps
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
                // 只有真要拉起后端时才去解析版本,复用已在跑的实例不付这次网络开销
                spawn_dsh(&resolve_dsh_spec())
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
            tauri::async_runtime::spawn(async move {
                let started = Instant::now();
                let deadline = started + Duration::from_secs(BOOT_TIMEOUT_SECS);
                // 后端没在跑时:短暂等待后就把加载页显示出来。切换 spec 或首次安装要下
                // 约 220 MB,让用户全程盯着空白桌面(甚至怀疑没启动)是不可接受的。
                let reveal_at = started + Duration::from_secs(if had_child { 3 } else { 0 });
                let mut revealed = false;

                loop {
                    if !revealed && Instant::now() >= reveal_at {
                        let _ = window.show();
                        revealed = true;
                    }

                    let state = probe_dsh();

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

                    // 走到这里说明端口还没起来。超时了就只剩报错。
                    if Instant::now() > deadline {
                        let reason = if had_child {
                            "启动器已拉起但 dsh web 长时间未就绪(超过 10 分钟)。可能是下载被网络卡住,或 dsh 启动报错 —— 请看日志。"
                        } else {
                            "无法启动包管理器进程。请确认已安装 Node.js(pnpm 可选)。"
                        };
                        let html = error_html("DSH 启动失败", reason, BOOT_ERROR_STEPS);
                        let _ = window.eval(&format!(
                            "document.documentElement.innerHTML = {};",
                            serde_json::json!(html)
                        ));
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
