//! `ReviewEngine` — the extensibility seam for review engines (issue #11).
//!
//! The codex app-server is the MVP impl (`super::engines::codex`); a Claude engine
//! (`claude -p "/pr-review <N>"` headless, or the Agent SDK) plugs in by
//! implementing this trait. The UI depends only on the streamed
//! [`crate::events::ReviewEvent`]s, never a concrete engine.

use crate::error::AppResult;

/// Identifies a running review session.
pub type SessionId = String;

/// Outcome of [`ReviewEngine::start`]: a session started, or the `(pr, kind)` was
/// DEDUPED (already covered by an in-flight review) — both first-class, neither an
/// error.
///
/// AI-robust carrier (per `.claude/rules/prmonitor/ai-robust.md`): **Hard**. This
/// replaces the prior `Option<SessionId>` return where `Ok(None)` MEANT "deduped"
/// only by doc convention (Soft — a reader/impl could mistake it for "no id, treat
/// as failure"). With a named two-variant enum the dedup branch is named in the type
/// system: every caller's `match` is exhaustive (a forgotten arm is a compile error),
/// and a new engine impl literally cannot express "started but no id" or conflate
/// dedup with failure. Violation is now unexpressable, not merely discouraged.
/// (`#[non_exhaustive]` is deliberately omitted — the seam is in-crate, and the whole
/// point is that adding a variant SHOULD break every callsite so each handles it.)
#[derive(Debug)]
pub enum StartReviewOutcome {
    /// A review session started; carries its id (codex `threadId`).
    Started(SessionId),
    /// The `(pr, kind)` was already reserved / in flight — no second review started.
    Deduped,
}

/// Starts a review for a PR and streams [`crate::events::ReviewEvent`]s
/// out-of-band to the frontend; a running session can be interrupted.
#[allow(async_fn_in_trait)]
pub trait ReviewEngine {
    /// Start a review. `kind` is `"review"` or `"check"`. Returns
    /// [`StartReviewOutcome::Started`] with the session id, or
    /// [`StartReviewOutcome::Deduped`] when the `(pr, kind)` was already covered by an
    /// in-flight review — a dedup is never confused with a start failure (`Err`).
    async fn start(&self, pr_number: u64, kind: &str) -> AppResult<StartReviewOutcome>;
    /// Interrupt a running session.
    async fn stop(&self, session: &SessionId) -> AppResult<()>;
}
