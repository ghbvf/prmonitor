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
    /// In-flight `claude -p` reviews' kill handles, keyed by session id (#718). A
    /// claude review is a one-shot subprocess (no resident process), so this holds
    /// only each live review's pump-task abort handle for `stop`/shutdown. Methods
    /// take `&self`.
    pub claude: crate::review::engines::claude::ClaudeManager,
    /// Review sessions keyed by `threadId` (PR6). Shared (`Arc` inside) with each
    /// session's streaming pump task; methods take `&self`.
    pub sessions: crate::review::session::SessionRegistry,
    /// The webhook receiver + Cloudflare Quick Tunnel handle (#9). Started/stopped
    /// on demand via the `start_webhook`/`stop_webhook` commands; killed on app
    /// shutdown. Methods take `&self`.
    pub webhook: crate::pr::webhook::WebhookManager,
    /// The resident local REST API listener (AB#1043): a `127.0.0.1`-only axum server that
    /// lets a third party (curl/CLI) trigger a review + poll for completion. Started once in
    /// `lib.rs` `setup()`; NEVER tunneled (distinct from `webhook`); killed on app shutdown.
    /// Methods take `&self`.
    pub local_api: crate::review::local_api::LocalApiManager,
    /// The event inbox's replay-time hooks (AB#1065): the dispatcher + Azure refresh the
    /// `inbox_replay` command re-uses (installed once by the composition root in `setup()`,
    /// like the webhook ingestor/refresher). Methods take `&self`.
    pub inbox: crate::inbox::manager::InboxManager,
    /// The action outbox (AB#1066): the durable side-effect queue + its background worker. Holds
    /// the composition-root-injected executor closure, the worker's wake/stop signals, and the
    /// spawned task handle. Started once in `lib.rs` `setup()`; killed on app shutdown. Methods
    /// take `&self`.
    pub outbox: crate::outbox::manager::OutboxManager,
    /// The review slice's durable-notification producer seam (AB#1066): holds the composition-root-
    /// injected sink that enqueues a `model::Notification` into the action outbox. Lets `review`
    /// produce durable notifications WITHOUT naming the `outbox` slice (the sink closure, installed in
    /// `lib.rs`, is the only place that bridges review→outbox). Methods take `&self`.
    pub notify_outbox: crate::review::notify::NotificationOutbox,
}
