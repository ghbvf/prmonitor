# prmonitor：无 Apple ID 的 macOS / Windows 构建与 GitHub 发布

本文基于 `ghbvf/prmonitor` 的 `develop` 分支在 2026-07-11 的结构编写。项目是 Tauri v2 桌面应用：Vue 3 + TypeScript + Vite 前端，Rust/Tauri 后端。本文采用**无需 Apple ID、无需 Apple Developer 证书、无需任何 Apple Secret** 的 macOS ad-hoc 构建方案；Windows 发布 **portable zip**（解压即用，不编 NSIS/MSI 安装器）。

权威 git 远端为 Azure DevOps；GitHub（`ghbvf/prmonitor`）是镜像并承载 GitHub Actions。发版前需先把含 `release.yml` 的 `develop` 镜像到 GitHub，再在 GitHub 上建标签并**手动 Run workflow**（推荐）或推送 `v*` 标签。

## 1. 当前项目的打包条件

项目已经具备打包基础：

- `src-tauri/tauri.conf.json` 中 `bundle.active` 为 `true`，并包含 `.icns`、`.ico` 和 PNG 图标。
- `beforeBuildCommand` 会依次执行 `pnpm build` 与 `pnpm build:web`。
- `bundle.resources` 会把 `resources/iterm-daemon/iterm_daemon.py` 放入应用资源。
- macOS 可生成 `.app` 和 `.dmg`；Windows 发版产物为 **portable zip**（`tauri build --no-bundle` 得到 `prmonitor.exe` 后打包），不发布安装器。
- 当前版本号在三个文件中重复维护：`package.json`、`src-tauri/Cargo.toml`、`src-tauri/tauri.conf.json`。发布前必须一致。

需要特别区分“成功打包”和“最终用户无需安装任何依赖”：README 仍把 `gh` CLI 与 `codex` CLI 列为运行前提。打包不会自动把这两个 CLI 放进应用。iTerm 集成功能还会显式执行 `python3 iterm_daemon.py`，并要求该 Python 环境已安装 `iterm2` 包；把脚本放进 bundle 并不等于把 Python 解释器和模块一起打包。面向外部用户发布前，应在一台干净机器上验证 CLI 查找、登录状态、GUI 应用的 `PATH`、Python 运行时以及所有首次启动流程。

截至 2026-07-11，仓库已有一个 `v0.1` Pre-release（4 个资产），但当前 `.github/workflows/` 中只有 `ci.yml`，没有可复现该发布过程的自动打包/发布工作流。本文方案对后续版本采用严格的三段式标签 `vMAJOR.MINOR.PATCH`；由于当前项目版本是 `0.1.0` 且已有语义接近的 `v0.1`，建议首次自动发布直接升级为 `v0.1.1`，避免重复版本含义。

## 2. macOS 本地开发与构建

### 2.1 安装工具链

建议使用 Apple Silicon 或 Intel Mac，并安装：

```bash
xcode-select --install

# 安装 Rust；已安装时只需更新
rustup update stable

# Node.js 22 或更高版本
node --version

# pnpm 11
corepack enable
corepack prepare pnpm@11 --activate
pnpm --version
```

项目运行时还需要：

```bash
brew install gh
# codex CLI 按其官方方式安装

gh auth login
# codex 登录命令按当前 CLI 提示执行

# 仅使用 iTerm 集成功能时需要
python3 -m pip install iterm2
```

### 2.2 安装依赖和开发运行

```bash
git clone https://github.com/ghbvf/prmonitor.git
cd prmonitor
git checkout develop

pnpm install --frozen-lockfile
pnpm tauri dev
```

### 2.3 生成当前 Mac 架构的 `.app` 与 `.dmg`

没有 Apple ID 或 Developer ID 证书时，建议使用 Tauri 支持的匿名 ad-hoc 签名，而不是刻意生成完全未签名的 Apple Silicon 应用：

```bash
APPLE_SIGNING_IDENTITY=- \
  pnpm tauri build --bundles app,dmg
```

典型输出目录：

```text
src-tauri/target/release/bundle/macos/*.app
src-tauri/target/release/bundle/dmg/*.dmg
```

