# DeepSeek Harness Desktop

一个把 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 的 Web 界面包装成**独立桌面应用**的 Tauri 壳。

双击即可启动,不再需要手动执行 `npx @deepseek-ai/dsh web`,也不会再被塞进浏览器标签页。

## 下载安装

前往 [Releases](https://github.com/Attiv/deepseek-harness-desktop/releases) 下载最新版本:

| 平台 | 下载文件 | 说明 |
|---|---|---|
| **Windows** | `*-setup.exe` 或 `*.msi` | 安装程序,双击运行 |
| **macOS (Apple Silicon)** | `*_aarch64.dmg` | M1/M2/M3 芯片 |
| **macOS (Intel)** | `*_x64.dmg` | Intel 芯片 |
| **Linux** | `*.AppImage` 或 `*.deb` | 直接运行或安装 |

> ⚠️ 运行时需要系统已安装 [Node.js](https://nodejs.org/)(用于 `npx` 拉起 dsh 后端)

## 特性

- 🖥️ **独立桌面窗口** — 基于 Tauri v2 + 系统原生 WebView,自带标题栏,像原生 App 一样
- ⚡ **轻量** — 编译产物约 3 MB(对比 Electron 动辄上百 MB)
- 🔄 **自动跟随官方更新** — 后端仍走 `npx @deepseek-ai/dsh web`,官方发新版后下次启动自动拉取,壳本身无需改动
- 🚀 **智能启动** — 检测已有实例则直接复用,否则后台拉起(无黑框),轮询端口就绪后显示窗口
- 🌐 **跨平台** — Windows / macOS / Linux 全支持

## 工作原理

```
双击 DeepSeek-Harness
   ↓
检测 127.0.0.1:3080 是否已有 dsh 在跑?
   ├─ 有 → 窗口直接导航到 dsh 界面
   └─ 无 → 后台执行 npx @deepseek-ai/dsh web(隐藏窗口)
            ↓
            轮询端口直到就绪(最长 120s)
            ↓
            窗口导航到 http://127.0.0.1:3080 并显示
```

关键点:**真正的 dsh 仍然由 `npx` 拉起**,因此官方更新 dsh 时,桌面壳自动跟随升级,无需重新打包。

## 环境要求

| 平台 | 运行时依赖 | 说明 |
|---|---|---|
| Windows 10/11 | WebView2 Runtime | Win11 自带,Win10 通常已装 |
| macOS | 无额外依赖 | 使用系统自带 WKWebView |
| Linux | WebKitGTK | 主流发行版通常已装 |
| **所有平台** | **Node.js + npm** | 用于 `npx` 拉起 dsh 后端 |

## 本地构建

```bash
# 安装依赖
npm install

# 编译(首次会下载并编译 Rust crate,较慢)
npx tauri build
```

产物位于 `src-tauri/target/release/`。

### 构建前置要求

- [Rust](https://rustup.rs/) 工具链(stable)
- Node.js 20+
- Windows: MSVC Build Tools
- macOS: Xcode Command Line Tools
- Linux: `libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf`

## CI / 自动发布

本项目使用 GitHub Actions 自动构建和发布:

- **打 tag 触发**:推送 `v*` 格式的 tag 自动构建全平台并发布 Release
  ```bash
  git tag v1.0.0
  git push --tags
  ```
- **手动触发**:在 [Actions 页面](https://github.com/Attiv/deepseek-harness-desktop/actions) 点 "Run workflow"

构建矩阵:
- `windows-latest` → NSIS 安装程序 + MSI
- `macos-latest` (x86_64) → Intel dmg
- `macos-latest` (aarch64) → Apple Silicon dmg
- `ubuntu-22.04` → AppImage + deb

## 项目结构

```
dsh-app/
├── dist/                  # 前端资源(加载中占位页)
├── src-tauri/
│   ├── src/main.rs        # 核心逻辑:拉起 dsh、轮询端口、导航窗口
│   ├── icons/             # 应用图标(跨平台)
│   ├── Cargo.toml
│   └── tauri.conf.json
├── .github/workflows/     # CI 构建工作流
├── package.json
└── .gitignore
```

## 自定义

- 修改窗口大小/标题/行为:编辑 `src-tauri/src/main.rs`
- 修改端口/超时:编辑 `src-tauri/src/main.rs` 顶部的常量
- 修改图标:替换 `src-tauri/icons/` 下的文件,或运行 `npx tauri icon <path-to-png>`

## License

MIT
