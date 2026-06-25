//! Remote Access runtime Tauri command (AB#1225). Registered in `lib.rs` `generate_handler!`.

use tauri::Manager;

use crate::config::service as config_service;
use crate::error::AppResult;
use crate::state::AppState;

use super::status::ListenerRuntimeStatus;

/// Live per-listener runtime status for the 「远程访问」settings page. Loads the persisted config
/// and derives status fresh from the supervisor's live bound map (no stored vec → never stale).
#[tauri::command]
pub fn get_listener_runtime_status<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> AppResult<Vec<ListenerRuntimeStatus>> {
    let cfg = config_service::load(&app)?;
    // A bound local-api with an empty token 401s every request (`verify_bearer`), so report
    // `BoundNoAuth` not `Bound` — `AppConfig::default()` ships an empty `local_api_token`.
    let local_api_token_set = !cfg.local_api_token.trim().is_empty();
    Ok(app
        .state::<AppState>()
        .remote
        .status_snapshot(&cfg.listeners, local_api_token_set))
}
