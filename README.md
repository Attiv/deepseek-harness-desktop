# DeepSeek Harness Desktop

一个把 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 的 Web 界面包装成**独立桌面应用**的 Tauri 壳。

双击即可启动,不再需要手动执行 `pnpm dlx @deepseek-ai/dsh web`,也不会再被塞进浏览器标签页。

## 下载安装

前往 [Releases](https://github.com/Attiv/deepseek-harness-desktop/releases) 下载最新版本:

| 平台 | 下载文件 | 说明 |
|---|---|---|
| **Windows** | `*-setup.exe` 或 `*.msi` | 安装程序,双击运行 |
| **macOS (Apple Silicon)** | `*_aarch64.dmg` | M1/M2/M3 芯片 |
| **macOS (Intel)** | `*_x64.dmg` | Intel 芯片 |
| **Linux** | `*.AppImage` 或 `*.deb` | 直接运行或安装 |

> ⚠️ 运行时需要系统已安装 [Node.js](https://nodejs.org/) + [pnpm](https://pnpm.io/)(用于 `pnpm dlx` 拉起 dsh 后端)

## 特性

- 🖥️ **独立桌面窗口** — 基于 Tauri v2 + 系统原生 WebView,自带标题栏,像原生 App 一样
- ⚡ **轻量** — 编译产物约 3 MB(对比 Electron 动辄上百 MB)
- 🔄 **自动跟随最新版** — 启动时查 npm dist-tags,自动选**版本号最高的那个频道**再交给 `pnpm dlx`,无需用户确认任何东西。官方在 rc 阶段把新版发在 `next`(`latest` 会落后一截),裸 `npx @deepseek-ai/dsh` 拿不到,这里能拿到
- 🚀 **智能启动** — 检测已有实例则直接复用,否则后台拉起(无黑框),轮询端口就绪后显示窗口
- 🌐 **跨平台** — Windows / macOS / Linux 全支持

## 工作原理

```
双击 DeepSeek-Harness
   ↓
检测 127.0.0.1:3080 是否已有 dsh 在跑?
   ├─ 有 → 窗口直接导航到 dsh 界面(不查版本,零网络开销)
   └─ 无 → 查 registry 的 dist-tags,选出版本号最高的频道
            ↓
            后台执行 pnpm dlx @deepseek-ai/dsh@<频道> web --no-open(隐藏窗口)
            ↓
            立刻显示加载页,轮询端口直到就绪(上限 10 分钟)
            ↓
            窗口导航到 http://127.0.0.1:3080 并显示
```

关键点:**真正的 dsh 仍然由 `pnpm dlx` 拉起**,因此官方更新 dsh 时,桌面壳自动跟随升级,无需重新打包。

### 为什么要自己选频道

npm 的 `latest` 标签由发布者控制,rc 阶段它常常故意落后:

```
$ npm view @deepseek-ai/dsh dist-tags
{ "latest": "0.1.0-rc.7", "next": "0.1.0-rc.8" }
```

裸 `npx @deepseek-ai/dsh` 解析的是 `latest`,所以只会拿到 rc.7 —— 加 `@latest` 也一样。
壳启动时比较各个 tag 实际指向的版本号(按 semver,`rc.10 > rc.9`),挑最高的那个 tag 传给 pnpm dlx,
于是 rc 阶段跟到 `next`,GA 之后 `latest` 反超时又自动切回去,两个方向都成立。

### 为什么传 tag 而不是精确版本号

pnpm 的缓存目录按 **spec 字符串**哈希。传 tag(`@next`)时所有版本共用同一个目录,由 pnpm
原地升级;传精确版本(`@0.1.0-rc.8`)则每发一版就新建一个目录 —— dsh 一份装完约 **220 MB**,
几个版本就是 1 GB 垃圾。

### 为什么没有 `-y`

`-y` 是 `npx` 的确认选项，`pnpm dlx` 不支持它；pnpm 会直接安装并执行临时包，无需额外确认参数。

## 环境要求

| 平台 | 运行时依赖 | 说明 |
|---|---|---|
| Windows 10/11 | WebView2 Runtime | Win11 自带,Win10 通常已装 |
| macOS | 无额外依赖 | 使用系统自带 WKWebView |
| Linux | WebKitGTK | 主流发行版通常已装 |
| **所有平台** | **Node.js ≥ 22.19 + pnpm** | 用于 `pnpm dlx` 拉起 dsh 后端 |

> dsh 0.1.0-rc.8 的传递依赖 `@earendil-works/pi-ai` 声明 `node >= 22.19.0`。
> npm 默认不强制 engines,实测 Node 22.16 也能起来(只是 `npm warn EBADENGINE`),
> 但既然壳会自动跟最新版,把 Node 升到 22.19+ 才不会哪天被某个新 rc 咬到。

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

### 锁定 dsh 版本(可选)

默认自动跟最新,不需要配置。若某个新版有问题,在 `~/.dsh/settings.yaml` 加一行即可:

```yaml
app-dsh-channel: newest       # 默认:自动选版本最高的频道
# app-dsh-channel: latest     # 只跟官方稳定频道
# app-dsh-channel: next       # 只跟预览频道
# app-dsh-channel: 0.1.0-rc.7 # 钉死某个版本
```

改完重启 App 生效(需要先从菜单「退出」把后端一起关掉,单纯关窗口只是隐藏)。

### 其他

- 修改窗口大小/标题/行为:编辑 `src-tauri/src/main.rs`
- 修改端口/超时:编辑 `src-tauri/src/main.rs` 顶部的常量
- 修改图标:替换 `src-tauri/icons/` 下的文件,或运行 `npx tauri icon <path-to-png>`
- 启动日志:`~/.dsh/.dsh-app-launcher.log`(含本次实际使用的 spec)

## License

MIT
