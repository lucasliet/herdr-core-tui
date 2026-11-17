# herdr-core-tui (`hc-tui`)

<p align="center">
  <img src="assets/logo.png" alt="herdr-core-tui" width="100" />
</p>

<p align="center">
  English · <a href="README.zh-CN.md">简体中文</a>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-666666?labelColor=333333" alt="Apache 2.0 license" /></a>
</p>

---

**A terminal UI frontend for [`herdr-core` (`hc`)](https://herdr.dev), built on the legacy `herdr` Rust TUI.**

This fork turns the classic `herdr` Rust/Ratatui + `libghostty-vt` terminal workspace UI into an in-process frontend backed by your existing `herdr-core` (`hc`) daemon, keeping full bidirectional parity with the `herdr-app` desktop UI.

## What this fork is (and is not)

- **It is a TUI frontend for `herdr-core` (`hc`):** workspaces, tabs, pane splits, live PTY streams, agent status updates (OSC 7501 via `hc`), and remote SSH machines are owned by `hc` and synchronized through the shared `"herdr-app"` Automerge document.
- **Bidirectional parity with `herdr-app`:** changes you make in `herdr-app` (workspaces, tabs, layouts, local/remote machines) appear dynamically in `herdr-core-tui`, and changes you make in `herdr-core-tui` sync back to `herdr-app`.
- **Does not conflict with or overwrite `hc`, `herdr-app`, or legacy `herdr`:** installed binaries are named `herdr-core-tui` and `hc-tui` (never `herdr` or `hc`), and `update` will never overwrite your existing `hc` installation.
- **No legacy `integrations` installer/hooks:** legacy TUI hook management and the settings `integrations` tab were removed; agent lifecycle and status detection rely directly on `hc`.
- **No standalone legacy server required:** instead of maintaining a separate legacy `herdr` server/socket state, sessions live inside `hc`.

> Note on ecosystem naming: [`followcat/hc-tui`](https://github.com/followcat/hc-tui) is an independent, from-scratch Rust TUI client for `herdr-core`. This repository (`herdr-core-tui`, with convenience alias `hc-tui`) is a direct fork of upstream [`herdrdev/herdr`](https://github.com/herdrdev/herdr) wired onto `hc`. Invoke `herdr-core-tui` if you want to keep both installed side-by-side.

## Prerequisites

You need **`hc` (`herdr-core`)** installed (either standalone or bundled with `herdr-app`).

`herdr-core-tui` resolves the `hc` / `herdr-core` binary automatically from:

1. `HC_BIN` environment variable (or configured path)
2. `HERDR_CORE_DIRS` environment variable
3. Current `PATH` and login-shell `PATH`
4. Standard install locations (`~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/share/mise/shims`, `~/.nix-profile/bin`, `/run/current-system/sw/bin`, `~/.cargo/bin`, plus `~/.local/share/herdr-core/...` on macOS and `%LOCALAPPDATA%\Programs\herdr-core` on Windows)

## Install

### Prebuilt binaries (Fork GitHub Releases)

Tagging `v*` (or running the Release workflow manually) publishes fork release artifacts under `-core-tui` naming:

- **Linux (`x86_64` / `aarch64`, static musl):**
  - `herdr-core-tui-linux-x86_64.tar.gz` / `herdr-core-tui-linux-aarch64.tar.gz` (contains both `herdr-core-tui` and `hc-tui`)
  - Raw standalone binaries: `herdr-core-tui-linux-x86_64`, `herdr-core-tui-linux-aarch64`
- **macOS (`x86_64` / `aarch64`):**
  - `herdr-core-tui-macos-x86_64.tar.gz` / `herdr-core-tui-macos-aarch64.tar.gz` (contains both `herdr-core-tui` and `hc-tui`)
  - Raw standalone binaries: `herdr-core-tui-macos-x86_64`, `herdr-core-tui-macos-aarch64`
- **Windows (`x86_64`):**
  - `herdr-core-tui-windows-x86_64.zip` (contains `herdr-core-tui.exe`, `hc-tui.exe`, and the pinned Microsoft ConPTY runtime)
- **Checksums:** `SHA256SUMS.txt`

Example install from a `.tar.gz` release archive:

```bash
tar -xzf herdr-core-tui-macos-aarch64.tar.gz -C ~/.local/bin
```

### Build and install from source

Building requires Rust and Zig `0.16.0` (for the vendored `libghostty-vt` terminal core). If Zig `0.16.0` is placed at `.toolchain/zig/zig` (or pointed to via `ZIG`), `cargo` picks it up automatically:

```bash
cargo install --path . --locked
```

This installs both `herdr-core-tui` and the `hc-tui` launcher alias into Cargo's `bin` directory (`~/.cargo/bin`).

## Usage

```bash
herdr-core-tui
# or using the convenience alias:
hc-tui
```

Run `herdr-core-tui --version` or `herdr-core-tui --help` for CLI options.

## Releases & CI

- `.github/workflows/ci.yml` runs formatting, lint/clippy, unit/contract checks, and Windows ConPTY package verification on pull requests and pushes to `master`.
- `.github/workflows/release.yml` builds all 5 OS/architecture targets (`herdr-core-tui-*`) and publishes a GitHub Release on the fork whenever a `v*` tag is pushed (or when triggered manually with `workflow_dispatch`).
- `.github/workflows/build-artifacts-manual.yml` provides on-demand artifact builds without creating a release.

## Development

```bash
cargo build --offline --locked --bins
cargo test --offline --locked --bin hc-tui
cargo test --offline --locked --bin herdr-core-tui hc::
bun test ./scripts/docs
bun test scripts/release-workflows.test.ts
python3 -m unittest scripts.test_package_windows_conpty
```

If you are an AI agent working in this repository, read [`AGENTS.md`](./AGENTS.md) before making changes.

## Credits & License

Forked from [`herdrdev/herdr`](https://github.com/herdrdev/herdr). Past upstream backers are listed in [`SPONSORS.md`](./SPONSORS.md).

Licensed under the [Apache License 2.0](LICENSE).
