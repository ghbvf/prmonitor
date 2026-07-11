//! [`ReviewEngine`] implementation backed by the codex app-server (the MVP engine
//! per issue #11). A thin adapter: it bundles the per-call context (the resident
//! [`CodexManager`], the [`SessionRegistry`], the `AppHandle`, and config) and
//! delegates the orchestration to [`crate::review::session`]. A future Claude
//! engine implements the same [`ReviewEngine`] without touching the commands.

use super::CodexManager;
use crate::config::service::ResolvedCli;
use crate::error::AppResult;
use crate::model::CodexReasoningEffort;
use crate::model::ReviewKind;
use crate::review::engine::{ReviewEngine, ReviewStartCapability, SessionId, StartReviewOutcome};
use crate::review::session::{self, CommentUrlContext, SessionInfo, SessionRegistry};

/// Per-request engine handle. Borrows the long-lived state from `AppState` plus
/// the request's `AppHandle`; constructed fresh by each command (cheap — all
/// borrows) so `start`/`stop` run inline while the streaming pump it spawns
/// outlives it (the pump owns clones, not the engine).
pub struct CodexEngine<'a, R: tauri::Runtime> {
    pub app: &'a tauri::AppHandle<R>,
    pub codex: &'a CodexManager,
    pub registry: &'a SessionRegistry,
    /// The codex binary name (PATH-resolved; matches `get_codex_status`).
    pub codex_cli: &'a ResolvedCli,
    /// Owning project id (#35): scopes the registry reservation / dedup and stamps
    /// every streamed `ReviewEvent` so the frontend attributes it to the right
    /// project. The composition root (lib.rs) sets it from the project being acted on.
    pub project_id: &'a str,
    /// Monitored repo `owner/name` (named in the review prompt).
    pub repo: &'a str,
    /// Absolute local clone path codex runs the skill against (the turn cwd).
    pub repo_root: &'a str,
    /// Absolute path to the pr-review skill file attached to the turn.
    pub skill_abs_path: &'a str,
    /// Hand-typed codex model name (empty = codex's configured default). Set as the
    /// per-turn `model` override on `turn/start` (the app-server is shared, so model
    /// selection can't be a spawn flag).
    pub codex_model: &'a str,
    pub codex_reasoning_effort: CodexReasoningEffort,
    /// IMMUTABLE comment-URL source context (AB#1042), built from the project at dispatch.
    /// Owned (not a borrow) so it can move into `start_review` → the `Starting` session,
    /// pinning the terminal `finalize_turn`'s URL resolve to the project the review ran
    /// against — never a config edited mid-review. `pub(crate)`: the field's type is a
    /// crate-internal context, and the only constructors (commands.rs / lib.rs) are in-crate.
    pub(crate) url_ctx: CommentUrlContext,
    /// The PR number for the FOLLOW-UP (`send_message`) path only — the `ReviewEngine`
    /// trait's `send_message(session, message, user_item_id)` carries no `pr_number`, so the
    /// command resolves it (in-memory registry or durable row) and sets it here. The
    /// `start`/`stop` paths take `pr_number` as a method arg and ignore this field (set to 0
    /// at those construction sites).
    pub pr_number: u64,
    /// Full persisted session identity for the FOLLOW-UP path. It pins the creating engine,
    /// original kind, timestamp, and URL metadata across app restarts/config edits.
    pub session_info: Option<SessionInfo>,
    /// The owning outbox row id for the AB#1204 cross-restart dedup claim — `Some(outbox_id)` ONLY
    /// on the OUTBOX executor's start path ([`crate::review::commands::start_for_outbox`]), `None`
    /// on the manual / follow-up / auto-dispatch paths. When `Some`, `start` writes the claim's
    /// `thread_id` breadcrumb INSIDE `session::start_review` — right after `thread/start` yields a
    /// stable `thread_id`. The session row and claim breadcrumb commit atomically BEFORE registry
    /// promotion / `start_turn`, so linkage failure leaves no false-live session and the retry owns
    /// a clean reservation. `None` skips the claim write entirely (no outbox row to claim).
    pub outbox_claim_id: Option<i64>,
}

impl<R: tauri::Runtime> ReviewEngine for CodexEngine<'_, R> {
    async fn start(
        &self,
        _capability: &ReviewStartCapability,
        pr_number: u64,
        kind: ReviewKind,
    ) -> AppResult<StartReviewOutcome> {
        session::start_review(
            self.app,
            self.codex,
            self.registry,
            self.codex_cli,
            self.repo,
            self.repo_root,
            self.skill_abs_path,
            self.codex_model,
            self.codex_reasoning_effort,
            self.project_id,
            pr_number,
            kind,
            // `&self` start can't move the field; clone the owned context for this turn.
            self.url_ctx.clone(),
            // AB#1204: outbox path passes `Some(outbox_id)` so the claim breadcrumb is written
            // right after `thread/start` (before the turn runs); manual/auto paths pass `None`.
            self.outbox_claim_id,
        )
        .await
    }

    async fn send_message(
        &self,
        session: &SessionId,
        message: &str,
        user_item_id: &str,
    ) -> AppResult<()> {
        session::resume_turn(
            self.app,
            self.codex,
            self.registry,
            self.codex_cli,
            self.repo_root,
            self.codex_model,
            self.codex_reasoning_effort,
            self.project_id,
            self.pr_number,
            self.session_info
                .as_ref()
                .expect("CodexEngine::send_message requires session_info"),
            session,
            message,
            user_item_id,
            // `&self` can't move the field; clone the owned context for this follow-up turn.
            self.url_ctx.clone(),
        )
        .await
    }
}
