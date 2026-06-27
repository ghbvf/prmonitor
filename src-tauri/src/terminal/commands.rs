//! Terminal slice Tauri commands (#1383).
//!
//! Each ACTION command (list / create / attach / detach / input / resize) `resume()`s the
//! daemon (a user terminal action is explicit intent to run — overriding a prior
//! `stop_terminal_daemon`, mirroring how a manual review `resume()`s a user-stopped codex),
//! resolves the bundled daemon script, builds an [`ITermBackend`], and delegates. The two
//! lifecycle commands (`get_terminal_status` passive probe, `stop_terminal_daemon` sync) do
//! NOT resume — the probe honors a user stop, and `stop` sets it.

use tauri::path::BaseDirectory;
use tauri::Manager;

use super::backend::TerminalBackend;
use super::iterm::ITermBackend;
use super::process::TerminalDaemonStatus;
use super::webpty::WebPtyBackend;
use crate::error::{AppError, AppResult};
use crate::model::{CreateSessionOpts, TerminalBackendKind, TerminalSession};
use crate::state::AppState;

/// The python interpreter that runs the daemon (PATH-resolved). Hardcoded — the slice
/// deliberately does NOT read `config` for it (keeping `terminal` free of any cross-slice
/// config dependency, per the slice-boundary charter); a future configurable interpreter is a
/// separate decision.
const PYTHON_BIN: &str = "python3";

/// Resolve the bundled daemon script to an absolute path. In a packaged app it lives under
/// the resource dir (declared in `tauri.conf.json` `bundle.resources`); `BaseDirectory::Resource`
/// resolves the same relative path tauri copied it to.
pub(crate) fn resolve_script_path<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> AppResult<String> {
    let path = app
        .path()
        .resolve(
            "resources/iterm-daemon/iterm_daemon.py",
            BaseDirectory::Resource,
        )
        .map_err(|e| AppError::new(format!("无法定位 iTerm daemon 脚本: {e}")))?;
    Ok(path.to_string_lossy().into_owned())
}

/// The backend ROUTING hub (#1372) — the **Hard carrier** for the second backend. Each session is
/// owned by exactly one backend; this enum dispatches a per-session op to its owner via an
/// exhaustive per-method `match`, so adding a third [`TerminalBackendKind`] variant without a
/// `RoutedBackend` arm is a compile error (the missing arm cannot be expressed). Mirrors the
/// `ReviewEngine`/`EventSourceProvider` seam style: a new backend is a new arm, not a changed call
/// site.
enum RoutedBackend<'a, R: tauri::Runtime> {
    Iterm(ITermBackend<'a, R>),
    WebPty(WebPtyBackend<'a, R>),
}

impl<R: tauri::Runtime> TerminalBackend for RoutedBackend<'_, R> {
    /// ARCH-1: present ONLY to satisfy the trait — NEVER called through `RoutedBackend`. The `list`
    /// command (`list_terminal_sessions_inner`) MERGES both backends directly; routing
    /// `list_sessions` through one arm would return only ONE backend's sessions (a footgun for a
    /// future caller). Use `list_terminal_sessions_inner`, not this.
    async fn list_sessions(&self) -> AppResult<Vec<TerminalSession>> {
        match self {
            RoutedBackend::Iterm(b) => b.list_sessions().await,
            RoutedBackend::WebPty(b) => b.list_sessions().await,
        }
    }
    /// ARCH-1: present ONLY to satisfy the trait — NEVER called through `RoutedBackend`. Create
    /// routes per-backend directly in `create_terminal_session_inner` (it matches `opts.backend`,
    /// not a session id, so there is no owner to route to yet).
    async fn create_session(&self, opts: CreateSessionOpts) -> AppResult<TerminalSession> {
        match self {
            RoutedBackend::Iterm(b) => b.create_session(opts).await,
            RoutedBackend::WebPty(b) => b.create_session(opts).await,
        }
    }
    async fn send_text(&self, session_id: &str, text: &str) -> AppResult<()> {
        match self {
            RoutedBackend::Iterm(b) => b.send_text(session_id, text).await,
            RoutedBackend::WebPty(b) => b.send_text(session_id, text).await,
        }
    }
    async fn subscribe(&self, session_id: &str) -> AppResult<()> {
        match self {
            RoutedBackend::Iterm(b) => b.subscribe(session_id).await,
            RoutedBackend::WebPty(b) => b.subscribe(session_id).await,
        }
    }
    async fn unsubscribe(&self, session_id: &str) -> AppResult<()> {
        match self {
            RoutedBackend::Iterm(b) => b.unsubscribe(session_id).await,
            RoutedBackend::WebPty(b) => b.unsubscribe(session_id).await,
        }
    }
    async fn resize(&self, session_id: &str, cols: u16, rows: u16) -> AppResult<()> {
        match self {
            RoutedBackend::Iterm(b) => b.resize(session_id, cols, rows).await,
            RoutedBackend::WebPty(b) => b.resize(session_id, cols, rows).await,
        }
    }
    async fn close_session(&self, session_id: &str) -> AppResult<()> {
        match self {
            RoutedBackend::Iterm(b) => b.close_session(session_id).await,
            RoutedBackend::WebPty(b) => b.close_session(session_id).await,
        }
    }
}

