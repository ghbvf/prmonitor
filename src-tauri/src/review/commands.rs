//! Review slice Tauri commands.

use tauri::Manager;

use crate::config::service as config_service;
use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::model::EngineKind;
use crate::review::engine::{ReviewEngine, SessionId, StartReviewOutcome};
use crate::review::engines::claude::process::CLAUDE_BIN;
use crate::review::engines::claude::ClaudeEngine;
use crate::review::engines::codex::{CodexEngine, CodexStatus};
use crate::review::history_store::{self, HistoryItem};
use crate::review::session::{CommentUrlContext, SessionInfo};
use crate::state::AppState;

/// The codex binary name (PATH-resolved). Single source for every review command
/// and the composition-layer dispatcher ([`crate::dispatch`]), which imports this
/// `pub(crate)` const rather than re-stating the literal.
pub(crate) const CODEX_BIN: &str = "codex";

/// Rejects any review `kind` other than `review` / `check` at the command boundary.
///
/// `session.rs` branches on `kind == "check"` and treats EVERY other value as a
/// `review` turn — so an unvalidated kind (a bogus string from a buggy/forged invoke)
/// would silently run a full review while the registry/ledger key keeps the bogus
/// kind, splitting dedup. Whitelisting here fails fast before any session starts. The
/// Hard path (future) is a shared Rust enum ↔ TS union; this is the Medium guard until
/// then. Pure (no `AppHandle`) so it is unit-testable.
fn validate_kind(kind: &str) -> AppResult<()> {
    if kind == "review" || kind == "check" {
        Ok(())
    } else {
        Err(AppError::new(format!(
            "kind 非法（只接受 review | check）: {kind:?}"
        )))
    }
}

/// Reports codex app-server availability for the StatusBar. Ensures the resident
/// connection (lazy start: first call spawns + handshakes, later calls reuse) and
/// reports `available` + version. The probe never errors (failures map to a
/// status struct); only the config read can fail.
///
/// The codex app-server is GLOBAL and single (one resident process for all
/// projects); its spawn-handshake cwd is the ACTIVE project's `repo_root`, read from
/// the config slice's public service (#35). Per-turn `cwd` scopes each review's
/// working dir, so this is only the handshake cwd. `AppConfig` stays config-private.
#[tauri::command]
pub async fn get_codex_status<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> AppResult<CodexStatus> {
    let repo_root = config_service::active_repo_root(&app)?;
    Ok(state.codex.status(CODEX_BIN, &repo_root).await)
}

/// 显式启动常驻 codex app-server（清除「已停止」标记并拉起握手）。返回最新状态。
/// 全局单例 codex 的握手 cwd 取「活动项目」的 `repo_root`（#35）；每轮 review 的实际工作目录由 per-turn `cwd` 覆盖。
#[tauri::command]
pub async fn start_codex<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> AppResult<CodexStatus> {
    let repo_root = config_service::active_repo_root(&app)?;
    Ok(state.codex.start(CODEX_BIN, &repo_root).await)
}

/// 显式停止常驻 codex app-server（设「已停止」标记 + 杀进程；被动状态探测此后不再自动拉起，显式 review 仍会强制启动）。
/// 走统一错误漏斗 `AppResult`（与其余命令一致；`stop` 不会失败，故恒 `Ok`。前端 `invoke<CodexStatus>` 不变——`AppResult` 成功序列化为 `T`）。
#[tauri::command]
pub fn stop_codex(state: tauri::State<'_, AppState>) -> AppResult<CodexStatus> {
    Ok(state.codex.stop())
}

