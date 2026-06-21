---
name: app-build-run
description: "prmonitor Tauri App 本地启动与打包：启动开发版桌面 app，编译 Apple/macOS app/dmg，交叉编译 Windows x64 exe/NSIS installer，处理 cargo-xwin/makensis/llvm 等构建依赖与常见失败。"
argument-hint: "<dev | apple | windows-x64>"
allowed-tools: [Read, Grep, Bash]
---

# App Build & Run

用于 prmonitor（Tauri v2 + Vue 3 + Rust）本地启动、macOS 打包、Windows x64 打包。

## 先做检查

在仓库根目录执行：

```bash
git status --short --branch
pnpm install --frozen-lockfile
rustc -V
cargo -V
pnpm -v
node -v
```

`src-tauri/tauri.conf.json` 中 `beforeBuildCommand` 已配置为 `pnpm build`，所以 `pnpm tauri build` 会自动先跑前端类型检查和 Vite 构建。

## 启动本地 App

开发调试桌面 app：

```bash
pnpm tauri dev
```

只启动前端页面：

```bash
pnpm dev
```

注意：Vite 固定端口是 `1420`，`strictPort: true`。如果端口被占用，先停掉占用进程，不要随意换端口；Tauri 的 `devUrl` 也写死为 `http://localhost:1420`。

## 编译 Apple/macOS App

当前机器架构构建：

```bash
pnpm tauri build
```

常见产物：

```text
src-tauri/target/release/bundle/macos/prmonitor.app
src-tauri/target/release/bundle/dmg/prmonitor_<version>_<arch>.dmg
```

Apple Silicon 机器默认产 `aarch64`。如果要 universal macOS 包：

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
pnpm tauri build --target universal-apple-darwin
```

## 编译 Windows x64 App

推荐：正式发版用 Windows runner/Windows VM 构建并签名。macOS 交叉编译可以用于本地验证，Tauri 会提示 cross-platform compilation is experimental。

Windows runner 上：

```powershell
pnpm install --frozen-lockfile
rustup target add x86_64-pc-windows-msvc
pnpm tauri build --target x86_64-pc-windows-msvc
```

macOS 交叉编译 Windows x64：

```bash
rustup target add x86_64-pc-windows-msvc
cargo install --locked cargo-xwin
brew install makensis llvm
PATH="/opt/homebrew/opt/llvm/bin:$PATH" pnpm tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc
```

不要写成 `pnpm tauri build -- --runner ...`；多余的 `--` 会把 `--runner` 传给 Cargo，导致 `unexpected argument '--runner'`。

常见产物：

```text
src-tauri/target/x86_64-pc-windows-msvc/release/prmonitor.exe
src-tauri/target/x86_64-pc-windows-msvc/release/bundle/nsis/prmonitor_<version>_x64-setup.exe
```

验证 Windows x64：

```bash
file src-tauri/target/x86_64-pc-windows-msvc/release/prmonitor.exe
```

期望包含：

```text
PE32+ executable (GUI) x86-64, for MS Windows
```

## 常见失败

- `failed to find tool "llvm-lib"`：安装 `llvm`，并在构建命令前加 `PATH="/opt/homebrew/opt/llvm/bin:$PATH"`。
- 找不到 `makensis`：`brew install makensis`。
- GitHub 下载 `nsis_tauri_utils.dll` 返回 `http status: 500`：通常是 GitHub release asset 临时/CDN 问题，直接重试；如果手动验证，用 `curl -L -H 'Accept: application/octet-stream' <url>`。
- `Warn ignoring msi`：macOS 交叉编 Windows 时 MSI 不可用，NSIS installer 仍可产出。
- 签名被跳过：非 Windows host 默认不会签 Windows 安装包；正式发布在 Windows runner 上配置签名。