/// Build the iTerm backend handle (borrows the resident daemon + the resolved daemon script).
fn iterm_backend<'a, R: tauri::Runtime>(
    app: &'a tauri::AppHandle<R>,
    state: &'a AppState,
    script: &'a str,
) -> ITermBackend<'a, R> {
    ITermBackend::new(app, &state.terminal, PYTHON_BIN, script)
}

/// Build the Web PTY backend handle (borrows the resident PTY pool).
fn webpty_backend<'a, R: tauri::Runtime>(
    app: &'a tauri::AppHandle<R>,
    state: &'a AppState,
) -> WebPtyBackend<'a, R> {
    WebPtyBackend::new(app, &state.web_pty)
}

/// Map the WebPty ownership predicate to the owning [`TerminalBackendKind`] (#1372). Pure (takes
/// `state.web_pty.owns(id)`'s result), so it's unit-testable without a live app: a PTY-registry hit
/// → `WebPty`, a miss → `Iterm` (iTerm session GUIDs aren't in the PTY registry).
fn resolve_owner(is_pty: bool) -> TerminalBackendKind {
    if is_pty {
        TerminalBackendKind::WebPty
    } else {
        TerminalBackendKind::Iterm
    }
}

/// A resolved per-session route: which backend owns the id, plus (iTerm only) the resolved daemon
/// script the `ITermBackend` borrows. Resolving the script + `resume()`ing the iTerm daemon happens
/// ONLY on the iTerm arm — a pure-PTY op must not revive the iTerm daemon.
enum SessionRoute {
    Iterm(String),
    WebPty,
}

/// Resolve a session's route from the live PTY registry. iTerm arm: `resume()` (a user action
/// overrides a prior stop) + resolve the bundled script. WebPty arm: nothing — the registry owns it.
fn route_session<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    session_id: &str,
) -> AppResult<SessionRoute> {
    match resolve_owner(state.web_pty.owns(session_id)) {
        TerminalBackendKind::Iterm => {
            state.terminal.resume();
            Ok(SessionRoute::Iterm(resolve_script_path(app)?))
        }
        TerminalBackendKind::WebPty => Ok(SessionRoute::WebPty),
    }
}

/// Build the routing-hub backend for a resolved route (the `script` in the iTerm arm outlives the
/// borrow because [`SessionRoute`] owns it).
fn routed_for<'a, R: tauri::Runtime>(
    app: &'a tauri::AppHandle<R>,
    state: &'a AppState,
    route: &'a SessionRoute,
) -> RoutedBackend<'a, R> {
    match route {
        SessionRoute::Iterm(script) => RoutedBackend::Iterm(iterm_backend(app, state, script)),
        SessionRoute::WebPty => RoutedBackend::WebPty(webpty_backend(app, state)),
    }
}

