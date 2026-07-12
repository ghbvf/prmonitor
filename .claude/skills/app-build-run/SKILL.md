---
name: app-build-run
description: "prmonitor Tauri App 本地启动与打包：启动开发版桌面 app，编译 Apple/macOS app/dmg，交叉编译 Windows x64 exe；正式发版 Windows 为 portable zip（见 release.yml --no-bundle），本地交叉编仍可能涉及 cargo-xwin/makensis。"
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

`src-tauri/tauri.conf.json` 中 `beforeBuildCommand` 已配置为 `pnpm build && pnpm build:web`，所以 `pnpm tauri build` 会自动先跑前端类型检查 + 桌面 Vite 构建（`dist`）+ 远程 Web 终端 SPA 构建（`dist-web`，#1504 经 rust-embed 嵌入 terminal listener）。无需手动先跑 `build:web`。

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

启动已编译的 release `.app`：

```bash
open -n src-tauri/target/release/bundle/macos/prmonitor.app
```

不要为普通重启重签。只有刚重新 `pnpm tauri build`、bundle 文件变动、或 `codesign --verify --deep --strict --verbose=4 src-tauri/target/release/bundle/macos/prmonitor.app` 失败时，才进入下面的 macOS 签名复用流程。

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

### 本机 macOS 签名证书复用

本机 release `.app` 需要用已信任的本地 code signing identity 重签后再 `open`。不要每次生成新证书；先复用保存的证书：

```text
~/Library/Application Support/prmonitor/codesign/
├── current-name.txt
├── current-cert-path.txt
├── current-p12-path.txt
├── prmonitor-local-code-signing-20260624202710.cer
└── prmonitor-local-code-signing-20260624202710.p12
```

当前可用 identity：

```text
prmonitor Local Code Signing 20260624202710
```

构建后或签名失效时重签：

```bash
.claude/skills/app-build-run/scripts/macos-local-codesign.sh
open -n src-tauri/target/release/bundle/macos/prmonitor.app
```

脚本先验证现有签名：如果 `.app` 已经由当前 identity 签名且 `codesign --verify` 通过，会直接跳过，不会重复重签。需要强制重签时传 `--force`。脚本只做复用：先查 `security find-identity -v -p codesigning`，identity 缺失时从保存的 `.p12` 导入；不会自动 `openssl` 生成新证书，避免钥匙串出现重复项。`.p12` 是本机私钥材料，只保存在用户目录，不能提交到 git。

如果脚本提示证书未被系统信任，运行一次：

```bash
sudo security add-trusted-cert -d -r trustRoot -p codeSign -k /Library/Keychains/System.keychain "$HOME/Library/Application Support/prmonitor/codesign/prmonitor-local-code-signing-20260624202710.cer"
```

验证：

```bash
security verify-cert -c "$HOME/Library/Application Support/prmonitor/codesign/prmonitor-local-code-signing-20260624202710.cer" -p codeSign
codesign --verify --deep --strict --verbose=4 src-tauri/target/release/bundle/macos/prmonitor.app
```

## 编译 Windows x64 App

推荐：正式发版由 GitHub Actions `windows-latest` 执行 `pnpm tauri build --target x86_64-pc-windows-msvc --no-bundle`，产物为 portable zip（见 `.github/workflows/release.yml` / `docs/BUILD_RELEASE.zh-CN.md`）。本地交叉编译仅用于验证二进制。

Windows runner 上（本地验证 / 非发版）：

```powershell
pnpm install --frozen-lockfile
rustup target add x86_64-pc-windows-msvc
pnpm tauri build --target x86_64-pc-windows-msvc --no-bundle
```

macOS 交叉编译 Windows x64（实验性，非正式发版路径）：

```bash
rustup target add x86_64-pc-windows-msvc
cargo install --locked cargo-xwin
brew install llvm
# 若仍要本地试 NSIS：另装 makensis，并去掉 --no-bundle、改回 --bundles nsis
PATH="/opt/homebrew/opt/llvm/bin:$PATH" pnpm tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc --no-bundle
```

不要写成 `pnpm tauri build -- --runner ...`；多余的 `--` 会把 `--runner` 传给 Cargo，导致 `unexpected argument '--runner'`。

常见产物：

```text
# 发版 / --no-bundle
src-tauri/target/x86_64-pc-windows-msvc/release/prmonitor.exe
# GitHub Release 资产名
prmonitor_<version>_windows_x64_portable.zip
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
- 本地仍尝试 NSIS 时找不到 `makensis`：`brew install makensis`（正式发版不需要）。
- GitHub 下载 `nsis_tauri_utils.dll` 返回 `http status: 500`：通常是 GitHub release asset 临时/CDN 问题，直接重试；如果手动验证，用 `curl -L -H 'Accept: application/octet-stream' <url>`。
- `Warn ignoring msi`：macOS 交叉编 Windows 时 MSI 不可用。
- 签名被跳过：非 Windows host 默认不会签 Windows 包；正式发布若需 Authenticode，在 Windows runner 上配置签名。
