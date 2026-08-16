# DeepSeek Harness Desktop

一个把 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 的 Web 界面包装成**独立桌面应用**的 Tauri 壳。

双击即可启动,不再需要手动执行 `npx @deepseek-ai/dsh web`,也不会再被塞进浏览器标签页。

## 特性

- 🖥️ **独立桌面窗口** — 基于 Tauri v2 + WebView2,自带标题栏,像原生 App 一样
- ⚡ **轻量** — 编译产物仅约 3 MB(对比 Electron 动辄上百 MB)
- 🔄 **自动跟随官方更新** — 后端仍走 `npx @deepseek-ai/dsh web`,官方发新版后下次启动自动拉取,壳本身无需改动
- 🚀 **智能启动** — 检测已有实例则直接复用,否则后台拉起(无黑框),轮询端口就绪后显示窗口

## 工作原理

```
双击 DeepSeek-Harness.exe
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

- Windows 10/11
- [WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)(Win11 自带,Win10 通常已装)
- Node.js + npm(用于 `npx` 拉起 dsh)
- 构建时需要:Rust 工具链 + MSVC Build Tools

## 构建

```bash
# 安装依赖
npm install

# 编译(首次会下载并编译大量 Rust crate,较慢)
npx tauri build
```

产物位于 `src-tauri/target/release/`。

## 项目结构

```
dsh-app/
├── dist/                  # 前端资源(加载中占位页)
├── src-tauri/
│   ├── src/main.rs        # 核心逻辑:拉起 dsh、轮询端口、导航窗口
│   ├── icons/             # 应用图标
│   ├── Cargo.toml
│   └── tauri.conf.json
├── package.json
└── .gitignore
```

## 自定义

- 修改窗口大小/标题/行为:编辑 `src-tauri/src/main.rs`
- 修改端口/超时:编辑 `src-tauri/src/main.rs` 顶部的常量
- 修改图标:替换 `src-tauri/icons/` 下的文件

## License

MIT
