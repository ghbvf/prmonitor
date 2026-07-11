use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    future::Future,
    io,
    path::{Path, PathBuf},
    process::{ExitStatus, Output, Stdio},
    sync::Mutex,
};

#[cfg(unix)]
use std::{
    io::Read,
    process::Command as StdCommand,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use strum::IntoEnumIterator;
use tokio::process::{Child, Command};

#[cfg(any(windows, test))]
use super::model::is_windows_cli_extension;
use super::model::{canonical_cli_name, is_executable_file, CliToolsConfig};
use crate::{
    error::{AppError, AppResult},
    model::{CliResolutionSource, CliTool},
};

#[cfg(unix)]
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const LOGIN_SHELL_OUTPUT_LIMIT: usize = 64 * 1024;
const PATH_BEGIN: &[u8] = b"__PRMONITOR_PATH_BEGIN__";
const PATH_END: &[u8] = b"__PRMONITOR_PATH_END__";

/// A validated launch capability for one managed CLI.
///
/// **Hard carrier (downstream):** fields and construction are private to this module; callers can
/// only obtain a value through `config::service::resolve_cli` and can only create a subprocess via
/// [`Self::command`], which returns a restricted [`ManagedCommand`]. The wrapper exposes process
/// arguments, working directory, stdio, lifecycle, and execution, but deliberately has no
/// environment mutation API. Keeping the resolved executable and its PATH together is therefore a
/// type-system guarantee. The sealed `CliTool` plus exhaustive config lookup closes the upstream
/// side of the same funnel.
#[derive(Debug, Clone)]
pub struct ResolvedCli {
    program: PathBuf,
    source: CliResolutionSource,
    enhanced_path: OsString,
    fingerprint: String,
}

impl ResolvedCli {
    pub fn command(&self) -> ManagedCommand {
        let mut command = Command::new(&self.program);
        command.env("PATH", &self.enhanced_path);
        ManagedCommand { inner: command }
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    fn diagnostics(&self) -> CliResolutionDiagnostics {
        CliResolutionDiagnostics {
            resolved_path: self.program.to_string_lossy().into_owned(),
            source: self.source,
            fingerprint: self.fingerprint.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(program: impl Into<PathBuf>) -> Self {
        let program = program.into();
        resolved(
            program,
            CliResolutionSource::Custom,
            std::env::var_os("PATH").unwrap_or_default(),
        )
    }
}

/// Restricted launch builder for managed CLIs.
///
/// Environment mutation is intentionally absent: callers cannot remove or replace the PATH bound
/// to the validated executable by [`ResolvedCli`].
pub struct ManagedCommand {
    inner: Command,
}

impl ManagedCommand {
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner.args(args);
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.inner.current_dir(dir);
        self
    }

    pub fn stdin(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.inner.stdin(stdio);
        self
    }

    pub fn stdout(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.inner.stdout(stdio);
        self
    }

    pub fn stderr(&mut self, stdio: impl Into<Stdio>) -> &mut Self {
        self.inner.stderr(stdio);
        self
    }

    pub fn kill_on_drop(&mut self, enabled: bool) -> &mut Self {
        self.inner.kill_on_drop(enabled);
        self
    }

    pub fn spawn(&mut self) -> io::Result<Child> {
        self.inner.spawn()
    }

    pub fn status(&mut self) -> impl Future<Output = io::Result<ExitStatus>> + '_ {
        self.inner.status()
    }

    pub fn output(&mut self) -> impl Future<Output = io::Result<Output>> + '_ {
        self.inner.output()
    }

    #[cfg(test)]
    pub(crate) fn as_std(&self) -> &std::process::Command {
        self.inner.as_std()
    }
}

/// Read-only probe data detached from the launch capability.
///
/// The config slice creates this DTO while `ResolvedCli` keeps its executable and enhanced PATH
/// private. Diagnostics can cross into the composition root without creating a second subprocess
/// construction API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliResolutionDiagnostics {
    pub resolved_path: String,
    pub source: CliResolutionSource,
    pub fingerprint: String,
}

/// Probe-only envelope: configured and resolved display paths are copied inside the config slice,
/// while the `ResolvedCli` launch capability never crosses this diagnostic seam.
#[derive(Debug, Clone)]
pub struct CliProbeDiagnostics {
    pub configured_path: String,
    pub resolution: AppResult<CliResolutionDiagnostics>,
}

/// Owns the one process-lifetime login-shell PATH cache. Its resolution method stays private to the
/// config slice; sibling slices receive only `ResolvedCli` from `config::service`.
#[derive(Default)]
pub struct CliResolver {
    login_shell_path: Mutex<Option<Option<OsString>>>,
}

