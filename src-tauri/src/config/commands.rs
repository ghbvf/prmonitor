//! Config slice Tauri commands.

use super::model::AppConfig;
use super::service;
use crate::error::AppResult;

/// Sample command proving slice → command → composition-root wiring (replaces
/// the scaffold `greet`). Returns the app version from Cargo metadata.
#[tauri::command]
pub fn app_version() -> AppResult<String> {
    Ok(env!("CARGO_PKG_VERSION").to_string())
}

/// Returns the current configuration (skeleton: defaults; PR2 adds persistence).
#[tauri::command]
pub fn get_config() -> AppResult<AppConfig> {
    Ok(service::current())
}