/// The single source for engine selection on the MANUAL / explicit path (AB#1042): the
/// shared dispatch body behind BOTH [`start_review`] (project resolved by id) and
/// [`trigger_review`] (project resolved by id-or-repo `reference`). The AUTO-dispatch path
/// has its own engine selection in `lib.rs::run_auto_dispatch` (intentionally separate — it
/// monomorphizes `dispatch::auto_dispatch` per concrete engine and applies the codex
/// stop-flag gate that doesn't exist on the manual path). Both are INDEPENDENTLY exhaustive
/// `match project.engine_kind` over the sealed [`EngineKind`] (model.rs) — that exhaustiveness
/// is the **Hard** carrier: a new variant without an arm in EITHER match is a compile error,
/// so neither path can silently miss a new engine. Don't add a THIRD manual-path `match`:
/// this one folds in the dedup + outcome mapping, so every explicit entry routes through it.
///
/// Folds the `outcome → Result` mapping in (both callers handle a [`StartReviewOutcome`]
/// identically): `Started` → the session id; `Deduped` (the registry already has an
/// in-flight review for this `(project_id, pr, kind)`) → a benign "already in flight" error
/// — a re-start does NOT double-start; stop the running one first to re-review.
///
/// `kind` is pre-validated by the caller (both validate before resolving the project).
async fn dispatch_engine<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    project: &config_service::Project,
    pr_number: u64,
    kind: &str,
) -> AppResult<SessionId> {
    // Snapshot the comment-URL source context from the project NOW (AB#1042), so the terminal
    // `finalize_turn` resolves the pr-review comment URL against the project the review ran
    // against — never a config edited mid-review. Both engines carry this owned context into
    // their `Starting` session; built once here since the fields are identical for either.
    let url_ctx = CommentUrlContext {
        source_kind: project.source_kind,
        repo: project.repo.clone(),
        azure_org: project.azure_org.clone(),
        azure_project: project.azure_project.clone(),
    };
    let outcome = match project.engine_kind {
        EngineKind::Codex => {
            let skill_abs = skill_abs_path(&project.repo_root, &project.skill_rel_path);
            // MANUAL / explicit force-start: an explicit trigger (UI button OR a CLI/deeplink
            // entry, AB#1042) overrides a prior `stop_codex`. `resume()` clears the user-stop
            // flag BEFORE `engine.start()` reaches the `connection()` funnel (which refuses
            // when stopped). Auto-dispatch does NOT resume, so a stopped server is never
            // auto-revived (PR #47 F1) — `trigger_review` IS explicit/manual, so it resumes
            // like `start_review` (same convention for every explicit entry point).
            state.codex.resume();
            let engine = CodexEngine {
                app,
                codex: &state.codex,
                registry: &state.sessions,
                codex_bin: CODEX_BIN,
                project_id: &project.id,
                repo: &project.repo,
                repo_root: &project.repo_root,
                skill_abs_path: &skill_abs,
                codex_model: &project.codex_model,
                url_ctx,
            };
            engine.start(pr_number, kind).await?
        }
        EngineKind::Claude => {
            let engine = ClaudeEngine {
                app,
                claude: &state.claude,
                registry: &state.sessions,
                claude_bin: CLAUDE_BIN,
                project_id: &project.id,
                repo: &project.repo,
                repo_root: &project.repo_root,
                claude_model: &project.claude_model,
                url_ctx,
            };
            engine.start(pr_number, kind).await?
        }
    };
    match outcome {
        StartReviewOutcome::Started(session_id) => Ok(session_id),
        StartReviewOutcome::Deduped => Err(AppError::new(format!(
            "PR {pr_number} 的 {kind} review 已在进行中"
        ))),
    }
}

/// Start a review for `(project_id, pr_number)` (`kind` = `"review"` or `"check"`),
/// returning the session id (codex `threadId`). Output streams out-of-band via the
/// `review:event` Tauri event ([`crate::events::ReviewEvent`]), each event stamped
/// with `project_id` (#35) so the frontend routes it to the owning project.
#[tauri::command]
pub async fn start_review<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    project_id: String,
    pr_number: u64,
    kind: String,
) -> AppResult<SessionId> {
    // Reject a bogus `kind` BEFORE any side effect (project resolve / codex resume):
    // `session.rs` treats every non-"check" value as a review, so an unvalidated kind
    // would run a full review under a bad registry key (see `validate_kind`).
    validate_kind(&kind)?;
    // Resolve the project being reviewed (#35) and re-check ITS filesystem-dependent
    // paths so an absent / escaping `skillRelPath` (e.g. a hand-edited config) fails
    // before we attach the skill path to the turn, rather than handing codex a bad path.
    // `project_validated` is the per-project analogue of the old `load_validated`; the
    // review slice still depends only on `config::service`, never `config::model`.
    let project = config_service::project_validated(&app, &project_id)?;
    dispatch_engine(&app, &state, &project, pr_number, &kind).await
}

/// Trigger a review by a free-form `reference` (a project `id` OR a `repo`), the
/// transport-agnostic funnel entry point for a third-party trigger (CLI/deeplink, future —
/// AB#1042). Resolves the project via [`config_service::project_by_ref_validated`] (id-first,
/// repo case-insensitive, ambiguity rejected), then shares [`dispatch_engine`] with
/// [`start_review`] — so engine selection + dedup stay single-source. `kind` is validated
/// first (same boundary as `start_review`); a `Deduped` surfaces as the same benign error.
#[tauri::command]
pub async fn trigger_review<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    reference: String,
    pr_number: u64,
    kind: String,
) -> AppResult<SessionId> {
    // Reject a bogus `kind` BEFORE resolving the project / any side effect (parity with
    // `start_review`): an unvalidated kind would run a full review under a bad registry key.
    validate_kind(&kind)?;
    // Resolve by id-or-repo + validate the project's filesystem paths (the trigger funnel's
    // analogue of `project_validated`), keeping the review slice on `config::service` only.
    let project = config_service::project_by_ref_validated(&app, &reference)?;
    dispatch_engine(&app, &state, &project, pr_number, &kind).await
}