/// List EVERY session across both backends (#1372). The PTY pool always contributes (its `list`
/// can't fail); the iTerm daemon is best-effort. If iTerm errors but the PTY pool is non-empty,
/// return just the PTY rows + log; propagate the iTerm error ONLY when BOTH are empty/failed.
pub(crate) async fn list_terminal_sessions_inner<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
) -> AppResult<Vec<TerminalSession>> {
    let pty_sessions = state.web_pty.list();
    // PROD-1: do NOT `resume()` here. Listing must not revive a user-stopped iTerm daemon (that
    // would override an explicit stop + spuriously spawn python for PTY-only / Windows users). The
    // iTerm enumeration stays best-effort — if the daemon is stopped/absent it errors and we fall
    // back to the PTY rows. A per-session action still `resume()`s in its iTerm arm.
    let iterm_result = match resolve_script_path(app) {
        Ok(script) => iterm_backend(app, state, &script).list_sessions().await,
        Err(e) => Err(e),
    };
    match iterm_result {
        Ok(mut sessions) => {
            sessions.extend(pty_sessions);
            Ok(sessions)
        }
        Err(e) if pty_sessions.is_empty() => Err(e), // both failed/empty → surface the iTerm error.
        Err(e) => {
            eprintln!("iTerm 会话列举失败，仅返回 WebPty 会话: {}", e.message);
            Ok(pty_sessions)
        }
    }
}

#[tauri::command]
pub async fn list_terminal_sessions<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> AppResult<Vec<TerminalSession>> {
    list_terminal_sessions_inner(&app, &state).await
}

pub(crate) async fn create_terminal_session_inner<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    opts: CreateSessionOpts,
) -> AppResult<TerminalSession> {
    // Create-time backend pick (**Hard**: exhaustive over the sealed kind). `None` → `Iterm` (the
    // pre-#1372 default). Opts validation is BACKEND-AWARE (F5): `window_id`/`profile` are iTerm
    // concepts — the iTerm arm validates + uses them; the WebPty arm REJECTS them (a shell takes
    // neither) instead of silently ignoring, and must NOT touch the iTerm daemon (no `resume`/script).
    match opts.backend.unwrap_or_default() {
        TerminalBackendKind::Iterm => {
            if let Some(window_id) = opts.window_id.as_deref() {
                check_identifier(window_id)?;
            }
            if let Some(profile) = opts.profile.as_deref() {
                check_identifier(profile)?;
            }
            state.terminal.resume();
            let script = resolve_script_path(app)?;
            iterm_backend(app, state, &script)
                .create_session(opts)
                .await
        }
        TerminalBackendKind::WebPty => {
            if opts.window_id.is_some() || opts.profile.is_some() {
                return Err(AppError::new(
                    "WebPty shell 会话不支持 window/profile 参数".to_string(),
                ));
            }
            webpty_backend(app, state).create_session(opts).await
        }
    }
}

#[tauri::command]
pub async fn create_terminal_session<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    opts: CreateSessionOpts,
) -> AppResult<TerminalSession> {
    create_terminal_session_inner(&app, &state, opts).await
}

/// Attach the panel to a session = start streaming its screen (daemon `subscribe`). The
/// backend emits the one-shot `Attached` then the daemon's `screenUpdate` stream follows.
pub(crate) async fn attach_terminal_inner<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    session_id: &str,
) -> AppResult<()> {
    check_identifier(session_id)?;
    let route = route_session(app, state, session_id)?;
    routed_for(app, state, &route).subscribe(session_id).await
}

#[tauri::command]
pub async fn attach_terminal<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> AppResult<()> {
    attach_terminal_inner(&app, &state, &session_id).await
}

/// Detach the panel from a session = stop streaming its screen (daemon `unsubscribe`).
pub(crate) async fn detach_terminal_inner<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    session_id: &str,
) -> AppResult<()> {
    check_identifier(session_id)?;
    let route = route_session(app, state, session_id)?;
    routed_for(app, state, &route).unsubscribe(session_id).await
}

