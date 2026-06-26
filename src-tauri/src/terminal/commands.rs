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
use crate::error::{AppError, AppResult};
use crate::model::{CreateSessionOpts, TerminalSession};
use crate::state::AppState;

/// The python interpreter that runs the daemon (PATH-resolved). Hardcoded — the slice
/// deliberately does NOT read `config` for it (keeping `terminal` free of any cross-slice
/// config dependency, per the slice-boundary charter); a future configurable interpreter is a
/// separate decision.
const PYTHON_BIN: &str = "python3";

/// Resolve the bundled daemon script to an absolute path. In a packaged app it lives under
/// the resource dir (declared in `tauri.conf.json` `bundle.resources`); `BaseDirectory::Resource`
/// resolves the same relative path tauri copied it to.
fn resolve_script_path<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> AppResult<String> {
    let path = app
        .path()
        .resolve(
            "resources/iterm-daemon/iterm_daemon.py",
            BaseDirectory::Resource,
        )
        .map_err(|e| AppError::new(format!("无法定位 iTerm daemon 脚本: {e}")))?;
    Ok(path.to_string_lossy().into_owned())
}

/// Build the backend for a single command. The 2nd-backend extension point lands HERE: a
/// future `TerminalBackendKind` (sealed enum) would drive an exhaustive `match` selecting
/// `ITermBackend` vs a WebPty backend — the Hard carrier forcing every command to handle the
/// new variant. Single iTerm backend this PR, so the selection is direct.
fn backend<'a, R: tauri::Runtime>(
    app: &'a tauri::AppHandle<R>,
    state: &'a AppState,
    script: &'a str,
) -> ITermBackend<'a, R> {
    ITermBackend::new(app, &state.terminal, PYTHON_BIN, script)
}

#[tauri::command]
pub async fn list_terminal_sessions<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> AppResult<Vec<TerminalSession>> {
    state.terminal.resume();
    let script = resolve_script_path(&app)?;
    backend(&app, &state, &script).list_sessions().await
}

#[tauri::command]
pub async fn create_terminal_session<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    opts: CreateSessionOpts,
) -> AppResult<TerminalSession> {
    if let Some(window_id) = opts.window_id.as_deref() {
        check_identifier(window_id)?;
    }
    if let Some(profile) = opts.profile.as_deref() {
        check_identifier(profile)?;
    }
    state.terminal.resume();
    let script = resolve_script_path(&app)?;
    backend(&app, &state, &script).create_session(opts).await
}

/// Attach the panel to a session = start streaming its screen (daemon `subscribe`). The
/// backend emits the one-shot `Attached` then the daemon's `screenUpdate` stream follows.
#[tauri::command]
pub async fn attach_terminal<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> AppResult<()> {
    check_identifier(&session_id)?;
    state.terminal.resume();
    let script = resolve_script_path(&app)?;
    backend(&app, &state, &script).subscribe(&session_id).await
}

/// Detach the panel from a session = stop streaming its screen (daemon `unsubscribe`).
#[tauri::command]
pub async fn detach_terminal<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> AppResult<()> {
    check_identifier(&session_id)?;
    state.terminal.resume();
    let script = resolve_script_path(&app)?;
    backend(&app, &state, &script)
        .unsubscribe(&session_id)
        .await
}

/// Upper bound on a single `send_terminal_input` payload (64 KiB, byte length). A keystroke /
/// paste is tiny; this caps a malformed / oversized `data` before it reaches the daemon
/// (loopback-only DoS hardening — would be P2 if the IPC listener were ever exposed).
const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Reject an oversized `send_terminal_input` payload with an actionable error. Pure free helper
/// (no Tauri `State`) so it's unit-testable without a live app — the command calls it first.
fn check_input_size(data: &str) -> AppResult<()> {
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
fn check_identifier(id: &str) -> AppResult<()> {
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
fn check_grid(cols: u16, rows: u16) -> AppResult<()> {
    if !(MIN_GRID..=MAX_COLS).contains(&cols) || !(MIN_GRID..=MAX_ROWS).contains(&rows) {
        return Err(AppError::new(format!(
            "非法终端网格尺寸（cols={cols} rows={rows}）：列需在 {MIN_GRID}..={MAX_COLS}、行需在 {MIN_GRID}..={MAX_ROWS} 之间"
        )));
    }
    Ok(())
}

/// Forward keystrokes / pasted text into a session (daemon `sendText`).
#[tauri::command]
pub async fn send_terminal_input<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    session_id: String,
    data: String,
) -> AppResult<()> {
    check_identifier(&session_id)?;
    check_input_size(&data)?;
    state.terminal.resume();
    let script = resolve_script_path(&app)?;
    backend(&app, &state, &script)
        .send_text(&session_id, &data)
        .await
}

/// Resize a session's grid (daemon `resize`), e.g. after an xterm fit.
#[tauri::command]
pub async fn resize_terminal<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    session_id: String,
    cols: u16,
    rows: u16,
) -> AppResult<()> {
    check_identifier(&session_id)?;
    check_grid(cols, rows)?;
    state.terminal.resume();
    let script = resolve_script_path(&app)?;
    backend(&app, &state, &script)
        .resize(&session_id, cols, rows)
        .await
}

/// Probe the daemon for the StatusBar (lazy start: first call spawns + handshakes, later
/// calls reuse). PASSIVE: honors a prior `stop_terminal_daemon` (does NOT resume), never
/// errors (failures map to a status struct), mirroring `get_codex_status`.
#[tauri::command]
pub async fn get_terminal_status<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> AppResult<TerminalDaemonStatus> {
    let script = resolve_script_path(&app)?;
    Ok(state.terminal.status(&app, PYTHON_BIN, &script).await)
}

/// Explicitly stop the resident daemon (sync, like `stop_codex`): sets the user-stop flag +
/// kills the child. Passive `get_terminal_status` then reports stopped without reviving; an
/// action command (which `resume()`s) still force-starts. Goes through `AppResult` for funnel
/// consistency (`stop` can't fail, so it is always `Ok`).
#[tauri::command]
pub fn stop_terminal_daemon(state: tauri::State<'_, AppState>) -> AppResult<TerminalDaemonStatus> {
    Ok(state.terminal.stop())
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
}
