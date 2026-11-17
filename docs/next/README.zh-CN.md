# herdr-core-tui (`hc-tui`)

<p align="center">
  <img src="assets/logo.png" alt="herdr-core-tui" width="100" />
</p>

<p align="center">
  <a href="README.md">English</a> · 简体中文
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-666666?labelColor=333333" alt="Apache 2.0 license" /></a>
</p>

---

**面向 [`herdr-core` (`hc`)](https://herdr.dev) 的终端 TUI 前端，基于传统 `herdr` Rust TUI 改造。**

本分支将原版 `herdr` Rust/Ratatui + `libghostty-vt` 终端界面改造为连接现有 `herdr-core` (`hc`) 守护进程的进程内前端，与桌面端 `herdr-app` 保持双向实时同步。

## 本分支的定位（以及它不是什么）

- **它是 `herdr-core` (`hc`) 的 TUI 前端：** 工作区、标签页、分屏布局、实时 PTY 流、智能体状态更新（通过 `hc` 的 OSC 7501）以及远程 SSH 机器均由 `hc` 管理，并通过共享的 `"herdr-app"` Automerge 文档实时同步。
- **与 `herdr-app` 双向同步：** 在 `herdr-app` 中配置的工作区、标签页、布局与远程机器会动态出现在 `herdr-core-tui` 中；在 `herdr-core-tui` 中的修改同样会同步回 `herdr-app`。
- **不会与 `hc`、`herdr-app` 或旧版 `herdr` 冲突：** 安装的二进制名为 `herdr-core-tui` 与 `hc-tui`（不使用 `herdr` 或 `hc`），且 `update` 命令不会覆盖现有 `hc` 安装。
- **移除了旧版 `integrations` 安装器与钩子：** 移除设置中的 `integrations` 标签页及旧版 hook 安装逻辑，智能体状态直接依赖 `hc`。
- **不再运行旧版独立 `herdr` 后台服务：** 所有终端会话直接托管在 `hc` 中。

> 命名说明：[`followcat/hc-tui`](https://github.com/followcat/hc-tui) 是一个从零开发的第三方 `herdr-core` Rust TUI 客户端；本仓库（`herdr-core-tui`，别名 `hc-tui`）则是直接基于上游 [`herdrdev/herdr`](https://github.com/herdrdev/herdr) 改造的 `hc` 前端。若系统同时安装了其他工具，可直接调用 `herdr-core-tui` 避免重名。

## 前置要求

需要本地已安装 **`hc` (`herdr-core`)**（独立安装或随 `herdr-app` 附带）。

`herdr-core-tui` 会按以下顺序自动查找 `hc` / `herdr-core`：

1. `HC_BIN` 环境变量（或配置文件中的路径）
2. `HERDR_CORE_DIRS` 环境变量
3. 当前 `PATH` 与登录 shell 的 `PATH`
4. 常见安装路径（`~/.local/bin`、`/opt/homebrew/bin`、`/usr/local/bin`、`~/.local/share/mise/shims`、`~/.nix-profile/bin`、`/run/current-system/sw/bin`、`~/.cargo/bin`，macOS 下的 `~/.local/share/herdr-core/...`，以及 Windows 下的 `%LOCALAPPDATA%\Programs\herdr-core`）

## 安装

### 预编译版本（Fork GitHub Releases）

推送 `v*` 标签（或手动触发 Release 工作流）会自动构建并发布 `-core-tui` 命名的发行产物：

- **Linux（`x86_64` / `aarch64`，静态 musl）：** `herdr-core-tui-linux-*.tar.gz`（含 `herdr-core-tui` 与 `hc-tui`）及单文件二进制 `herdr-core-tui-linux-*`
- **macOS（`x86_64` / `aarch64`）：** `herdr-core-tui-macos-*.tar.gz`（含 `herdr-core-tui` 与 `hc-tui`）及单文件二进制 `herdr-core-tui-macos-*`
- **Windows（`x86_64`）：** `herdr-core-tui-windows-x86_64.zip`（含 `herdr-core-tui.exe`、`hc-tui.exe` 与 ConPTY 运行时）
- **校验文件：** `SHA256SUMS.txt`

### 从源码构建与安装

构建需要 Rust 与 Zig `0.16.0`（用于编译内置 `libghostty-vt`）。若将 Zig `0.16.0` 放在 `.toolchain/zig/zig` 或通过 `ZIG` 环境变量指定即可直接编译安装：

```bash
cargo install --path . --locked
```

这会在 Cargo 的 `bin` 目录同时安装 `herdr-core-tui` 与 `hc-tui`。

## 使用

```bash
herdr-core-tui
# 或使用别名：
hc-tui
```

## 致谢与许可证

基于 [`herdrdev/herdr`](https://github.com/herdrdev/herdr) 分支开发。原项目支持者名单见 [`SPONSORS.md`](./SPONSORS.md)。

采用 [Apache License 2.0](LICENSE) 开源许可。
