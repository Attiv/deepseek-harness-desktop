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

> 🔃 装好之后就**不用再回来下安装包**了:桌面壳会自己检查新版本并原地升级
> (见[自动更新](#自动更新))。手动下载始终可用,`.deb` / `.rpm` 安装的场景也走这条路。

## 自动更新

桌面壳内置两条更新通道,优先走官方通道,不可用时自动退化:

| 通道 | 触发 | 行为 |
|---|---|---|
| 官方通道 | 启动后自动检查(菜单可关)、菜单「配置 → 检查更新…」 | 读 Release 上的 `latest.json`,按本机 target 取「下载地址 + 签名」→ 下载 → **minisign 校验** → 原地替换 → 重启 |
| 兜底通道 | 官方通道不可用(典型:本机自建包没注入签名公钥) | 查 GitHub Release API,按系统/架构在资产名里挑出本机该下的包,只提示并打开下载页,不动本机文件 |

「本机该下哪个包」由 target 决定,不需要用户自己认:

| 本机 | `latest.json` 里的 key | 官方通道实际下载 |
|---|---|---|
| macOS Intel | `darwin-x86_64` | `*_x64.app.tar.gz` |
| macOS Apple Silicon | `darwin-aarch64` | `*_aarch64.app.tar.gz` |
| Windows | `windows-x86_64` | `*-setup.exe`(NSIS,被动模式带进度条) |
| Linux | `linux-x86_64` | `*.AppImage` |

`.deb` / `.rpm` 不走官方通道 —— Tauri 的更新器不接管包管理器,这类安装会落到兜底通道,
提示你去下载页取对应的 deb/rpm。

### 为什么必须配签名密钥

Tauri 的更新器**强制**校验更新包签名,验不过就拒绝安装 —— 这是为了防止有人往
`latest.json` 里塞一个自己编译的包。密钥是独立于 Apple / Windows 代码签名的一对
ed25519(Minisign)密钥,只需生成一次:

```bash
# 私钥务必备份:换钥意味着已经装过的用户再也收不到更新
npx tauri signer generate -w ~/.tauri/dsh-app.key
```

把私钥和公钥分别放到仓库(一次性):

```bash
gh secret set TAURI_SIGNING_PRIVATE_KEY < ~/.tauri/dsh-app.key
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD   # 生成密钥时设了密码才需要
gh variable set TAURI_UPDATER_PUBLIC_KEY < ~/.tauri/dsh-app.key.pub
```

- `TAURI_SIGNING_PRIVATE_KEY` 是 **secret** —— 只在 CI 里给产物签名,永不进仓库。
- `TAURI_UPDATER_PUBLIC_KEY` 是 **variable**(公钥不是秘密)—— CI 会把它注入
  `src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey`,应用靠它验签。

私钥**不可找回**:GitHub 的 secret 是只写存储,读不回来。`~/.tauri/dsh-app.key`
一旦丢失,现有用户就永久收不到更新 —— 换钥意味着所有已安装版本验签失败,只能逐个
引导手动重装。请把私钥文件备份到本机之外(密码管理器 / 私密仓库 / 加密归档)。

仓库里这两处的默认值是「不签名」(`createUpdaterArtifacts: false` + `pubkey: ""`)。
这不是随手留的:一旦 `createUpdaterArtifacts` 为 true 而环境里没有私钥,
连本地 `npx tauri build` 都会直接失败。发布流程负责同时注入公钥并打开这一项。

### 发布流程

推 tag 后由 GitHub Actions 分三段完成,**任何一段不齐都停在 draft**:

1. **create-release** —— 建一个 draft Release(草稿不会被 `releases/latest` 看见)。
   tag 如果已经发布过则直接失败,拒绝覆盖线上清单。
2. **build** —— 四个平台**串行**构建,逐个上传并把签名产物合并进同一个 `latest.json`。
   串行是必须的:`latest.json` 的生成方式是「下载已有的 → 合并本平台 → 重新上传」,
   并发跑会让清单里只剩最后一个平台。
3. **publish-release** —— 校验 `latest.json` 里 `darwin-x86_64` / `darwin-aarch64` /
   `windows-x86_64` / `linux-x86_64` 四个平台齐全、都带 `signature`、url 都是 https,
   通过后才把 draft 转正。

漏配上面两个 secret/variable 时,构建会在第一步守卫处立刻失败 —— 总比发出去一个
用户装不上的版本强。

## 特性

- 🖥️ **独立桌面窗口** — 基于 Tauri v2 + 系统原生 WebView,自带标题栏,像原生 App 一样
- ⚡ **轻量** — 编译产物约 3 MB(对比 Electron 动辄上百 MB)
- 🔄 **默认跟随 next 频道** — 避免长期落后于上游预览版；菜单「配置 → DSH 频道」可在 `next` / `latest` / `alpha` 之间切换。
  某个频道在上游发布缺件、装不上时,会按 `next → latest → alpha` 逐个换频道重试,并在加载页写明整条回退链
- 🚀 **智能启动** — 检测已有实例则直接复用,否则后台拉起(无黑框),轮询端口就绪后显示窗口
- 📊 **启动进度可见** — 加载页逐步显示「检查实例 → 解析版本 → 拉起后端 → 等待下载 → 完成认证 → 加载界面」,带实时耗时、进度条,失败时显示后端日志尾部,不再是一个沉默的转圈
- 🧯 **失败给结论也给建议** — 后端提前退出、缺少包管理器、认证失败、下载超时会被分别识别;错误页直接列出对应的排查步骤,而不是笼统地说"启动失败"
- 🧹 **按归属清理后端** — 真正退出时只终止本桌面应用启动的完整 `pnpm → node → dsh` 进程树,不影响其他终端任务或复用的外部服务
- 🔃 **应用内更新** — 启动后自动检查桌面壳新版本(可关),也可随时菜单「配置 → 检查更新…」;按本机系统与架构自动取对应安装包,校验签名后原地替换并重启。详见[自动更新](#自动更新)
- 🩹 **卡住能自己爬出来** — 菜单「配置 → 重新加载页面」(`Cmd+R`) 一键刷新界面;
  工作区弹窗卡在「正在删除工作区」**超过 45s 会自动刷新一次**(10 分钟冷却;
  刻意不误伤「正在生成回复」这类正常长任务)
- 🌐 **跨平台** — Windows / macOS / Linux 全支持

## 工作原理

```
双击 DeepSeek-Harness
   ↓
检测 127.0.0.1:3080 是否已有 dsh 在跑?
   ├─ 有 → 窗口直接导航到 dsh 界面(不查版本,零网络开销)
   └─ 无 → 使用 `next` 预览频道(默认;菜单可切 `latest` / `alpha`)
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
| 拉起后端 | 执行 `pnpm dlx` / `npx -y`(自动把 pnpm 与 node 的常见安装位置补进 PATH,Finder 双击也能起来) | 没装 Node.js 或 pnpm |
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
| 上游这个版本装不上 | 日志出现 `ERR_PNPM_NO_MATCHING_VERSION` | 壳已自动换过全部频道;见下方「上游发布缺件」 |

后端是被本应用拉起的,若它起不来,超时后会被**自动终止**,不会留着进程继续占端口。
错误页会显示实际等待时长、当时所处阶段、日志路径与日志尾部，并提供「查看后端日志」和「重新启动应用」按钮。

### 退出与关闭

- 从菜单选择「退出」、按 `Cmd+Q`(macOS)或 `Ctrl+Q`(Windows/Linux),以及通过 Dock/系统退出应用,都属于真正退出。此时应用会终止**仅由本桌面应用启动**的完整 `pnpm → node → dsh` 进程树。
- 普通关闭主窗口只会隐藏窗口,应用及其启动的后端继续运行,可通过快捷键重新唤回。
- 如果启动时在 `127.0.0.1:3080` 检测到可正常响应的已有 dsh/HTTP 服务,应用只会复用它；退出应用时不会终止这个外部服务。

### 跨电脑导入配置

从菜单「配置」导出 ZIP，在另一台电脑选择「导入配置…」。选择文件后会显示处理中提示，
完成后弹出原生成功或失败对话框；成功回执包含实际导入文件数、配置目录和被覆盖文件的备份目录。
空包或没有可导入文件的包会报告失败，不再把“导入 0 个文件”视为成功。

导出范围现在包含全局 `cordis.patch.yml` 和各 profile 的 `pnpm-lock.yaml`。
旧包没有包含的文件需在来源电脑使用修复版重新导出。配置 ZIP 不包含 `node_modules`；
新电脑缺少插件包时，先执行 `dsh plugin --profile web install`（其他 profile 替换 `web`），再重启。
本地路径依赖仍需要在新电脑上提供对应源码。

导入后请通过「配置 → 退出 DeepSeek Harness」彻底退出，再重新启动。仅关闭窗口、刷新页面或
重新打开插件页面不会重启后端。如果桌面应用复用了终端启动的 DSH，还需重启那个终端实例。

旧版导入存在原始路径与规范化路径混用的问题，Windows 的路径前缀及 macOS/Linux 的目录别名
可能导致所有文件被静默跳过。本次修复统一使用规范化配置根目录，并用原生回执代替页面 `alert()`。
这属于桌面壳修复，需要重新构建并安装桌面应用；仅更新 DSH 或插件不会更新桌面壳。

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
- 或手动在 `~/.dsh/.dsh-app-settings.yaml` 写 `app-dsh-channel: "latest"`

两种方式都需要**完全退出并重新启动**应用后生效。

### 上游发布缺件

dsh 是一个几十个子包的 monorepo,上游偶尔会「主包发了、某个子包没跟上」。这时
pnpm 会直接判定无解并秒退:

```
ERR_PNPM_NO_MATCHING_VERSION  No matching version found for
@deepseek-ai/dsh-client-ui-sidebar-documentpreview@^0.1.5-rc.3
```

**这不是本机的问题**——重装 Node、换镜像源、清缓存都没用,那个版本在 npm 上不存在。

更麻烦的是它**会一次拖垮多个频道**。2026-09-22 实测:

| 频道 | 指向版本 | 能否安装 |
|---|---|---|
| `next` | `0.1.5-rc.3` | ❌ 直接依赖那个从未发布的子包 |
| `latest` | `0.1.5-rc.2` | ❌ 依赖 `dsh-web-app: ^0.1.5-rc.2`,而 caret 允许同段更高的预发布版,于是向上浮到坏掉的 `0.1.5-rc.3` |
| `alpha` | `0.1.7-alpha.1` | ✅ |

所以壳不假设「总有一个频道是好的」:遇到这类错误会**顺着 `next → latest → alpha`
一直试到某个装得成为止**,并把整条回退链写在加载页与日志里。全部试完仍不行才报
「上游这个版本装不上」,那时错误页会给出查看 dist-tags 的命令。

想省掉每次启动都试一遍前面几个坏频道,可以把 `app-dsh-channel` 直接钉到当前能用的
那个(如 `alpha`),等上游修好再切回来。

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

本项目使用 GitHub Actions 自动构建和发布(流程详见[发布流程](#发布流程)):

- **打 tag 触发**:推送 `v*` 格式的 tag
  ```bash
  git tag v1.4.13
  git push --tags
  ```
- **手动重跑**:在 [Actions 页面](https://github.com/Attiv/deepseek-harness-desktop/actions)
  点 "Run workflow" 并填入**已存在**的版本 tag(形如 `v1.4.13`)。

前置条件:Actions 可读的两个凭据必须已经配好,否则构建会在守卫步骤直接失败 ——
`TAURI_SIGNING_PRIVATE_KEY`(secret)、`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`(secret,
设置私钥密码才需要)、`TAURI_UPDATER_PUBLIC_KEY`(variable)。配置命令见
[为什么必须配签名密钥](#为什么必须配签名密钥)。

构建矩阵(串行执行,保证 `latest.json` 合并正确):
- `windows-latest` → NSIS 安装程序 + MSI
- `macos-latest` (x86_64) → Intel dmg
- `macos-latest` (aarch64) → Apple Silicon dmg
- `ubuntu-22.04` → AppImage + deb + rpm

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

**推荐用菜单**:「配置 → DSH 频道」里勾选 `next`(预览版,默认)、`latest`(稳定版)
或 `alpha`(最新,迭代最激进),切换后提示需要完全退出并重启应用。

也可以直接编辑 `~/.dsh/.dsh-app-settings.yaml`(壳独占的设置文件,首次写入时自动创建)。

> 早期版本把这两个开关混在 dsh 自己的 `~/.dsh/settings.yaml` 里。dsh 从
> 0.1.7-alpha.1 起会把那个文件改名成 `settings.yaml.imported`(把 section 搬进当前
> profile),寄存在那里的设置会跟着一起消失 —— 所以壳改用自己独占的文件。
> 旧位置与 `.imported` 仍会被**读**到,老用户不会因此丢掉已设的频道与快捷键。

```yaml
# 文件位于 ~/.dsh/.dsh-app-settings.yaml
app-dsh-channel: "next"       # 默认:跟预览频道(通常比 latest 新)
# app-dsh-channel: "latest"   # 只跟官方稳定频道
# app-dsh-channel: "alpha"    # 跟版本号最高的频道(上游发得最快,也最容易缺件)
# app-dsh-channel: "newest"   # 自动选版本最高的频道(菜单里不显示,但配置可生效)
# app-dsh-channel: "0.1.5-rc.3" # 钉死某个版本(该版本必须真能装上)
```

菜单「配置 → 版本信息」可查看当前桌面壳版本与生效频道；「配置 → 一键更新全部插件」
会显示实时更新状态，完成后需完全重启 DSH。

### 关闭启动时自动检查更新

菜单 **配置 → 启动时自动检查更新** 取消勾选即可，等价于在 `~/.dsh/settings.yaml`
顶层写：

```yaml
app-auto-check-update: "off"
```

关掉之后仍可用「配置 → 检查更新…」手动检查。默认是开启的。

三种方式都需**完全退出并重新启动**应用才生效。若 3080 端口上已有其他 dsh 实例
在运行，应用只会复用它，此时新频道不会生效 —— 需要先停掉那个实例。

> 启动日志位于 `~/.dsh/.dsh-app-launcher.log`。日志是**追加**写入的（超过 1 MiB 才会
> 保留一代滚动备份为 `.log.1`），所以重启不会抹掉上次的启动现场，可以放心重启后再看。

### 其他

- 修改窗口大小/标题/行为:编辑 `src-tauri/src/main.rs`
- 修改端口/超时:编辑 `src-tauri/src/main.rs` 顶部的常量
- 修改图标:替换 `src-tauri/icons/` 下的文件,或运行 `npx tauri icon <path-to-png>`
- 启动日志:`~/.dsh/.dsh-app-launcher.log`(含本次实际使用的 spec)

## License

MIT