#[tauri::command]
pub async fn detach_terminal<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> AppResult<()> {
    detach_terminal_inner(&app, &state, &session_id).await
}

/// Upper bound on a single `send_terminal_input` payload (64 KiB, byte length). A keystroke /
/// paste is tiny; this caps a malformed / oversized `data` before it reaches the daemon
/// (loopback-only DoS hardening — would be P2 if the IPC listener were ever exposed).
const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Reject an oversized `send_terminal_input` payload with an actionable error. Pure free helper
/// (no Tauri `State`) so it's unit-testable without a live app — the command calls it first.
pub(crate) fn check_input_size(data: &str) -> AppResult<()> {
    if data.len() > MAX_INPUT_BYTES {
        return Err(AppError::new(format!(
            "终端输入过大（{} 字节，超过 {} 字节上限）：请减少单次粘贴/输入的内容",
            data.len(),
            MAX_INPUT_BYTES
        )));
    }
    Ok(())
}

/// Upper bound on an identifier (`session_id` / `window_id` / `profile`) in bytes. iTerm
/// session GUIDs vary in shape, so the guard is deliberately NON-EMPTY + length-cap ONLY —
/// no character-set whitelist (that would risk rejecting valid IDs). A Medium runtime guard
/// (per `ai-robust.md`) catching an empty / oversized identifier at the command boundary
/// before it reaches the daemon (loopback-only hardening — the IPC listener isn't exposed).
const MAX_ID_BYTES: usize = 256;

/// Cap on how many bytes of an identifier an error message echoes — an oversized id can't
/// bloat the error string.
const ID_ECHO_BYTES: usize = 80;

/// Truncate an identifier for safe echoing in an error message, never splitting a UTF-8
/// sequence (slices on a char boundary).
fn truncate_id(id: &str) -> String {
    if id.len() <= ID_ECHO_BYTES {
        return id.to_string();
    }
    let mut end = ID_ECHO_BYTES;
    while end > 0 && !id.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &id[..end])
}

/// Reject an empty or over-long identifier with an actionable error. Pure free helper
/// (no Tauri `State`) so it's unit-testable; the action commands call it first on their
/// `session_id` (and `create` on the optional `window_id` / `profile`).
pub(crate) fn check_identifier(id: &str) -> AppResult<()> {
    if id.is_empty() {
        return Err(AppError::new(
            "标识符为空：session / window / profile 标识符不能为空".to_string(),
        ));
    }
    if id.len() > MAX_ID_BYTES {
        return Err(AppError::new(format!(
            "标识符过长（{} 字节，超过 {} 字节上限）：{}",
            id.len(),
            MAX_ID_BYTES,
            truncate_id(id)
        )));
    }
    Ok(())
}

/// Whitelist range for a terminal grid. The daemon already rejects `cols/rows <= 0`, but a
/// Rust-layer range guard (Medium runtime guard per `ai-robust.md`) caps an absurd grid at
/// the command boundary before it reaches the IPC — defense in depth.
const MIN_GRID: u16 = 1;
const MAX_COLS: u16 = 1000;
const MAX_ROWS: u16 = 1000;

/// Reject an out-of-range terminal grid with an actionable error. Pure free helper
/// (no Tauri `State`) so it's unit-testable; `resize_terminal` calls it first.
pub(crate) fn check_grid(cols: u16, rows: u16) -> AppResult<()> {
    if !(MIN_GRID..=MAX_COLS).contains(&cols) || !(MIN_GRID..=MAX_ROWS).contains(&rows) {
        return Err(AppError::new(format!(
            "非法终端网格尺寸（cols={cols} rows={rows}）：列需在 {MIN_GRID}..={MAX_COLS}、行需在 {MIN_GRID}..={MAX_ROWS} 之间"
        )));
    }
    Ok(())
}

/// Forward keystrokes / pasted text into a session (daemon `sendText`).
pub(crate) async fn send_terminal_input_inner<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    session_id: &str,
    data: &str,
) -> AppResult<()> {
    check_identifier(session_id)?;
    check_input_size(data)?;
    let route = route_session(app, state, session_id)?;
    routed_for(app, state, &route)
        .send_text(session_id, data)
        .await
}

