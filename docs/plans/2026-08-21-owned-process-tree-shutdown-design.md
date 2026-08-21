# 应用退出时清理自有后端进程树：设计

## 背景

桌面壳通过 shell 启动 `pnpm dlx @deepseek-ai/dsh web --no-open`。当前菜单退出只在 Windows 上递归终止进程树；macOS/Linux 只调用 `Child::kill()` 终止直接子进程，可能留下 `pnpm`、`node` 或 dsh 后端。系统级退出路径（例如 macOS Dock 退出）也没有统一执行清理。

## 目标

- 菜单“退出”、`Cmd+Q`/`Ctrl+Q`、Dock 或系统退出均清理由本应用启动的完整后端进程树。
- 不影响用户在其他终端运行的 `node`、`npm`、`pnpm` 或 dsh。
- 普通关闭主窗口仍然只隐藏窗口。
- 启动时若复用已有的 3080 服务，退出时不终止该外部服务。

## 方案

### 进程所有权

继续使用 `DshChild(Mutex<Option<Child>>)` 表示所有权。只有桌面壳实际启动了后端时才保存 `Child`；复用已有服务时保持 `None`。清理函数通过 `take()` 原子取走句柄，因此多条退出路径重复触发时也只会清理一次。

### Unix（macOS/Linux）

启动 shell 时将其放进新的独立进程组，后续的 `pnpm`、`node` 和 dsh 默认继承该进程组。退出时向负的进程组 ID 发送 `SIGTERM`，给后端短暂的优雅退出时间，再发送 `SIGKILL` 兜底并回收直接子进程。

使用独立进程组而不是按进程名查杀，确保清理范围严格限定在本应用创建的进程树。

### Windows

启动时同时设置 `CREATE_NO_WINDOW` 和 `CREATE_NEW_PROCESS_GROUP`。退出继续使用 `taskkill /PID <pid> /T /F`，由 `/T` 递归终止该 PID 的所有子孙进程。

### 生命周期接入

抽取统一的 `stop_owned_dsh(&AppHandle)`：

1. 从 `DshChild` 中取出拥有的子进程；
2. 调用平台对应的进程树终止逻辑；
3. 忽略“进程已退出”等清理期错误，确保应用仍能退出。

菜单 `quit` 在 `app.exit(0)` 前显式调用它；Tauri 最终、不可取消的 `RunEvent::Exit` 也调用它，作为快捷键、Dock 和系统退出的幂等兜底。可取消的 `RunEvent::ExitRequested` 不执行清理，避免退出请求被取消后应用继续运行、后端却已被终止。

## 错误处理

- 后端已自行退出：终止操作允许返回“进程不存在”，随后正常退出应用。
- `SIGTERM` 后仍有后代存活：固定短暂等待后使用 `SIGKILL`。
- Windows `taskkill` 失败：记录错误但不阻塞应用退出。
- Mutex 中毒或状态缺失：不 panic，记录问题后继续退出。

## 测试

- Unix 集成式单元测试创建独立进程组及子孙进程，调用终止函数后验证整个进程组消失。
- 测试清理状态的 `take()` 语义，确保重复调用安全。
- 源码回归测试确认菜单退出和最终 `RunEvent::Exit` 都调用统一清理函数，并确保可取消的 `ExitRequested` 不触发清理。
- 最终运行 Python 回归测试、Rust 单元测试、格式检查和 release 编译检查。
