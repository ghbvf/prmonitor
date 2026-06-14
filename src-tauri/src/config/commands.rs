//! Config slice Tauri commands.

use super::model::AppConfig;
use super::service;
use crate::error::AppResult;

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

/// Validates and persists the configuration.
#[tauri::command]
pub fn set_config<R: tauri::Runtime>(app: tauri::AppHandle<R>, config: AppConfig) -> AppResult<()> {
    service::save(&app, config)
}
