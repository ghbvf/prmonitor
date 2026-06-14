//! Config slice logic.
//!
//! Backend-owned persistence via `tauri-plugin-store`'s Rust `StoreExt`. The
//! frontend calls the `get_config` / `set_config` commands (not the store plugin
//! directly), so all reads/writes funnel through here.

use tauri_plugin_store::StoreExt;

use super::model::AppConfig;
use crate::error::{AppError, AppResult};

/// Store file holding the persisted config.
const STORE_FILE: &str = "config.json";
/// Key under which the [`AppConfig`] value lives in the store.
const CONFIG_KEY: &str = "appConfig";

/// Loads the persisted configuration, falling back to [`AppConfig::default`]
/// when nothing has been stored yet.
pub fn load<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> AppResult<AppConfig> {
    let store = app
        .store(STORE_FILE)
        .map_err(|e| AppError::new(format!("打开配置存储失败: {e}")))?;

    match store.get(CONFIG_KEY) {
        None => Ok(AppConfig::default()),
        // Surface (don't silently discard) a corrupt/incompatible persisted
        // config so the user can fix it rather than lose their settings.
        Some(value) => serde_json::from_value(value)
            .map_err(|e| AppError::new(format!("解析持久化配置失败: {e}"))),
    }
}

/// Persists the configuration after validating filesystem-dependent fields.
pub fn save<R: tauri::Runtime>(app: &tauri::AppHandle<R>, config: AppConfig) -> AppResult<()> {
    super::model::validate(&config)?;

    let store = app
        .store(STORE_FILE)
        .map_err(|e| AppError::new(format!("打开配置存储失败: {e}")))?;

    let value = serde_json::to_value(&config).map_err(|e| AppError::new(e.to_string()))?;
    // tauri-plugin-store 2.x: `Store::set` is infallible and returns `()`.
    store.set(CONFIG_KEY, value);
    store
        .save()
        .map_err(|e| AppError::new(format!("写入配置存储失败: {e}")))?;
    Ok(())
}
