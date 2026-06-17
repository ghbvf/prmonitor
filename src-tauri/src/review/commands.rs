//! Review slice Tauri commands.

use crate::config::service as config_service;
use crate::error::{AppError, AppResult};
use crate::review::engine::{ReviewEngine, SessionId, StartReviewOutcome};
use crate::review::engines::codex::{CodexEngine, CodexStatus};
use crate::review::session::SessionInfo;
use crate::state::AppState;

/// The codex binary name (PATH-resolved). Single source for every review command
/// and the composition-layer dispatcher ([`crate::dispatch`]), which imports this
/// `pub(crate)` const rather than re-stating the literal.
pub(crate) const CODEX_BIN: &str = "codex";

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
    // Resolve the project being reviewed (#35) and re-check ITS filesystem-dependent
    // paths so an absent / escaping `skillRelPath` (e.g. a hand-edited config) fails
    // before we attach the skill path to the turn, rather than handing codex a bad path.
    // `project_validated` is the per-project analogue of the old `load_validated`; the
    // review slice still depends only on `config::service`, never `config::model`.
    let project = config_service::project_validated(&app, &project_id)?;
    let skill_abs = skill_abs_path(&project.repo_root, &project.skill_rel_path);
    // MANUAL force-start: a user asking to review overrides a prior `stop_codex`.
    // `resume()` clears the user-stop flag BEFORE `engine.start()` reaches the
    // `connection()` funnel (which refuses when stopped). Auto-dispatch does NOT
    // resume, so a stopped server is never auto-revived (PR #47 F1).
    state.codex.resume();
    let engine = CodexEngine {
        app: &app,
        codex: &state.codex,
        registry: &state.sessions,
        codex_bin: CODEX_BIN,
        project_id: &project.id,
        repo: &project.repo,
        repo_root: &project.repo_root,
        skill_abs_path: &skill_abs,
    };
    // `Deduped` = the registry already has an in-flight review for this
    // `(project_id, pr, kind)`: a manual re-start is a benign no-op surfaced as an error
    // (the UI shows it; nothing double-starts). Stop the running one first to re-review.
    match engine.start(pr_number, &kind).await? {
        StartReviewOutcome::Started(session_id) => Ok(session_id),
        StartReviewOutcome::Deduped => Err(AppError::new(format!(
            "PR {pr_number} 的 {kind} review 已在进行中"
        ))),
    }
}

/// Interrupt a running review session (by its `threadId`). The terminal
/// `turnCompleted` (status `interrupted`) follows on the `review:event` stream.
#[tauri::command]
pub async fn stop_review<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> AppResult<()> {
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
    };
    engine.stop(&session_id).await
}

/// Snapshot of all review sessions (running + finished) for the UI.
#[tauri::command]
pub fn list_review_sessions(state: tauri::State<'_, AppState>) -> Vec<SessionInfo> {
    state.sessions.list()
}

/// Resolve the absolute path to the pr-review skill file codex attaches to the
/// turn. `repo_root` is an absolute dir and `skill_rel_path` a relative path under
/// it (both config-validated), so the join is absolute and infallible.
fn skill_abs_path(repo_root: &str, skill_rel_path: &str) -> String {
    std::path::Path::new(repo_root)
        .join(skill_rel_path)
        .to_string_lossy()
        .into_owned()
}
