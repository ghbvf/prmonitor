//! Remote Access runtime Tauri commands.

use tauri::Manager;

use crate::config::service as config_service;
use crate::error::AppResult;
use crate::state::AppState;

use super::status::RemoteAccessRuntimeStatus;

#[tauri::command]
pub fn get_remote_access_runtime_status<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> AppResult<RemoteAccessRuntimeStatus> {
    let cfg = config_service::load(&app)?;
    let local_api_token_set = !cfg.local_api_token.trim().is_empty();
    Ok(app
        .state::<AppState>()
        .remote
        .status_snapshot(&cfg.remote_access, local_api_token_set))
}
