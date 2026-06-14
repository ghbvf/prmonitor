//! Composition-root state, shared across slices via `tauri::State<AppState>`.
//!
//! Slices attach their long-lived handles here as they are implemented
//! (e.g. the scheduler handle in PR4, the codex session manager in PR6).

#[derive(Default)]
pub struct AppState {}
