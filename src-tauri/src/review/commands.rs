//! Review slice Tauri commands.

use crate::config::service as config_service;
use crate::error::AppResult;
use crate::review::engines::codex::CodexStatus;

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
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<CodexStatus> {
    let cfg = config_service::load(&app)?;
    Ok(state.codex.status("codex", &cfg.repo_root).await)
}
