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

> ⚠️ 运行时需要系统已安装 [Node.js](https://nodejs.org/)(自带 `npx`)。装了 [pnpm](https://pnpm.io/) 会优先使用 `pnpm dlx`,没装则自动回退到 `npx -y`。

## 特性

- 🖥️ **独立桌面窗口** — 基于 Tauri v2 + 系统原生 WebView,自带标题栏,像原生 App 一样
- ⚡ **轻量** — 编译产物约 3 MB(对比 Electron 动辄上百 MB)
- 🔄 **默认跟随 next 频道** — 避免长期落后于上游预览版；可在菜单「配置 → DSH 频道」一键切回 `latest`(稳定版)
- 🚀 **智能启动** — 检测已有实例则直接复用,否则后台拉起(无黑框),轮询端口就绪后显示窗口
- 📊 **启动进度可见** — 加载页逐步显示「检查实例 → 解析版本 → 拉起后端 → 等待下载 → 完成认证 → 加载界面」,带实时耗时、进度条,失败时显示后端日志尾部,不再是一个沉默的转圈
- 🧯 **失败给结论也给建议** — 后端提前退出、缺少包管理器、认证失败、下载超时会被分别识别;错误页直接列出对应的排查步骤,而不是笼统地说"启动失败"
- 🧹 **按归属清理后端** — 真正退出时只终止本桌面应用启动的完整 `pnpm → node → dsh` 进程树,不影响其他终端任务或复用的外部服务
- 🌐 **跨平台** — Windows / macOS / Linux 全支持

## 工作原理

```
双击 DeepSeek-Harness
   ↓
检测 127.0.0.1:3080 是否已有 dsh 在跑?
   ├─ 有 → 窗口直接导航到 dsh 界面(不查版本,零网络开销)
   └─ 无 → 使用 `next` 预览频道(默认;菜单可切 `latest`)
            ↓
            后台执行 pnpm dlx @deepseek-ai/dsh@<频道> web --no-open(隐藏窗口)
            ↓
            立刻显示加载页,逐步汇报启动阶段,
            轮询端口直到就绪(上限 10 分钟)
            ↓
            窗口导航到 http://127.0.0.1:3080 并显示
```

关键点:**真正的 dsh 仍然由 `pnpm dlx` 拉起**,因此官方更新 dsh 时,桌面壳自动跟随升级,无需重新打包。

### 启动过程中你会看到什么

加载页按阶段推进,每一步都会点亮并显示实时耗时;启动失败时,展开「启动日志」可以看到
后端输出的尾部内容。也可直接执行 `tail ~/.dsh/.dsh-app-launcher.log` 查看后端输出。

| 阶段 | 含义 | 卡在这里通常是因为 |
|---|---|---|
| 检查实例 | 探测 `127.0.0.1:3080` | —— |
| 解析版本 | 查询 registry 的 dist-tags | 网络受限(最多等 5s 即回退) |
| 拉起后端 | 执行 `pnpm dlx` / `npx -y` | 没装 Node.js 或 pnpm |
| 等待下载 | 包下载、校验或后端初始化尚未完成 | 这不是实际下载进度;持续等待时应查看后端日志 |
| 完成认证 | 用 launch token 换 cookie | 3080 被别的 dsh 实例占着 |
| 加载界面 | 导航到 dsh GUI | —— |

### 旧版为什么会停在「已等待 48s」

这是桌面壳的线程调度问题,不是下载确认框。旧版把同步 HTTP 轮询和 `thread::sleep`
放进没有任何 `await` 的异步任务中。重复轮询耗尽 Tokio 的协作调度预算后,
会卡在 HTTP 客户端初始化,无法返回外层循环,因此进度停更,10 分钟超时也不再执行。
后端是独立进程,仍可能正常启动,所以浏览器能打开但桌面一直停在加载页。

**v1.4.11** 将同步启动循环移到 `spawn_blocking` 工作线程,不再占用异步任务的调度预算。
回归测试覆盖连续 160 次本机 HTTP 探测与无响应请求的超时;48 秒只是当时环境下的表现,
不是固定的下载或超时阈值。

### 启动失败时

应用不会只留一个转圈,而是判定原因并给出针对性建议:

| 判定 | 触发条件 | 典型建议 |
|---|---|---|
| 找不到包管理器 | 后端脚本打印 `neither pnpm nor npx found` | 装 Node.js ≥ 22.19 或 pnpm |
| 后端进程启动失败 | 拉起的进程在监听端口前就退出 | 在终端手动跑一次 `pnpm dlx` 看报错 |
| DSH 认证失败 | 端口有响应但始终换不到 cookie | 关掉终端里占用 3080 的那个实例 |
| DSH 启动超时 | 等满 10 分钟仍未就绪 | 检查网络/镜像源,或钉死 `app-dsh-channel` |

后端是被本应用拉起的,若它起不来,超时后会被**自动终止**,不会留着进程继续占端口。
错误页会显示实际等待时长、当时所处阶段、日志路径与日志尾部,便于直接定位。

### 退出与关闭

- 从菜单选择「退出」、按 `Cmd+Q`(macOS)或 `Ctrl+Q`(Windows/Linux),以及通过 Dock/系统退出应用,都属于真正退出。此时应用会终止**仅由本桌面应用启动**的完整 `pnpm → node → dsh` 进程树。
- 普通关闭主窗口只会隐藏窗口,应用及其启动的后端继续运行,可通过快捷键重新唤回。
- 如果启动时在 `127.0.0.1:3080` 检测到可正常响应的已有 dsh/HTTP 服务,应用只会复用它；退出应用时不会终止这个外部服务。

### 为什么默认使用 next 频道

npm 的 `latest` 标签由发布者控制,rc 阶段它常常故意落后:

```
$ npm view @deepseek-ai/dsh dist-tags
{ "latest": "0.1.5-rc.1", "next": "0.1.5-rc.2" }
```

裸 `npx @deepseek-ai/dsh` 解析的是 `latest`,所以只会拿到 rc.1 —— 加 `@latest` 也一样。
跟在 `latest` 上意味着桌面壳长期落后一个版本,遇到上游已修的问题时会表现为
「客户端卡住、应用不工作」,只能退回终端手动跑 `@next` 绕开。

因此桌面壳**默认使用 `next`**。若某些第三方 profile 插件尚未适配预览版、
导致模型选择等 UI 槽位异常,可随时切回 `latest`:

- 菜单 **配置 → DSH 频道 → latest(稳定版)**
- 或手动在 `~/.dsh/settings.yaml` 写 `app-dsh-channel: "latest"`

两种方式都需要**完全退出并重新启动**应用后生效。

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
├── dist/                  # 前端资源:启动进度页 + 快捷键设置页
├── src-tauri/
│   ├── src/main.rs        # 核心逻辑:拉起 dsh、轮询端口、汇报进度、导航窗口
│   ├── icons/             # 应用图标(跨平台)
│   ├── Cargo.toml
│   └── tauri.conf.json
├── tests/                 # Python 回归测试(启动命令、加载页协议)
├── .github/workflows/     # CI 构建工作流
├── package.json
└── .gitignore
```

### 加载页与主进程的约定

加载页 `dist/index.html` 暴露 `window.__dshBoot`,主进程通过 `window.eval` 调用它
(不用 Tauri event,以免为这点进度额外开事件权限):

- `__dshBoot.update({ stage, percent, title, detail, log })` — 推进阶段
- `__dshBoot.fail({ title, reason, detail, steps, log })` — 进入失败态

`stage` 取值 `probe` / `resolve` / `spawn` / `download` / `auth` / `ready`,必须与
`main.rs` 里 `BootStage::id()` 返回的字符串一致 —— `tests/test_launcher_command.py`
会校验两边不会漂移。

## 测试

```bash
# Rust 单元测试(失败分类、阶段顺序、token 解析等)
cd src-tauri && cargo test

# Python 回归测试(启动命令行、加载页协议与渲染顺序)
python3 -m unittest discover -s tests
```

## 自定义

### 切换 DSH 频道

**推荐用菜单**:「配置 → DSH 频道」里勾选 `next`(预览版,默认)或 `latest`(稳定版),
切换后提示需要完全退出并重启应用。

也可以直接编辑 `~/.dsh/settings.yaml`:

```yaml
app-dsh-channel: "next"       # 默认:跟预览频道(通常比 latest 新)
# app-dsh-channel: "latest"   # 只跟官方稳定频道
# app-dsh-channel: "newest"   # 自动选版本最高的频道(菜单里不显示,但配置可生效)
# app-dsh-channel: "0.1.5-rc.2" # 钉死某个版本
```

菜单「配置 → 版本信息」可查看当前桌面壳版本与生效频道；「配置 → 一键更新全部插件」
会显示实时更新状态，完成后需完全重启 DSH。

三种方式都需**完全退出并重新启动**应用才生效。若 3080 端口上已有其他 dsh 实例
在运行，应用只会复用它，此时新频道不会生效 —— 需要先停掉那个实例。

> 启动日志位于 `~/.dsh/.dsh-app-launcher.log`。排查时请先备份日志，重启可能覆盖上次启动现场。

### 其他

- 修改窗口大小/标题/行为:编辑 `src-tauri/src/main.rs`
- 修改端口/超时:编辑 `src-tauri/src/main.rs` 顶部的常量
- 修改图标:替换 `src-tauri/icons/` 下的文件,或运行 `npx tauri icon <path-to-png>`
- 启动日志:`~/.dsh/.dsh-app-launcher.log`(含本次实际使用的 spec)

## License

MIT