/// Interrupt a running review session (by its `threadId`). The terminal
/// `turnCompleted` (status `interrupted`) follows on the `review:event` stream.
#[tauri::command]
pub async fn stop_review<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> AppResult<()> {
    // Stop-engine resolution WITHOUT an engine field on the persisted `SessionInfo`
    // (#718): a session id is globally unique, so whichever manager holds its kill handle
    // definitively OWNS the session. Try claude first — `stop` returns true iff the
    // ClaudeManager owned this session (and just aborted its pump → killed `claude -p`).
    // This is deterministic, not a guess, and avoids changing `SessionInfo`'s
    // persisted/mirrored wire shape and all its constructors. If false, the session is
    // codex's (or already gone) → fall through to the unchanged codex interrupt path.
    if state.claude.stop(&session_id) {
        return Ok(());
    }
    // `stop` interrupts an already-live turn purely by its session id (codex
    // `threadId`); it needs neither the project, the repo, nor the skill path (see
    // `session::stop_review`, where `codex_bin`/`repo_root` are bound to `_`). So we
    // build the engine with empty context fields and skip the config read entirely —
    // a missing / invalid config must not block stopping a running review.
    let engine = CodexEngine {
        app: &app,
        codex: &state.codex,
        registry: &state.sessions,
        codex_bin: CODEX_BIN,
        project_id: "",
        repo: "",
        repo_root: "",
        skill_abs_path: "",
        // `stop` resolves purely by session id; model is irrelevant on the interrupt path.
        codex_model: "",
        // `stop` never reaches `start_review`/`promote_reservation`, so the URL context is
        // unused here — a default (empty) value satisfies the field without a config read.
        url_ctx: CommentUrlContext {
            source_kind: crate::model::SourceKind::default(),
            repo: String::new(),
            azure_org: String::new(),
            azure_project: String::new(),
        },
    };
    engine.stop(&session_id).await
}

/// Snapshot of all review sessions (running + finished) for the UI — the IN-MEMORY
/// registry (live status). After a restart this is empty; [`get_pr_sessions`] reads the
/// durable table instead.
#[tauri::command]
pub fn list_review_sessions(state: tauri::State<'_, AppState>) -> Vec<SessionInfo> {
    state.sessions.list()
}

/// A session's persisted history items in stream order (#70) — the message/reasoning
/// blocks produced before the user opened the session. Drives the review panel's "open a
/// history session and see prior content" (#67) by hydrating the per-session buffer.
#[tauri::command]
pub fn get_session_history<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: String,
    pr_number: u64,
    thread_id: String,
) -> AppResult<Vec<HistoryItem>> {
    let db = app.state::<Database>();
    // Scoped read (pr-review F6): history is returned only if `thread_id` belongs to this
    // `(project_id, pr_number)`, so a caller can't pull another PR's history by raw id.
    history_store::get_history(db.inner(), &project_id, pr_number, &thread_id)
}

/// A PR's persisted sessions, newest first, from the durable `review_session` table (#70)
/// — so each PR can restore its session list after a restart (the #67 nav associates
/// sessions per PR). Distinct from [`list_review_sessions`] (in-memory live snapshot).
#[tauri::command]
pub fn get_pr_sessions<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: String,
    pr_number: u64,
) -> AppResult<Vec<SessionInfo>> {
    let db = app.state::<Database>();
    history_store::get_pr_sessions(db.inner(), &project_id, pr_number)
}

/// Resolve the absolute path to the pr-review skill file codex attaches to the
/// turn. `repo_root` is an absolute dir and `skill_rel_path` a relative path under
/// it (both config-validated), so the join is absolute and infallible.
///
/// Single source for both codex callsites — the manual `start_review` here and the
/// auto path's `run_auto_dispatch` in the composition root (`lib.rs`), which calls
/// `review::commands::skill_abs_path` rather than keeping its own copy. The review
/// slice owns the codex skill-path concept, so it lives here (`pub(crate)`).
pub(crate) fn skill_abs_path(repo_root: &str, skill_rel_path: &str) -> String {
    std::path::Path::new(repo_root)
        .join(skill_rel_path)
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_kind_accepts_review_and_check_only() {
        assert!(validate_kind("review").is_ok());
        assert!(validate_kind("check").is_ok());
        // Any other value (incl. case variants / empty / arbitrary) is rejected so it
        // can never reach `session.rs` and run as a review under a bogus key.
        for bad in ["", "Review", "CHECK", "foo", "review ", "reviewcheck"] {
            assert!(
                validate_kind(bad).is_err(),
                "expected kind {bad:?} rejected"
            );
        }
    }
}
