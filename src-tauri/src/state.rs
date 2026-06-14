//! Composition-root state, shared across slices via `tauri::State<AppState>`.
//!
//! Slices attach their long-lived handles here as they are implemented
//! (e.g. the scheduler handle in PR4, the review session manager in PR6).

#[derive(Default)]
pub struct AppState {
    /// The scheduled-pull loop handle (PR4). Long-lived; methods take `&self`.
    pub scheduler: crate::pr::scheduler::Scheduler,
    /// The resident codex app-server connection (PR5). Lazily started, kept alive
    /// so reviews start fast; killed on app shutdown. Methods take `&self`.
    pub codex: crate::review::engines::codex::CodexManager,
}
