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
    /// Continue an EXISTING (terminal `Done`/`Failed`) session with a follow-up user
    /// `message`, streaming the reply through the SAME [`crate::events::ReviewEvent`]
    /// pipeline (`MessageDelta` / `ReasoningDelta` / `TurnCompleted`) the initial review
    /// used. `user_item_id` is the CALLER-supplied id the user's typed message is persisted
    /// under (so the frontend's optimistic bubble id == the persisted id, and reopen-dedup
    /// works). Each engine continues its own conversation: codex issues a second turn on the
    /// resident thread (same-app-run only — no `thread/resume` in the protocol); claude
    /// `--resume`s the on-disk transcript (cross-restart). Persisting the user message and
    /// the registry/status transitions are reused, NOT duplicated.
    ///
    /// AI-robust carrier (per `.claude/rules/prmonitor/ai-robust.md`): **Hard**. This is a
    /// trait METHOD, so every concrete [`ReviewEngine`] (`CodexEngine`, `ClaudeEngine`) MUST
    /// implement follow-up — a new engine that omits it is a compile error, not a silently
    /// missing capability. The command routes to it via the same exhaustive `match
    /// EngineKind` the start path uses, never a downcast.
    async fn send_message(
        &self,
        session: &SessionId,
        message: &str,
        user_item_id: &str,
    ) -> AppResult<()>;
}
