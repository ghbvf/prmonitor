//! Config slice logic.
//!
//! Skeleton: returns defaults. PR2 reads/writes persisted config via
//! `tauri-plugin-store` here.

use super::model::AppConfig;

/// Returns the current configuration.
pub fn current() -> AppConfig {
    AppConfig::default()
}