impl CliResolver {
    pub(super) fn resolve(
        &self,
        tools: &CliToolsConfig,
        tool: CliTool,
        refresh_path: bool,
    ) -> AppResult<ResolvedCli> {
        tools.validate_tool(tool)?;
        let login_shell_path = self.cached_login_shell_path(refresh_path);
        let inputs = ResolveInputs {
            process_path: std::env::var_os("PATH"),
            login_shell_path,
            platform_fallbacks: platform_fallback_dirs(),
            path_extensions: path_extensions(),
        };
        resolve_with_inputs(tools, tool, &inputs)
    }

    pub(super) fn diagnostics(
        &self,
        tools: &CliToolsConfig,
        tool: CliTool,
        refresh_path: bool,
    ) -> CliProbeDiagnostics {
        CliProbeDiagnostics {
            configured_path: tools.path(tool).as_str().to_string(),
            resolution: self
                .resolve(tools, tool, refresh_path)
                .map(|resolved| resolved.diagnostics()),
        }
    }

    pub(super) fn diagnostics_all(
        &self,
        tools: &CliToolsConfig,
        refresh_path: bool,
    ) -> Vec<CliProbeDiagnostics> {
        self.diagnostics_all_with(tools, refresh_path, discover_login_shell_path)
    }

    fn diagnostics_all_with<F>(
        &self,
        tools: &CliToolsConfig,
        refresh_path: bool,
        discover: F,
    ) -> Vec<CliProbeDiagnostics>
    where
        F: FnOnce() -> Option<OsString>,
    {
        // Refresh once before validating any individual draft. Every row in this batch then uses
        // the same login-shell snapshot, so one invalid tool cannot gate refresh for its siblings.
        let login_shell_path = self.cached_login_shell_path_with(refresh_path, discover);
        let inputs = ResolveInputs {
            process_path: std::env::var_os("PATH"),
            login_shell_path,
            platform_fallbacks: platform_fallback_dirs(),
            path_extensions: path_extensions(),
        };
        CliTool::iter()
            .map(|tool| CliProbeDiagnostics {
                configured_path: tools.path(tool).as_str().to_string(),
                resolution: resolve_with_inputs(tools, tool, &inputs)
                    .map(|resolved| resolved.diagnostics()),
            })
            .collect()
    }

    fn cached_login_shell_path(&self, refresh: bool) -> Option<OsString> {
        self.cached_login_shell_path_with(refresh, discover_login_shell_path)
    }

    fn cached_login_shell_path_with<F>(&self, refresh: bool, discover: F) -> Option<OsString>
    where
        F: FnOnce() -> Option<OsString>,
    {
        if !refresh {
            let cached = self
                .login_shell_path
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(value) = cached.as_ref() {
                return value.clone();
            }
        }

        // Shell startup is external and may consume the full timeout. Never hold the cache mutex
        // across it: unrelated callers can continue using a value installed by another probe.
        let discovered = discover();
        let mut cached = self
            .login_shell_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if refresh || cached.is_none() {
            *cached = Some(discovered);
        }
        cached.clone().flatten()
    }
}

struct ResolveInputs {
    process_path: Option<OsString>,
    login_shell_path: Option<OsString>,
    platform_fallbacks: Vec<PathBuf>,
    path_extensions: Vec<OsString>,
}

fn resolve_with_inputs(
    tools: &CliToolsConfig,
    tool: CliTool,
    inputs: &ResolveInputs,
) -> AppResult<ResolvedCli> {
    tools.validate_tool(tool)?;
    let configured = tools.path(tool);
    let fallback_path = std::env::join_paths(&inputs.platform_fallbacks).unwrap_or_default();
    let enhanced_path = merge_paths([
        inputs.process_path.as_deref(),
        inputs.login_shell_path.as_deref(),
        Some(fallback_path.as_os_str()),
    ]);

    if !configured.is_auto() {
        let program = PathBuf::from(configured.as_str());
        return Ok(resolved(
            program,
            CliResolutionSource::Custom,
            enhanced_path,
        ));
    }

    let sources = [
        (
            inputs.process_path.as_deref(),
            CliResolutionSource::ProcessPath,
        ),
        (
            inputs.login_shell_path.as_deref(),
            CliResolutionSource::LoginShellPath,
        ),
        (
            Some(fallback_path.as_os_str()),
            CliResolutionSource::PlatformFallback,
        ),
    ];
    for (path, source) in sources {
        if let Some(program) = find_on_path(path, tool, &inputs.path_extensions) {
            return Ok(resolved(program, source, enhanced_path));
        }
    }

    Err(AppError::new(format!(
        "未找到可执行的 {}；请安装该 CLI 或配置绝对路径",
        canonical_cli_name(tool)
    )))
}

