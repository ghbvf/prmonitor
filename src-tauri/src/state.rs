//! Composition-root state, shared across slices via `tauri::State<AppState>`.
//!
//! Slices attach their long-lived handles here as they are implemented
//! (e.g. the scheduler handle in PR4, the review session manager in PR6).

#[derive(Default)]
pub struct AppState {
    /// The per-project scheduled-pull loops (#35): a `project_id → Scheduler` set the
    /// composition root reconciles to the enabled projects. Long-lived; methods take
    /// `&self`. Pre-#35 this was a single `Scheduler`; now one app drives N parallel
    /// poll loops, one per enabled project.
    pub scheduler: crate::pr::scheduler::SchedulerSet,
    /// The resident codex app-server connection (PR5). Lazily started, kept alive
    /// so reviews start fast; killed on app shutdown. Methods take `&self`.
    pub codex: crate::review::engines::codex::CodexManager,
    /// Review sessions keyed by `threadId` (PR6). Shared (`Arc` inside) with each
    /// session's streaming pump task; methods take `&self`.
    pub sessions: crate::review::session::SessionRegistry,
    /// The webhook receiver + Cloudflare Quick Tunnel handle (#9). Started/stopped
    /// on demand via the `start_webhook`/`stop_webhook` commands; killed on app
    /// shutdown. Methods take `&self`.
    pub webhook: crate::pr::webhook::WebhookManager,
}
