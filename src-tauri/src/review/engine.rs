//! `ReviewEngine` — the extensibility seam for review engines (issue #11).
//!
//! The codex app-server is the MVP impl (`super::engines::codex`); a Claude engine
//! (`claude -p "/pr-review <N>"` headless, or the Agent SDK) plugs in by
//! implementing this trait. The UI depends only on the streamed
//! [`crate::events::ReviewEvent`]s, never a concrete engine.

use crate::error::AppResult;

/// Identifies a running review session.
pub type SessionId = String;

/// Starts a review for a PR and streams [`crate::events::ReviewEvent`]s
/// out-of-band to the frontend; a running session can be interrupted.
#[allow(async_fn_in_trait)]
pub trait ReviewEngine {
    /// Start a review. `kind` is `"review"` or `"check"`. Returns `Ok(Some(id))` when
    /// a session started, or `Ok(None)` when the `(pr, kind)` was DEDUPED (already
    /// covered by an in-flight review) — a first-class "not started, not an error"
    /// outcome so a caller never confuses a dedup with a start failure.
    async fn start(&self, pr_number: u64, kind: &str) -> AppResult<Option<SessionId>>;
    /// Interrupt a running session.
    async fn stop(&self, session: &SessionId) -> AppResult<()>;
}