这里的 `-` 是 `codesign` 的伪身份。它不需要 Apple 账户或证书，但从技术上仍会给应用写入匿名 ad-hoc 签名。该签名不能证明开发者身份，也不能完成 Apple notarization；从浏览器或 GitHub 下载后，macOS Gatekeeper 仍可能要求用户手动允许。

可检查签名类型：

```bash
APP="src-tauri/target/release/bundle/macos/prmonitor.app"
codesign --verify --deep --strict --verbose=2 "$APP"
codesign -dv --verbose=4 "$APP" 2>&1 | grep 'Signature=adhoc'
```

### 2.4 生成 Intel + Apple Silicon 通用版本

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin

APPLE_SIGNING_IDENTITY=- \
  pnpm tauri build \
  --target universal-apple-darwin \
  --bundles app,dmg
```

输出目录：

```text
src-tauri/target/universal-apple-darwin/release/bundle/macos/*.app
src-tauri/target/universal-apple-darwin/release/bundle/dmg/*.dmg
```

可验证主程序是否同时包含两种架构：

```bash
APP="src-tauri/target/universal-apple-darwin/release/bundle/macos/prmonitor.app"
EXECUTABLE=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$APP/Contents/Info.plist")
lipo "$APP/Contents/MacOS/$EXECUTABLE" -archs
```

输出应同时包含 `x86_64` 与 `arm64`。

### 2.5 在其他 Mac 上首次打开 ad-hoc 应用

对下载者明确说明：该 macOS 包没有 Developer ID，也没有经过 Apple 公证。建议按以下顺序操作：

1. 下载 `.dmg` 或 `.app.zip`，并下载 `SHA256SUMS.txt`（GitHub Release）或 `SHA256SUMS-macos.txt`（Actions Artifact）。
2. 先核对目标文件的 SHA-256，例如：

```bash
shasum -a 256 prmonitor_0.1.1_macos_universal_adhoc.dmg
grep 'prmonitor_0.1.1_macos_universal_adhoc.dmg' SHA256SUMS.txt
```

3. 将 `prmonitor.app` 放入 `/Applications`。
4. 在 Finder 中按住 Control 点击（或右键）应用，选择“打开”。
5. 若仍被拦截，先尝试打开一次，然后进入“系统设置 → 隐私与安全性 → 安全性”，选择“仍要打开”。
6. 只有在来源可信且 SHA-256 一致时，才考虑终端兜底：

```bash
xattr -dr com.apple.quarantine /Applications/prmonitor.app
open /Applications/prmonitor.app
```

不要通过全局关闭 Gatekeeper 的方式解决，也不要让用户对来源不明的应用执行 `xattr`。

## 3. Windows portable（推荐由 GitHub Actions 构建）

### 推荐方案：`windows-latest` 上 `--no-bundle` + zip

发版不使用 NSIS/MSI。工作流在 Windows Runner 执行：

```bash
pnpm tauri build --target x86_64-pc-windows-msvc --no-bundle
```

然后把 `prmonitor.exe`（若 release 旁有 `resources/` 则一并纳入）打成：

```text
prmonitor_<version>_windows_x64_portable.zip
```

用户解压后直接运行 `prmonitor.exe`。系统需已安装 WebView2；无安装向导。

### 本地 / 交叉编译备选

在 Windows 本机可同样使用 `--no-bundle` 得到 `src-tauri/target/.../release/prmonitor.exe` 后自行 zip。

macOS 上若需交叉编译验证二进制（非发版路径），可用 `cargo-xwin` + `--no-bundle`；**不要**再装 makensis 或传 `--bundles nsis`。交叉编译仍属实验性，正式资产以 Actions Windows Runner 为准。

## 4. GitHub Actions 构建与发布方案

核心配置：

```text
.github/workflows/release.yml
scripts/check-release-version.mjs
.github/workflows/ci.yml
docs/BUILD_RELEASE.zh-CN.md
```

本方案的行为：

1. **推荐**：在 GitHub 上对已存在的三段式标签手动 `workflow_dispatch`（`publish=true`）发版。
2. **辅路径**：向 GitHub 推送 `v*` 标签也会自动构建并发布。
3. 校验标签格式，并确保三个项目版本号与标签一致。
4. 在 macOS Runner 构建 universal ad-hoc `.app.zip` 与 `.dmg`。
5. 在 Windows Runner 构建 x64 **portable zip**（`--no-bundle`）。
6. 生成平台校验文件及总 `SHA256SUMS.txt`。
7. `publish=true`（或标签推送）时创建 GitHub Release；重跑时覆盖同名资产。
8. `v0.2.0-rc.1` 这类标签自动建立 prerelease。
9. 手动运行默认 `publish=false` 只生成 Actions Artifacts。

### 4.1 无 Apple ID 的发布策略

本配置固定采用以下方式构建 macOS 包：

```yaml
env:
  APPLE_SIGNING_IDENTITY: "-"
```

具体行为：

- 不读取 `APPLE_ID`、`APPLE_PASSWORD`、`APPLE_TEAM_ID`、`APPLE_CERTIFICATE` 等任何 Apple Secret。
- 标签触发时，即使仓库没有配置 Apple 账户或证书，也会继续构建并创建 GitHub Release。
- macOS `.app` 会通过 `codesign --verify` 检查，并确认输出包含 `Signature=adhoc`。
- macOS 文件名明确包含 `_adhoc`，避免与 Developer ID 签名、公证版本混淆。
- Release 页面自动在生成的更新日志之前加入未签名/未公证警告。
- Release 同时附带 `MACOS_INSTALL_NOTICE.zh-CN.txt`，给下载用户说明校验及 Gatekeeper 放行步骤。
- Windows portable zip / `prmonitor.exe` 未做 Authenticode 签名，可能显示 SmartScreen 警告。

该方案可以公开发布文件，但不能获得 Apple 的开发者身份验证与恶意软件公证结果。发布者必须接受安装体验较差、用户信任成本较高这一限制。

### 4.2 GitHub Secrets

**本方案不需要配置任何 Apple Secret。** GitHub Actions 自动提供的 `GITHUB_TOKEN` 用于创建 Release，不需要手工创建 token。

如果仓库里已经存在旧的 Apple Secrets，它们不会被当前 `release.yml` 引用；可以保留，也可以由仓库管理员删除。以后获得 Developer ID 证书后，不能只添加 Secret 就期望自动切换，因为当前工作流明确固定为 `APPLE_SIGNING_IDENTITY=-`。届时应改回 Developer ID 签名和 notarization 流程，或者新建独立的正式签名工作流。

### 4.3 GitHub Actions 权限

工作流已经在 `publish` Job 中声明：

```yaml
permissions:
  contents: write
```

若发布仍报 `Resource not accessible by integration`，检查：

```text
Settings → Actions → General → Workflow permissions
```

仓库或组织策略必须允许工作流写入 Contents。组织级策略可能覆盖仓库设置。

### 4.4 发布一个版本（推荐：手动 Run workflow）

必须先把发布工作流合并到 Azure `develop`，再镜像到 GitHub，再在 **GitHub** 上创建标签并手动发版。仓库现有 `v0.1` 是旧的两段式标签，不要复用；建议首次发布从 `v0.1.1` 开始。

1. 三处版本改为 `0.1.1` 并校验：

```bash
cargo check --manifest-path src-tauri/Cargo.toml
node scripts/check-release-version.mjs 0.1.1
pnpm install --frozen-lockfile
pnpm build
pnpm test
```

2. 合并到 Azure `develop` 后镜像：

```bash
bash hack/automation/mirror-to-github.sh
```

3. 在 GitHub 为该 commit 创建标注标签 `v0.1.1`（或本地打 tag 后 **push 到 GitHub**；只推 Azure 不会触发 GHA）。

4. 打开 GitHub：`Actions → Release desktop apps → Run workflow`：

   - `tag` = `v0.1.1`（必须已存在于 GitHub）
   - `publish=true` → 构建并创建/更新 Release
   - `publish=false` → 只生成 Artifacts

辅路径：向 GitHub 推送 `v0.1.1` 也会自动构建发布。不需要 Apple ID。

成功后的 Release 资产包括：

```text
prmonitor_0.1.1_macos_universal_adhoc.app.zip
prmonitor_0.1.1_macos_universal_adhoc.dmg
prmonitor_0.1.1_windows_x64_portable.zip
MACOS_INSTALL_NOTICE.zh-CN.txt
WINDOWS_INSTALL_NOTICE.zh-CN.txt
SHA256SUMS-macos.txt
SHA256SUMS-windows.txt
SHA256SUMS.txt
```

`.app` 本身是目录，不能作为普通 Release 文件直接上传，所以工作流用 `ditto` 保留 macOS 元数据并打成 `.app.zip`。Windows 用户解压 portable zip 后运行 `prmonitor.exe`（zip 内含 `WINDOWS_INSTALL_NOTICE.zh-CN.txt`）。

工作流不再提供 `allow_adhoc_macos`；macOS 始终 ad-hoc；`publish=true` 不会因缺少 Apple ID 失败。

## 5. 签名状态与发布质量

### macOS：匿名 ad-hoc 签名

当前自动发布结果是：

```text
匿名 ad-hoc 签名 → 不做 Apple notarization → 发布 .app.zip / .dmg
```

它的边界必须明确：

- 不需要 Apple ID、开发者会员或证书。
- 可满足 Apple Silicon 可执行代码需要签名的基本技术要求。
- 不能让 Gatekeeper 识别开发者，也不能证明文件经过 Apple 恶意软件检查。
- 用户可能看到“无法验证开发者”或“Apple 无法检查是否包含恶意软件”等提示。
- SHA-256 只能帮助用户核对下载文件是否与 Release 资产一致，不能替代代码签名、公证或安全审计。

若以后需要面向普通用户提供接近无阻碍的安装体验，应改为：

```text
Developer ID Application 签名 → Apple notarization → staple → 发布 DMG
```

### Windows：portable、未做 Authenticode 签名

Windows portable zip / `prmonitor.exe` 未签名也能分发，但公众下载常会看到 SmartScreen。正式生产方案应选择传统 OV/EV Code Signing Certificate、Azure Trusted Signing，或其他受支持的云签名服务，并覆盖可执行文件本身。接入方式取决于证书提供商，通常通过 Tauri `bundle.windows.signCommand` 或服务商官方 Action 实现，不应把 `.pfx` 明文提交到仓库。

## 6. 当前项目的发布风险清单

发布前至少验证：

- 三处版本号完全一致，标签与版本一致。
- `gh` 与 `codex` 在干净系统上可被 GUI 进程找到；macOS GUI 通常不会继承终端 `.zshrc` 的 `PATH`。
- 用户未登录 `gh`/`codex` 时有清晰错误提示，而不是静默失败。
- `iterm_daemon.py` 所需 Python 解释器和模块在目标机器可用，或改成真正打包的 sidecar。
- Apple Silicon 与 Intel Mac 都能启动 universal app。
- Windows 10/11 x64 上 WebView2 的安装/引导行为正常。
- 深链 `prmonitor://` 在 portable 场景下的注册/升级行为符合预期。
- 应用升级不会破坏本地 SQLite/配置数据。
- GitHub Release 的 SHA-256 与本地下载文件一致。
- macOS ad-hoc 包在当前支持的 macOS 版本上完成 Gatekeeper 放行测试，并在下载页明确告知风险。
- Windows portable zip 在 Windows 10/11 干净环境解压运行，并确认 WebView2 与 SmartScreen 行为。

## 7. 官方参考

- Tauri GitHub Actions：<https://v2.tauri.app/distribute/pipelines/github/>
- Tauri macOS App Bundle：<https://v2.tauri.app/distribute/macos-application-bundle/>
- Tauri DMG：<https://v2.tauri.app/distribute/dmg/>
- Tauri CLI `--no-bundle`（跳过安装器 bundler）：`pnpm tauri build --help`
- Tauri macOS 签名与公证：<https://v2.tauri.app/distribute/sign/macos/>
- Tauri Windows 签名：<https://v2.tauri.app/distribute/sign/windows/>
- GitHub Actions 权限：<https://docs.github.com/actions/using-workflows/workflow-syntax-for-github-actions#permissions>
- Apple：打开来自未知开发者的应用：<https://support.apple.com/guide/mac-help/open-a-mac-app-from-an-unknown-developer-mh40616/mac>
