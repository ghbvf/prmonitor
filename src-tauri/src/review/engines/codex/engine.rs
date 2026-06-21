//! [`ReviewEngine`] implementation backed by the codex app-server (the MVP engine
//! per issue #11). A thin adapter: it bundles the per-call context (the resident
//! [`CodexManager`], the [`SessionRegistry`], the `AppHandle`, and config) and
//! delegates the orchestration to [`crate::review::session`]. A future Claude
//! engine implements the same [`ReviewEngine`] without touching the commands.

use super::CodexManager;
use crate::error::AppResult;
use crate::review::engine::{ReviewEngine, SessionId, StartReviewOutcome};
use crate::review::session::{self, SessionRegistry};

/// Per-request engine handle. Borrows the long-lived state from `AppState` plus
/// the request's `AppHandle`; constructed fresh by each command (cheap — all
/// borrows) so `start`/`stop` run inline while the streaming pump it spawns
/// outlives it (the pump owns clones, not the engine).
pub struct CodexEngine<'a, R: tauri::Runtime> {
    pub app: &'a tauri::AppHandle<R>,
    pub codex: &'a CodexManager,
    pub registry: &'a SessionRegistry,
    /// The codex binary name (PATH-resolved; matches `get_codex_status`).
    pub codex_bin: &'a str,
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
}

impl<R: tauri::Runtime> ReviewEngine for CodexEngine<'_, R> {
    async fn start(&self, pr_number: u64, kind: &str) -> AppResult<StartReviewOutcome> {
        session::start_review(
            self.app,
            self.codex,
            self.registry,
            self.codex_bin,
            self.repo,
            self.repo_root,
            self.skill_abs_path,
            self.codex_model,
            self.project_id,
            pr_number,
            kind,
        )
        .await
    }

    async fn stop(&self, session: &SessionId) -> AppResult<()> {
        session::stop_review(
            self.codex,
            self.registry,
            self.codex_bin,
            self.repo_root,
            session,
        )
        .await
    }
}