fn resolved(program: PathBuf, source: CliResolutionSource, enhanced_path: OsString) -> ResolvedCli {
    debug_assert!(
        program.is_absolute(),
        "resolved CLI program must be absolute"
    );
    let fingerprint = format!(
        "{}\0{}",
        program.to_string_lossy(),
        enhanced_path.to_string_lossy()
    );
    ResolvedCli {
        program,
        source,
        enhanced_path,
        fingerprint,
    }
}

fn merge_paths<'a>(paths: impl IntoIterator<Item = Option<&'a OsStr>>) -> OsString {
    let mut seen = HashSet::<OsString>::new();
    let mut merged = Vec::new();
    for path in paths.into_iter().flatten() {
        for directory in std::env::split_paths(path) {
            // Empty PATH entries mean the current directory on Unix, and relative entries change
            // meaning with `current_dir`. Neither may participate in discovery or child PATH.
            if !directory.is_absolute() {
                continue;
            }
            let key = path_dedup_key(&directory);
            if seen.insert(key) {
                merged.push(directory);
            }
        }
    }
    std::env::join_paths(merged).unwrap_or_default()
}

fn path_dedup_key(path: &Path) -> OsString {
    #[cfg(windows)]
    {
        path.as_os_str()
            .to_string_lossy()
            .to_ascii_lowercase()
            .into()
    }
    #[cfg(not(windows))]
    {
        path.as_os_str().to_os_string()
    }
}

fn find_on_path(
    path: Option<&OsStr>,
    tool: CliTool,
    path_extensions: &[OsString],
) -> Option<PathBuf> {
    let canonical = canonical_cli_name(tool);
    for directory in path.into_iter().flat_map(std::env::split_paths) {
        if !directory.is_absolute() {
            continue;
        }
        for candidate in executable_candidates(&directory, canonical, path_extensions) {
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable_candidates(
    directory: &Path,
    canonical: &str,
    path_extensions: &[OsString],
) -> Vec<PathBuf> {
    #[cfg(not(windows))]
    {
        let _ = path_extensions;
        vec![directory.join(canonical)]
    }
    #[cfg(windows)]
    {
        path_extensions
            .iter()
            .map(|extension| {
                let mut filename = OsString::from(canonical);
                filename.push(extension);
                directory.join(filename)
            })
            .collect()
    }
}

#[cfg(unix)]
fn discover_login_shell_path() -> Option<OsString> {
    let shell = select_login_shell(std::env::var_os("SHELL"), account_login_shell())?;
    discover_login_shell_path_from(&shell, LOGIN_SHELL_TIMEOUT)
}

#[cfg(unix)]
fn select_login_shell(
    environment_shell: Option<OsString>,
    account_shell: Option<OsString>,
) -> Option<PathBuf> {
    [environment_shell, account_shell]
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .find(|shell| shell.is_absolute() && is_executable_file(shell))
}

#[cfg(unix)]
fn account_login_shell() -> Option<OsString> {
    use std::{ffi::CStr, os::unix::ffi::OsStringExt, ptr};

    let buffer_size = unsafe {
        let suggested = libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX);
        if suggested > 0 {
            suggested as usize
        } else {
            16 * 1024
        }
    };
    let mut buffer = vec![0_u8; buffer_size];
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    let entry = unsafe { entry.assume_init() };
    if entry.pw_shell.is_null() {
        return None;
    }
    let bytes = unsafe { CStr::from_ptr(entry.pw_shell) }.to_bytes();
    (!bytes.is_empty()).then(|| OsString::from_vec(bytes.to_vec()))
}

#[cfg(unix)]
fn discover_login_shell_path_from(shell: &Path, timeout: Duration) -> Option<OsString> {
    use std::os::unix::process::CommandExt;

    let mut command = StdCommand::new(shell);
    command
        .args([
            "-ilc",
            "printf '__PRMONITOR_PATH_BEGIN__%s__PRMONITOR_PATH_END__' \"$PATH\"",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Give the shell and every descendant a private process group so a timed-out startup can be
    // terminated as one unit, including descendants that inherited and kept stdout open.
    command.process_group(0);
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let (output_tx, output_rx) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout
            .take((LOGIN_SHELL_OUTPUT_LIMIT + 1) as u64)
            .read_to_end(&mut output)
            .map(|_| output);
        let _ = output_tx.send(result);
    });

    let deadline = Instant::now() + timeout;
    let mut status = None;
    let mut output = None;
    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(value)) => status = Some(value),
                Ok(None) => {}
                Err(_) => {
                    terminate_process_group(&mut child);
                    let _ = reader.join();
                    return None;
                }
            }
        }
        if output.is_none() {
            match output_rx.try_recv() {
                Ok(Ok(value)) => output = Some(value),
                Ok(Err(_)) => {
                    terminate_process_group(&mut child);
                    let _ = reader.join();
                    return None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    terminate_process_group(&mut child);
                    let _ = reader.join();
                    return None;
                }
            }
        }
        if status.is_some() && output.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            terminate_process_group(&mut child);
            let _ = reader.join();
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let _ = reader.join();

    if !status?.success() {
        return None;
    }
    let output = output?;
    if output.len() > LOGIN_SHELL_OUTPUT_LIMIT {
        return None;
    }
    parse_marked_path(&output)
}

