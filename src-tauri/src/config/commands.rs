//! Config slice Tauri commands.

use tauri::Manager;

use super::model::AppConfig;
use super::service;
use crate::error::AppResult;
use crate::state::AppState;

/// Returns the bundled app version (from Cargo metadata) for the UI header.
#[tauri::command]
pub fn app_version() -> AppResult<String> {
    Ok(env!("CARGO_PKG_VERSION").to_string())
}

/// Returns the persisted configuration (defaults when nothing is stored yet).
#[tauri::command]
pub fn get_config<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> AppResult<AppConfig> {
    service::load(&app)
}

/// Validates and persists the configuration, then fires the post-save hook (AB#1225 F4) so a save
/// takes effect on the Remote Access listener runtime immediately — reconcile follows persistence
/// atomically, with no reliance on the frontend issuing a separate call. The hook (installed once in
/// `lib.rs` `setup()`, the SOLE config→remote bridge) is best-effort (`fire` returns `()`, and the
/// reconcile it drives swallows per-listener bind failures into status), so a bad listener never
/// fails the save of unrelated settings. config never names `crate::remote` — the seam keeps the
/// `config` slice from orchestrating the remote horizontal (mirrors `notify_outbox` / webhook).
#[tauri::command]
pub fn set_config<R: tauri::Runtime>(app: tauri::AppHandle<R>, config: AppConfig) -> AppResult<()> {
    let listeners = config.listeners.clone();
    service::save(&app, config)?;
    app.state::<AppState>().config_saved.fire(listeners);
    Ok(())
}

/// Persists the active project selection (#35) without re-validating the whole
/// config — see [`service::set_active_project`]. The frontend calls this on every
/// project switch so the last-viewed project survives a restart.
#[tauri::command]
pub fn set_active_project<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: String,
) -> AppResult<()> {
    service::set_active_project(&app, &project_id)
}