#[tauri::command]
pub async fn send_terminal_input<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    session_id: String,
    data: String,
) -> AppResult<()> {
    send_terminal_input_inner(&app, &state, &session_id, &data).await
}

/// Resize a session's grid (daemon `resize`), e.g. after an xterm fit.
pub(crate) async fn resize_terminal_inner<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    session_id: &str,
    cols: u16,
    rows: u16,
) -> AppResult<()> {
    check_identifier(session_id)?;
    check_grid(cols, rows)?;
    let route = route_session(app, state, session_id)?;
    routed_for(app, state, &route)
        .resize(session_id, cols, rows)
        .await
}

#[tauri::command]
pub async fn resize_terminal<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    session_id: String,
    cols: u16,
    rows: u16,
) -> AppResult<()> {
    resize_terminal_inner(&app, &state, &session_id, cols, rows).await
}

/// Stop a session's process (#1372). Routes to the owning backend: the WebPty backend SIGKILLs +
/// reaps the shell child and emits `SessionEnded`; the iTerm backend returns an actionable error
/// (it has no per-session process to kill — close the tab/window in iTerm instead). UNLIKE
/// `detach` (which leaves the session running), this terminates it.
pub(crate) async fn close_terminal_session_inner<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    session_id: &str,
) -> AppResult<()> {
    check_identifier(session_id)?;
    let route = route_session(app, state, session_id)?;
    routed_for(app, state, &route)
        .close_session(session_id)
        .await
}

#[tauri::command]
pub async fn close_terminal_session<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> AppResult<()> {
    close_terminal_session_inner(&app, &state, &session_id).await
}

/// Probe the daemon for the StatusBar (lazy start: first call spawns + handshakes, later
/// calls reuse). PASSIVE: honors a prior `stop_terminal_daemon` (does NOT resume), never
/// errors (failures map to a status struct), mirroring `get_codex_status`.
#[tauri::command]
pub async fn get_terminal_status<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> AppResult<TerminalDaemonStatus> {
    Ok(get_terminal_status_inner(&app, &state).await)
}

pub(crate) async fn get_terminal_status_inner<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
) -> TerminalDaemonStatus {
    match resolve_script_path(app) {
        Ok(script) => state.terminal.status(app, PYTHON_BIN, &script).await,
        Err(e) => TerminalDaemonStatus {
            available: false,
            desired_running: true,
            message: e.message,
        },
    }
}

/// Explicitly stop the resident daemon (sync, like `stop_codex`): sets the user-stop flag +
/// kills the child. Passive `get_terminal_status` then reports stopped without reviving; an
/// action command (which `resume()`s) still force-starts. Goes through `AppResult` for funnel
/// consistency (`stop` can't fail, so it is always `Ok`).
#[tauri::command]
pub fn stop_terminal_daemon(state: tauri::State<'_, AppState>) -> AppResult<TerminalDaemonStatus> {
    Ok(stop_terminal_daemon_inner(&state))
}