#[cfg(unix)]
fn terminate_process_group(child: &mut std::process::Child) {
    const SIGKILL: i32 = 9;
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }

    let process_group = -(child.id() as i32);
    // SAFETY: `process_group` is the negated id returned by the child we spawned with
    // `process_group(0)`, and SIGKILL has no pointer or lifetime requirements.
    let _ = unsafe { kill(process_group, SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn discover_login_shell_path() -> Option<OsString> {
    None
}

fn parse_marked_path(output: &[u8]) -> Option<OsString> {
    let begin = find_subslice(output, PATH_BEGIN)? + PATH_BEGIN.len();
    let end = find_subslice(&output[begin..], PATH_END)? + begin;
    if end <= begin {
        return None;
    }
    bytes_to_os_string(&output[begin..end])
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(unix)]
fn bytes_to_os_string(bytes: &[u8]) -> Option<OsString> {
    use std::os::unix::ffi::OsStringExt;
    Some(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
fn bytes_to_os_string(bytes: &[u8]) -> Option<OsString> {
    String::from_utf8(bytes.to_vec()).ok().map(OsString::from)
}

fn path_extensions() -> Vec<OsString> {
    #[cfg(windows)]
    {
        let raw = std::env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
        parse_windows_path_extensions(&raw)
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

#[cfg(any(windows, test))]
fn parse_windows_path_extensions(raw: &OsStr) -> Vec<OsString> {
    raw.to_string_lossy()
        .split(';')
        .filter(|extension| is_windows_cli_extension(extension))
        .map(|extension| {
            let extension = extension.trim_start_matches('.').to_ascii_lowercase();
            OsString::from(format!(".{extension}"))
        })
        .collect()
}

fn platform_fallback_dirs() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(target_os = "macos")]
    paths.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    #[cfg(all(unix, not(target_os = "macos")))]
    paths.extend(["/usr/local/bin", "/usr/bin", "/snap/bin"].map(PathBuf::from));
    #[cfg(unix)]
    if let Some(home) = std::env::var_os("HOME") {
        paths.extend(home_fallback_dirs(
            Path::new(&home),
            cfg!(target_os = "macos"),
        ));
    }
    #[cfg(windows)]
    {
        paths.extend(windows_fallback_dirs(
            std::env::var_os("ProgramFiles").as_deref(),
            std::env::var_os("LOCALAPPDATA").as_deref(),
            std::env::var_os("APPDATA").as_deref(),
            std::env::var_os("USERPROFILE").as_deref(),
            std::env::var_os("ChocolateyInstall").as_deref(),
        ));
    }
    paths
}

#[cfg(any(unix, test))]
fn home_fallback_dirs(home: &Path, include_pnpm: bool) -> Vec<PathBuf> {
    let mut paths = vec![
        home.join(".local/bin"),
        home.join(".cargo/bin"),
        home.join(".pyenv/shims"),
        home.join(".asdf/shims"),
        home.join(".local/share/mise/shims"),
        home.join(".volta/bin"),
        home.join(".bun/bin"),
    ];
    if include_pnpm {
        paths.push(home.join("Library/pnpm"));
    }
    paths
}

#[cfg(any(windows, test))]
fn windows_fallback_dirs(
    program_files: Option<&OsStr>,
    local_app_data: Option<&OsStr>,
    app_data: Option<&OsStr>,
    user_profile: Option<&OsStr>,
    chocolatey: Option<&OsStr>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(root) = program_files.map(PathBuf::from) {
        paths.push(root.join("GitHub CLI"));
        paths.push(root.join("Microsoft SDKs/Azure/CLI2/wbin"));
    }
    if let Some(local) = local_app_data.map(PathBuf::from) {
        paths.push(local.join("Programs/GitHub CLI"));
        paths.push(local.join("Microsoft/WinGet/Links"));
    }
    if let Some(app_data) = app_data.map(PathBuf::from) {
        paths.push(app_data.join("npm"));
        paths.push(app_data.join("npm/node_modules/.bin"));
    }
    if let Some(profile) = user_profile.map(PathBuf::from) {
        paths.push(profile.join(".volta/bin"));
        paths.push(profile.join("scoop/shims"));
        paths.push(profile.join(".cloudflared"));
    }
    if let Some(chocolatey) = chocolatey.map(PathBuf::from) {
        paths.push(chocolatey.join("bin"));
    }
    paths
}

#[cfg(test)]
mod tests {
    use std::{
        convert::TryFrom,
        fs,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::config::model::CliPath;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "prmonitor-cli-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn executable(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn inputs(
        process: Option<&Path>,
        login: Option<&Path>,
        fallbacks: Vec<PathBuf>,
    ) -> ResolveInputs {
        ResolveInputs {
            process_path: process.map(|path| path.as_os_str().to_os_string()),
            login_shell_path: login.map(|path| path.as_os_str().to_os_string()),
            platform_fallbacks: fallbacks,
            path_extensions: path_extensions(),
        }
    }

    fn auto_tools() -> CliToolsConfig {
        CliToolsConfig::default()
    }

    #[test]
    fn cli_path_accepts_auto_and_absolute_but_rejects_relative() {
        assert!(CliPath::try_from(String::new())
            .expect("empty means auto")
            .is_auto());
        assert!(CliPath::try_from("relative/gh".to_string()).is_err());

        let absolute = if cfg!(windows) {
            r"C:\Program Files\GitHub CLI\gh.exe"
        } else {
            "/tmp/工具 path/gh"
        };
        assert_eq!(
            CliPath::try_from(absolute.to_string())
                .expect("absolute path")
                .as_str(),
            absolute
        );
        let wire =
            serde_json::to_string(&CliPath::try_from(absolute.to_string()).unwrap()).unwrap();
        let back: CliPath = serde_json::from_str(&wire).unwrap();
        assert_eq!(back.as_str(), absolute);
        assert!(serde_json::from_str::<CliPath>("\"relative/gh\"").is_err());
    }

    #[test]
    fn cli_tools_validate_canonical_basename_exhaustively() {
        let root = TestDir::new("canonical-basename");
        let mut tools = CliToolsConfig::default();
        let gh = root.executable(if cfg!(windows) { "gh.exe" } else { "gh" });
        tools.set_for_test(
            CliTool::Gh,
            CliPath::try_from(gh.to_string_lossy().into_owned()).unwrap(),
        );
        tools.validate().expect("canonical gh basename");

        let wrong = root.executable(if cfg!(windows) {
            "github.exe"
        } else {
            "github"
        });
        tools.set_for_test(
            CliTool::Gh,
            CliPath::try_from(wrong.to_string_lossy().into_owned()).unwrap(),
        );
        assert!(tools.validate().is_err());
    }

    #[test]
    fn resolution_order_is_process_then_login_then_fallback() {
        let process = TestDir::new("process");
        let login = TestDir::new("login");
        let fallback = TestDir::new("fallback");
        let process_gh = process.executable("gh");
        login.executable("gh");
        fallback.executable("gh");

        let resolved = resolve_with_inputs(
            &auto_tools(),
            CliTool::Gh,
            &inputs(Some(&process.0), Some(&login.0), vec![fallback.0.clone()]),
        )
        .unwrap();
        assert_eq!(resolved.program, process_gh);
        assert_eq!(resolved.source, CliResolutionSource::ProcessPath);

        let without_process = resolve_with_inputs(
            &auto_tools(),
            CliTool::Gh,
            &inputs(None, Some(&login.0), vec![fallback.0.clone()]),
        )
        .unwrap();
        assert_eq!(without_process.source, CliResolutionSource::LoginShellPath);
    }

    #[test]
    fn missing_path_uses_platform_fallback() {
        let fallback = TestDir::new("only-fallback");
        let gh = fallback.executable("gh");
        let resolved = resolve_with_inputs(
            &auto_tools(),
            CliTool::Gh,
            &inputs(None, None, vec![fallback.0.clone()]),
        )
        .unwrap();
        assert_eq!(resolved.program, gh);
        assert_eq!(resolved.source, CliResolutionSource::PlatformFallback);
    }

    #[test]
    fn relative_and_empty_path_segments_never_enter_discovery_or_child_path() {
        let process = TestDir::new("absolute-process");
        let gh = process.executable("gh");
        let raw_path = std::env::join_paths([
            PathBuf::new(),
            PathBuf::from("relative-bin"),
            process.0.clone(),
        ])
        .unwrap();
        let resolved = resolve_with_inputs(
            &auto_tools(),
            CliTool::Gh,
            &ResolveInputs {
                process_path: Some(raw_path),
                login_shell_path: None,
                platform_fallbacks: Vec::new(),
                path_extensions: path_extensions(),
            },
        )
        .unwrap();

        assert_eq!(resolved.program, gh);
        assert!(resolved.program.is_absolute());
        assert!(find_on_path(
            Some(OsStr::new("relative-bin")),
            CliTool::Gh,
            &path_extensions()
        )
        .is_none());
        let command = resolved.command();
        let child_path = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("PATH"))
            .and_then(|(_, value)| value)
            .unwrap();
        assert!(std::env::split_paths(child_path).all(|directory| directory.is_absolute()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolved_program_is_stable_when_child_current_directory_changes() {
        let process = TestDir::new("stable-program");
        let other = TestDir::new("other-current-dir");
        let gh = process.executable("gh");
        let resolved = resolve_with_inputs(
            &auto_tools(),
            CliTool::Gh,
            &inputs(Some(&process.0), None, Vec::new()),
        )
        .unwrap();

        let status = resolved
            .command()
            .current_dir(&other.0)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        assert_eq!(resolved.program, gh);
    }

    #[test]
    fn selected_tool_validation_is_isolated_while_full_config_validation_remains_strict() {
        let root = TestDir::new("isolated-validation");
        let gh = root.executable("gh");
        let tools = CliToolsConfig {
            gh_path: CliPath::try_from(gh.to_string_lossy().into_owned()).unwrap(),
            az_path: CliPath::try_from(root.0.join("not-az").to_string_lossy().into_owned())
                .unwrap(),
            ..CliToolsConfig::default()
        };

        let gh_result = resolve_with_inputs(&tools, CliTool::Gh, &inputs(None, None, Vec::new()));
        assert!(gh_result.is_ok(), "invalid az row must not hide gh status");
        assert!(resolve_with_inputs(&tools, CliTool::Az, &inputs(None, None, Vec::new())).is_err());
        assert!(tools.validate().is_err(), "saving still validates all rows");
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_is_rejected_and_executable_symlink_is_followed() {
        use std::os::unix::{fs::symlink, fs::PermissionsExt};

        let directory = TestDir::new("permissions");
        let target = directory.executable("real-gh");
        let link = directory.0.join("gh");
        symlink(&target, &link).unwrap();
        let found = resolve_with_inputs(
            &auto_tools(),
            CliTool::Gh,
            &inputs(Some(&directory.0), None, Vec::new()),
        )
        .unwrap();
        assert_eq!(found.program, link);

        fs::remove_file(&link).unwrap();
        let plain = directory.executable("gh");
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(resolve_with_inputs(
            &auto_tools(),
            CliTool::Gh,
            &inputs(Some(&directory.0), None, Vec::new()),
        )
        .is_err());
    }

    #[test]
    fn custom_path_is_strict_and_command_carries_program_without_trusting_its_parent() {
        let custom = TestDir::new("custom path 工具");
        let process = TestDir::new("duplicate");
        let program = custom.executable("codex");
        let configured = CliPath::try_from(program.to_string_lossy().into_owned()).unwrap();
        let mut tools = CliToolsConfig {
            codex_path: configured,
            ..CliToolsConfig::default()
        };
        let duplicate_path = std::env::join_paths([&process.0, &process.0]).unwrap();
        let resolved = resolve_with_inputs(
            &tools,
            CliTool::Codex,
            &ResolveInputs {
                process_path: Some(duplicate_path),
                login_shell_path: Some(process.0.as_os_str().to_os_string()),
                platform_fallbacks: Vec::new(),
                path_extensions: path_extensions(),
            },
        )
        .unwrap();
        let command = resolved.command();
        assert_eq!(command.as_std().get_program(), program.as_os_str());
        let command_path = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("PATH"))
            .and_then(|(_, value)| value)
            .expect("PATH injected");
        let directories = std::env::split_paths(command_path).collect::<Vec<_>>();
        assert_eq!(directories[0], process.0);
        assert!(
            !directories.contains(&custom.0),
            "an absolute executable does not require its user-controlled parent on PATH"
        );
        assert_eq!(
            directories
                .iter()
                .filter(|path| **path == process.0)
                .count(),
            1
        );

        let missing = custom.0.join("codex-missing");
        tools.codex_path = CliPath::try_from(missing.to_string_lossy().into_owned()).unwrap();
        assert!(resolve_with_inputs(
            &tools,
            CliTool::Codex,
            &inputs(Some(&process.0), None, Vec::new()),
        )
        .is_err());
    }

    #[test]
    fn enhanced_path_excludes_untrusted_configured_cli_parents_and_fingerprint_ignores_source() {
        let gh_dir = TestDir::new("configured-gh");
        let codex_dir = TestDir::new("configured-codex");
        let gh = gh_dir.executable("gh");
        let codex = codex_dir.executable("codex");
        let tools = CliToolsConfig {
            gh_path: CliPath::try_from(gh.to_string_lossy().into_owned()).unwrap(),
            codex_path: CliPath::try_from(codex.to_string_lossy().into_owned()).unwrap(),
            ..CliToolsConfig::default()
        };
        let resolved =
            resolve_with_inputs(&tools, CliTool::Codex, &inputs(None, None, Vec::new())).unwrap();
        let command = resolved.command();
        let path = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("PATH"))
            .and_then(|(_, value)| value)
            .unwrap();
        let directories = std::env::split_paths(path).collect::<Vec<_>>();
        assert!(!directories.contains(&gh_dir.0));
        assert!(!directories.contains(&codex_dir.0));

        let same_path_other_source = super::resolved(
            resolved.program.clone(),
            CliResolutionSource::ProcessPath,
            resolved.enhanced_path.clone(),
        );
        assert_eq!(resolved.fingerprint(), same_path_other_source.fingerprint());
    }

    #[test]
    fn marked_shell_output_parser_ignores_noise_and_cache_refreshes_explicitly() {
        let parsed = parse_marked_path(
            b"shell banner\n__PRMONITOR_PATH_BEGIN__/opt/bin:/usr/bin__PRMONITOR_PATH_END__\nprompt",
        )
        .unwrap();
        assert_eq!(parsed, OsString::from("/opt/bin:/usr/bin"));
        assert!(parse_marked_path(b"noise only").is_none());

        let resolver = CliResolver::default();
        let calls = AtomicUsize::new(0);
        let first = resolver.cached_login_shell_path_with(false, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(OsString::from("/first"))
        });
        let cached = resolver.cached_login_shell_path_with(false, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(OsString::from("/ignored"))
        });
        let refreshed = resolver.cached_login_shell_path_with(true, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(OsString::from("/second"))
        });
        assert_eq!(first, Some(OsString::from("/first")));
        assert_eq!(cached, first);
        assert_eq!(refreshed, Some(OsString::from("/second")));
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let reentrant = CliResolver::default();
        let outer = reentrant.cached_login_shell_path_with(false, || {
            let inner =
                reentrant.cached_login_shell_path_with(false, || Some(OsString::from("/inner")));
            assert_eq!(inner, Some(OsString::from("/inner")));
            Some(OsString::from("/outer"))
        });
        assert_eq!(outer, Some(OsString::from("/inner")));
    }

    #[test]
    fn batch_refresh_happens_before_invalid_first_tool_validation() {
        let old = TestDir::new("stale-login-path");
        let fresh = TestDir::new("fresh-login-path");
        fresh.executable(if cfg!(windows) { "az.exe" } else { "az" });
        let invalid_gh = fresh.0.join(if cfg!(windows) { "gh.exe" } else { "gh" });
        let tools = CliToolsConfig {
            gh_path: CliPath::try_from(invalid_gh.to_string_lossy().into_owned()).unwrap(),
            ..CliToolsConfig::default()
        };
        let resolver = CliResolver::default();
        resolver.cached_login_shell_path_with(false, || Some(old.0.as_os_str().to_os_string()));

        let rows = resolver
            .diagnostics_all_with(&tools, true, || Some(fresh.0.as_os_str().to_os_string()));
        let gh = &rows[CliTool::Gh as usize];
        assert!(gh.resolution.is_err());
        let az_row = &rows[CliTool::Az as usize];
        az_row
            .resolution
            .as_ref()
            .expect("invalid gh does not abort sibling diagnostics");
        assert_eq!(
            resolver.cached_login_shell_path_with(false, || None),
            Some(fresh.0.as_os_str().to_os_string()),
            "batch refresh must occur before validating the invalid first row"
        );
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_timeout_covers_descendants_holding_stdout_open() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDir::new("shell-descendant-pipe");
        let shell = directory.0.join("fake-shell");
        fs::write(
            &shell,
            b"#!/bin/sh\n(sleep 5) &\nprintf '__PRMONITOR_PATH_BEGIN__/safe/bin__PRMONITOR_PATH_END__'\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();

        let started = Instant::now();
        assert!(discover_login_shell_path_from(&shell, Duration::from_millis(150)).is_none());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "pipe reader must remain inside the wall timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_process_path_and_shell_env_uses_account_login_shell() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDir::new("account-login-shell");
        let bin = directory.0.join("custom-bin");
        fs::create_dir_all(&bin).unwrap();
        let gh = bin.join("gh");
        fs::write(&gh, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        let shell = directory.0.join("account-shell");
        fs::write(
            &shell,
            format!(
                "#!/bin/sh\nprintf '__PRMONITOR_PATH_BEGIN__{}__PRMONITOR_PATH_END__'\n",
                bin.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();

        let selected = select_login_shell(None, Some(shell.into_os_string()))
            .expect("account database shell is used when SHELL is absent");
        let login_path = discover_login_shell_path_from(&selected, Duration::from_secs(1))
            .expect("account shell yields PATH");
        let resolved = resolve_with_inputs(
            &auto_tools(),
            CliTool::Gh,
            &ResolveInputs {
                process_path: None,
                login_shell_path: Some(login_path),
                platform_fallbacks: Vec::new(),
                path_extensions: Vec::new(),
            },
        )
        .expect("CLI resolves despite missing process PATH and SHELL");

        assert_eq!(resolved.program, gh);
        assert_eq!(resolved.source, CliResolutionSource::LoginShellPath);
    }

    #[cfg(windows)]
    #[test]
    fn windows_pathext_order_and_case_select_the_expected_program() {
        let directory = TestDir::new("windows-pathext");
        let cmd = directory.executable("gh.CMD");
        directory.executable("gh.EXE");
        let extensions = vec![OsString::from(".CMD"), OsString::from(".EXE")];

        assert_eq!(
            find_on_path(Some(directory.0.as_os_str()), CliTool::Gh, &extensions),
            Some(cmd)
        );

        let upper = directory.executable("GH.BAT");
        let tools = CliToolsConfig {
            gh_path: CliPath::try_from(upper.to_string_lossy().into_owned()).unwrap(),
            ..CliToolsConfig::default()
        };
        let resolved =
            resolve_with_inputs(&tools, CliTool::Gh, &inputs(None, None, Vec::new())).unwrap();
        assert_eq!(resolved.program, upper);
    }

    #[test]
    fn windows_pathext_parser_keeps_only_directly_launchable_types() {
        assert_eq!(
            parse_windows_path_extensions(OsStr::new(".PS1;.CMD;.js;.EXE;.BAT;.COM")),
            [".cmd", ".exe", ".bat", ".com"].map(OsString::from)
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_custom_path_rejects_extensionless_shim() {
        let directory = TestDir::new("windows-custom-extensionless");
        let shim = directory.executable("codex");
        let tools = CliToolsConfig {
            codex_path: CliPath::try_from(shim.to_string_lossy().into_owned()).unwrap(),
            ..CliToolsConfig::default()
        };

        assert!(tools.validate_tool(CliTool::Codex).is_err());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_ignores_extensionless_npm_shim_and_spawns_cmd_candidate() {
        let directory = TestDir::new("windows-npm-shim");
        fs::write(directory.0.join("codex"), b"extensionless shim").unwrap();
        let cmd = directory.0.join("codex.cmd");
        fs::write(
            &cmd,
            b"@echo off\r\nif \"%1\"==\"probe\" (echo selected-cmd& exit /b 0)\r\nexit /b 9\r\n",
        )
        .unwrap();
        let inputs = ResolveInputs {
            process_path: Some(directory.0.as_os_str().to_os_string()),
            login_shell_path: None,
            platform_fallbacks: Vec::new(),
            path_extensions: vec![OsString::from(".cmd")],
        };

        let resolved = resolve_with_inputs(&auto_tools(), CliTool::Codex, &inputs).unwrap();
        assert_eq!(resolved.program, cmd);
        let output = resolved.command().arg("probe").output().await.unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("selected-cmd"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_merge_accepts_drive_and_unc_and_deduplicates_case_insensitively() {
        let first = OsStr::new(r"C:\Tools;relative;;c:\tools;\\server\share\bin");
        let merged = merge_paths([Some(first)]);
        let directories = std::env::split_paths(&merged).collect::<Vec<_>>();

        assert_eq!(
            directories,
            [
                PathBuf::from(r"C:\Tools"),
                PathBuf::from(r"\\server\share\bin")
            ]
        );
        assert!(directories.iter().all(|directory| directory.is_absolute()));
    }

    #[test]
    fn platform_fallback_candidate_builders_cover_version_managers_and_windows_links() {
        let home = Path::new("/home/tester");
        let unix = home_fallback_dirs(home, true);
        for suffix in [
            ".local/bin",
            ".cargo/bin",
            ".pyenv/shims",
            ".asdf/shims",
            ".local/share/mise/shims",
            ".volta/bin",
            ".bun/bin",
            "Library/pnpm",
        ] {
            assert!(unix.contains(&home.join(suffix)));
        }

        let windows = windows_fallback_dirs(
            Some(OsStr::new(r"C:\Program Files")),
            Some(OsStr::new(r"C:\Users\u\AppData\Local")),
            Some(OsStr::new(r"C:\Users\u\AppData\Roaming")),
            Some(OsStr::new(r"C:\Users\u")),
            Some(OsStr::new(r"C:\ProgramData\chocolatey")),
        );
        for suffix in [
            "Microsoft/WinGet/Links",
            "npm",
            ".volta/bin",
            "scoop/shims",
            ".cloudflared",
            "bin",
        ] {
            assert!(
                windows.iter().any(|path| path.ends_with(suffix)),
                "{suffix}"
            );
        }
    }
}
