//! Review slice Tauri commands.

use crate::config::{model as config_model, service as config_service};
use crate::error::AppResult;
use crate::review::engine::{ReviewEngine, SessionId};
use crate::review::engines::codex::{CodexEngine, CodexStatus};
use crate::review::session::SessionInfo;
use crate::state::AppState;

/// The codex binary name (PATH-resolved). Single source for every review command.
const CODEX_BIN: &str = "codex";

/// Reports codex app-server availability for the StatusBar. Ensures the resident
/// connection (lazy start: first call spawns + handshakes, later calls reuse) and
/// reports `available` + version. The probe never errors (failures map to a
/// status struct); only the config read can fail.
///
/// `repo_root` (the codex cwd) is read from the config slice's public service —
/// the same cross-slice, function-level read the pr slice uses; `AppConfig` stays
/// config-private.
#[tauri::command]
pub async fn get_codex_status<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> AppResult<CodexStatus> {
    let cfg = config_service::load(&app)?;
    Ok(state.codex.status(CODEX_BIN, &cfg.repo_root).await)
}

/// Start a review for `pr_number` (`kind` = `"review"` or `"check"`), returning
/// the session id (codex `threadId`). Output streams out-of-band via the
/// `review:event` Tauri event ([`crate::events::ReviewEvent`]).
#[tauri::command]
pub async fn start_review<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    pr_number: u64,
    kind: String,
) -> AppResult<SessionId> {
    let cfg = config_service::load(&app)?;
    // `load` does not re-validate persisted config; validate here so an absent /
    // escaping `skillRelPath` (e.g. a hand-edited config) fails before we attach
    // the skill path to the turn, rather than handing codex a bad path.
    config_model::validate(&cfg)?;
    let skill_abs = skill_abs_path(&cfg.repo_root, &cfg.skill_rel_path);
    let engine = CodexEngine {
        app: &app,
        codex: &state.codex,
        registry: &state.sessions,
        codex_bin: CODEX_BIN,
        repo: &cfg.repo,
        repo_root: &cfg.repo_root,
        skill_abs_path: &skill_abs,
    };
    engine.start(pr_number, &kind).await
}

/// Interrupt a running review session (by its `threadId`). The terminal
/// `turnCompleted` (status `interrupted`) follows on the `review:event` stream.
#[tauri::command]
pub async fn stop_review<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> AppResult<()> {
    let cfg = config_service::load(&app)?;
    let engine = CodexEngine {
        app: &app,
        codex: &state.codex,
        registry: &state.sessions,
        codex_bin: CODEX_BIN,
        repo: &cfg.repo,
        repo_root: &cfg.repo_root,
        // `stop` interrupts by session id; it needs neither the repo nor the skill
        // path, so we skip resolving the skill path here.
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
