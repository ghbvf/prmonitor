//! prmonitor — composition root.
//!
//! Vertical-slice layout: each slice (`config`, `pr`, `review`) is self-contained
//! and sits directly under `src/` (no `features/` wrapper). Horizontal concerns
//! are flat files (`error`, `events`, `model`, `state`). Slices never import each
//! other's internals — cross-slice types live in [`model`], the only contract.
//!
//! This file is the *only* place that wires slices together and registers their
//! Tauri commands.

// Modules are `pub` so forward-looking seams and shared types (e.g.
// `pr::source::PrSource`, `review::engine::ReviewEngine`) count as reachable API
// in this skeleton rather than tripping `dead_code` before their first use.
pub mod config;
pub mod error;
pub mod events;
pub mod model;
pub mod pr;
pub mod review;
pub mod state;

use state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            config::commands::app_version,
            config::commands::get_config,
            config::commands::set_config,
            pr::commands::fetch_prs_now,
            pr::commands::gh_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
