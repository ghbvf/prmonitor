//! Web PTY backend — the SECOND [`super::backend::TerminalBackend`] impl (#1372).
//!
//! [`WebPtyBackend`] is a per-request handle (like [`super::iterm::ITermBackend`]) borrowing the
//! composition root's pieces — the `app` (for the emit funnel) and the resident
//! [`WebPtyManager`] (which owns the PTY pool). Each trait method delegates straight to the
//! manager, so the seam stays a thin adapter: the real work (spawn, reader pump, scrollback,
//! teardown) lives in [`super::webpty_manager`]. Together with the iTerm impl this realizes the
//! sealed `TerminalBackendKind` + exhaustive `match` the `#1383` slice reserved.
//!
//! The pure [`default_shell_program`] lives here (no PTY needed) so it is unit-testable.

use tauri::{AppHandle, Runtime};

use super::backend::TerminalBackend;
use super::webpty_manager::WebPtyManager;
use crate::error::AppResult;
use crate::model::{CreateSessionOpts, TerminalSession};

/// A per-request Web PTY backend handle. Cheap to build per command (it only borrows); the
/// expensive resident state (the PTY pool + reader threads) lives in [`WebPtyManager`].
pub struct WebPtyBackend<'a, R: Runtime> {
    app: &'a AppHandle<R>,
    manager: &'a WebPtyManager,
}

impl<'a, R: Runtime> WebPtyBackend<'a, R> {
    /// Build a handle borrowing the composition root's pieces (the command layer supplies them:
    /// `&app`, `&state.web_pty`).
    pub fn new(app: &'a AppHandle<R>, manager: &'a WebPtyManager) -> Self {
        Self { app, manager }
    }
}

impl<R: Runtime> TerminalBackend for WebPtyBackend<'_, R> {
    async fn list_sessions(&self) -> AppResult<Vec<TerminalSession>> {
        Ok(self.manager.list())
    }

    async fn create_session(&self, opts: CreateSessionOpts) -> AppResult<TerminalSession> {
        self.manager.spawn(self.app, &opts)
    }

    async fn send_text(&self, session_id: &str, text: &str) -> AppResult<()> {
        // Keystrokes / pasted text are raw bytes to the pty (a PTY carries bytes, not screen ops).
        self.manager.write(session_id, text.as_bytes())
    }

    async fn subscribe(&self, session_id: &str) -> AppResult<()> {
        self.manager.subscribe(self.app, session_id)
    }

    async fn unsubscribe(&self, session_id: &str) -> AppResult<()> {
        self.manager.unsubscribe(session_id)
    }

    async fn resize(&self, session_id: &str, cols: u16, rows: u16) -> AppResult<()> {
        self.manager.resize(session_id, cols, rows)
    }

    async fn close_session(&self, session_id: &str) -> AppResult<()> {
        self.manager.close(self.app, session_id)
    }
}

/// The shell program a new PTY session spawns. Pure (reads only env / PATH), so it is
/// unit-testable without opening a PTY. Unix: `$SHELL` (when set + non-blank), else `/bin/zsh`.
/// Windows: `powershell.exe` when it's on PATH, else `cmd.exe`.
pub fn default_shell_program() -> String {
    #[cfg(unix)]
    {
        unix_shell(std::env::var("SHELL").ok())
    }
    #[cfg(windows)]
    {
        windows_shell()
    }
}

/// Pure unix shell resolution (the `$SHELL` value is injected so the logic is testable without
/// mutating process env): the value when set + non-blank, else `/bin/zsh`.
#[cfg(unix)]
fn unix_shell(shell_env: Option<String>) -> String {
    shell_env
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/zsh".to_string())
}

/// Windows shell resolution: prefer PowerShell, fall back to `cmd.exe` when it isn't on PATH.
#[cfg(windows)]
fn windows_shell() -> String {
    fn on_path(exe: &str) -> bool {
        std::env::var_os("PATH")
            .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(exe).is_file()))
    }
    if on_path("powershell.exe") {
        "powershell.exe".to_string()
    } else {
        "cmd.exe".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unix_shell_prefers_a_set_shell_env() {
        assert_eq!(unix_shell(Some("/bin/bash".to_string())), "/bin/bash");
    }

    #[cfg(unix)]
    #[test]
    fn unix_shell_falls_back_to_zsh_when_unset_or_blank() {
        assert_eq!(unix_shell(None), "/bin/zsh");
        assert_eq!(unix_shell(Some(String::new())), "/bin/zsh");
        assert_eq!(unix_shell(Some("   ".to_string())), "/bin/zsh");
    }

    #[cfg(windows)]
    #[test]
    fn windows_shell_is_powershell_or_cmd() {
        let shell = windows_shell();
        assert!(
            shell == "powershell.exe" || shell == "cmd.exe",
            "unexpected windows shell: {shell}"
        );
    }

    #[test]
    fn default_shell_program_is_non_empty() {
        assert!(!default_shell_program().is_empty());
    }
}
