//! `TerminalBackend` — the extensibility seam for terminal backends (#1383).
//!
//! The iTerm daemon is the MVP impl (`super::iterm::ITermBackend`); a future WebPty (a
//! local pseudo-terminal exposed to the Remote Web Console, PR2+) plugs in by implementing
//! this trait. The frontend depends only on the streamed [`crate::events::TerminalEvent`]s
//! and the [`crate::model::TerminalSession`] / [`crate::model::CreateSessionOpts`] contract,
//! never a concrete backend.
//!
//! Mirrors the `review::engine::ReviewEngine` seam style: `#[allow(async_fn_in_trait)]`,
//! monomorphic (no `dyn`) — a new backend is a new implementation, not a changed call site
//! (the ai-robust charter's trait-seam rule). A SECOND backend is added by a sealed
//! `TerminalBackendKind` enum + an exhaustive `match` at the command layer (the Hard carrier
//! that forces every dispatch site to handle the new variant) — NOT now (single iTerm
//! backend this PR; see the slice doc in `mod.rs`).

use crate::error::AppResult;
use crate::model::{CreateSessionOpts, TerminalSession};

/// A source of addressable terminal sessions the frontend xterm panel attaches to.
///
/// THIS trait IS the future-WebPty extension point: the iTerm daemon impl borrows the
/// resident daemon; a WebPty impl would own a local PTY pool — both satisfy these six ops, so
/// the command layer and the frontend never learn which one is wired.
#[allow(async_fn_in_trait)]
pub trait TerminalBackend {
    /// Enumerate every session (flattened; the frontend re-groups window → tab → session).
    async fn list_sessions(&self) -> AppResult<Vec<TerminalSession>>;
    /// Create a new session, returning its row. `opts` lets the caller pick a window / profile
    /// or let the backend default both.
    async fn create_session(&self, opts: CreateSessionOpts) -> AppResult<TerminalSession>;
    /// Type `text` into a session (keystrokes / pasted input).
    async fn send_text(&self, session_id: &str, text: &str) -> AppResult<()>;
    /// Start streaming a session's screen (the panel attaches). The backend emits
    /// [`crate::events::TerminalEvent`]s out-of-band through [`crate::stream::emit`].
    async fn subscribe(&self, session_id: &str) -> AppResult<()>;
    /// Stop streaming a session's screen (the panel detaches).
    async fn unsubscribe(&self, session_id: &str) -> AppResult<()>;
    /// Resize a session's grid to `cols` × `rows`.
    async fn resize(&self, session_id: &str, cols: u16, rows: u16) -> AppResult<()>;
}