pub(crate) fn stop_terminal_daemon_inner(state: &AppState) -> TerminalDaemonStatus {
    state.terminal.stop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_input_size_accepts_within_bound() {
        assert!(check_input_size("ls\n").is_ok());
        // Exactly at the cap is allowed (the guard rejects only `> MAX_INPUT_BYTES`).
        assert!(check_input_size(&"x".repeat(MAX_INPUT_BYTES)).is_ok());
    }

    #[test]
    fn check_input_size_rejects_over_bound() {
        let too_big = "x".repeat(MAX_INPUT_BYTES + 1);
        let err = check_input_size(&too_big).expect_err("over-bound input must be rejected");
        assert!(
            err.message.contains("过大"),
            "error message should be actionable, got: {}",
            err.message
        );
    }

    #[test]
    fn check_identifier_accepts_normal() {
        assert!(check_identifier("w0t0p0").is_ok());
        // iTerm GUID-shaped ids (with dashes) are accepted — no character-set whitelist.
        assert!(check_identifier("8E37D9C2-4F1A-4B0E-9C3D-1A2B3C4D5E6F").is_ok());
        // Exactly at the cap is allowed (the guard rejects only `> MAX_ID_BYTES`).
        assert!(check_identifier(&"a".repeat(MAX_ID_BYTES)).is_ok());
    }

    #[test]
    fn check_identifier_rejects_empty() {
        let err = check_identifier("").expect_err("empty id must be rejected");
        assert!(
            err.message.contains("空"),
            "error message should be actionable, got: {}",
            err.message
        );
    }

    #[test]
    fn check_identifier_rejects_over_length() {
        let too_long = "a".repeat(MAX_ID_BYTES + 1);
        let err = check_identifier(&too_long).expect_err("over-length id must be rejected");
        assert!(
            err.message.contains("过长"),
            "error message should be actionable, got: {}",
            err.message
        );
        // The echoed id is truncated, not the full oversized string.
        assert!(
            err.message.len() < too_long.len(),
            "error must truncate the echoed id, got len {}",
            err.message.len()
        );
    }

    #[test]
    fn check_grid_accepts_in_range() {
        assert!(check_grid(80, 24).is_ok());
        // Boundaries are inclusive.
        assert!(check_grid(MIN_GRID, MIN_GRID).is_ok());
        assert!(check_grid(MAX_COLS, MAX_ROWS).is_ok());
    }

    #[test]
    fn check_grid_rejects_zero() {
        assert!(check_grid(0, 24).is_err());
        assert!(check_grid(80, 0).is_err());
        let err = check_grid(0, 0).expect_err("zero grid must be rejected");
        assert!(
            err.message.contains("非法"),
            "error message should be actionable, got: {}",
            err.message
        );
    }

    #[test]
    fn check_grid_rejects_oversized() {
        assert!(check_grid(MAX_COLS + 1, 24).is_err());
        assert!(check_grid(80, MAX_ROWS + 1).is_err());
    }

    // #1372: the routing predicate is pure — a PTY-registry hit routes to WebPty, a miss to the
    // iTerm default. (The exhaustive `match` in `RoutedBackend` is the Hard carrier; this pins the
    // predicate that selects the arm.)
    #[test]
    fn resolve_owner_maps_pty_ownership_to_kind() {
        assert_eq!(resolve_owner(true), TerminalBackendKind::WebPty);
        assert_eq!(resolve_owner(false), TerminalBackendKind::Iterm);
    }

    // A fresh PTY registry owns nothing, so an unknown id routes to the iTerm default.
    #[test]
    fn unknown_session_routes_to_iterm_by_default() {
        let pool = crate::terminal::webpty_manager::WebPtyManager::default();
        assert_eq!(resolve_owner(pool.owns("nope")), TerminalBackendKind::Iterm);
    }

    // F5: a WebPty create with iTerm-only `window_id` / `profile` opts is REJECTED (not silently
    // ignored). The reject fires before any PTY spawn, so this needs no managed AppState / live app.
    #[tokio::test]
    async fn webpty_create_rejects_window_and_profile() {
        let app = tauri::test::mock_app();
        let state = AppState::default();
        let err = create_terminal_session_inner(
            app.handle(),
            &state,
            CreateSessionOpts {
                window_id: Some("w0".to_string()),
                profile: None,
                backend: Some(TerminalBackendKind::WebPty),
            },
        )
        .await
        .expect_err("WebPty must reject window_id");
        assert!(
            err.message.contains("不支持"),
            "actionable: {}",
            err.message
        );

        let err2 = create_terminal_session_inner(
            app.handle(),
            &state,
            CreateSessionOpts {
                window_id: None,
                profile: Some("Solarized".to_string()),
                backend: Some(TerminalBackendKind::WebPty),
            },
        )
        .await
        .expect_err("WebPty must reject profile");
        assert!(
            err2.message.contains("不支持"),
            "actionable: {}",
            err2.message
        );
    }
}
