//! Composition-root state, shared across slices via `tauri::State<AppState>`.
//!
//! Slices attach their long-lived handles here as they are implemented
//! (e.g. the scheduler handle in PR4, the review session manager in PR6).

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use crate::config::model::AppConfig;
use crate::error::{AppError, AppResult};
use crate::model::{SendNotificationRequest, SendNotificationResponse};

/// The composition-root-injected post-save reconcile closure. Given the just-saved config, it
/// drives Remote Access listener and tunnel reconcile. OPAQUE on purpose (an `Arc<dyn Fn>`
/// mirroring `review::notify::NotificationSink`): the `config` slice holds only this type, never
/// references `crate::remote`. Aliased so the field type stays simple (clippy `type_complexity`).
pub type ConfigSavedSink = Arc<dyn Fn(AppConfig) + Send + Sync>;
pub type NotificationSendSink = Arc<
    dyn Fn(SendNotificationRequest, Option<String>) -> AppResult<SendNotificationResponse>
        + Send
        + Sync,
>;

/// The post-`set_config`-save hook seam (AB#1225 F4): a composition-root-injected closure the
/// `config` slice fires AFTER a successful save, so config never names a sibling horizontal
/// (`crate::remote`) to drive listener reconcile. Mirrors the established AppState-injected-closure
/// seams (`review::notify::NotificationOutbox` sink, `pr::webhook::WebhookManager` ingestor): the
/// closure — installed once in `lib.rs` `setup()` — is the SOLE place that bridges config→remote.
/// OPAQUE on purpose: `config::commands` holds only this `dyn Fn`, never references `crate::remote`.
#[derive(Default)]
pub struct ConfigSavedHook {
    hook: StdMutex<Option<ConfigSavedSink>>,
}

impl ConfigSavedHook {
    /// Install the post-save hook (composition root, in `setup()`). Replaces any prior hook (last
    /// writer wins), mirroring `NotificationOutbox::set_sink` / `WebhookManager::set_ingestor`.
    pub fn set_hook(&self, hook: ConfigSavedSink) {
        *self.hook.lock().unwrap_or_else(|p| p.into_inner()) = Some(hook);
    }

    /// Fire the installed hook with the just-saved config. Clones the `Arc` out of the lock
    /// before calling so the lock isn't held across the (best-effort) reconcile. Poison-safe
    /// (`into_inner`): a panicked prior holder must not panic-cascade every subsequent save. A
    /// no-op if the root hasn't installed the hook yet (never in practice — a save before `setup`
    /// completes), so a missing hook silently skips reconcile rather than failing the save.
    pub fn fire(&self, config: AppConfig) {
        let hook = self.hook.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(hook) = hook {
            hook(config);
        }
    }
}

/// Composition-root-injected user-notification send seam (#1460). Transports that live inside an
/// existing slice (local API / deeplink) call this opaque sender instead of naming the horizontal
/// notification module directly; `lib.rs` installs the concrete outbox-backed funnel.
#[derive(Default)]
pub struct NotificationSender {
    sink: StdMutex<Option<NotificationSendSink>>,
}

impl NotificationSender {
    pub fn set_sink(&self, sink: NotificationSendSink) {
        *self.sink.lock().unwrap_or_else(|p| p.into_inner()) = Some(sink);
    }

    pub fn send(
        &self,
        request: SendNotificationRequest,
        dedupe_prefix: Option<String>,
    ) -> AppResult<SendNotificationResponse> {
        let sink = self.sink.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let Some(sink) = sink else {
            return Err(AppError::new("notification sender 未初始化".to_string()));
        };
        sink(request, dedupe_prefix)
    }
}

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
    /// The Remote Access listener binding supervisor (AB#1225): reconciles `config.listeners[]`
    /// to bound loopback listeners. local-api is the sole real binder this PR — it mounts
    /// `review::local_api::build_router` on the port from its `listeners[]` entry (the resident
    /// `LocalApiManager` was removed; `listeners[]` is now the single source of truth). Reconciled
    /// in `lib.rs` `setup()` + after each `set_config`; killed on app shutdown. Methods take `&self`.
    pub remote: crate::remote::supervisor::ListenerSupervisor,
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
    /// The user-authored notification send funnel (#1460), injected by the composition root so
    /// transports can share one sender without reaching across slice boundaries.
    pub notification_sender: NotificationSender,
    /// The post-`set_config`-save hook (AB#1225 F4): holds the composition-root-injected closure
    /// that reconciles the Remote Access listener runtime after a save. Lets the `config` slice's
    /// `set_config` take effect on the listener runtime WITHOUT naming `crate::remote` (the closure,
    /// installed in `lib.rs`, is the sole bridge config→remote). Methods take `&self`.
    pub config_saved: ConfigSavedHook,
    /// The realtime stream bus (AB#1072 / #1373): the in-process broadcast hub the
    /// [`crate::stream::emit`] funnel publishes review/action [`crate::events::StreamEvent`]s to,
    /// and that the local-api SSE endpoint subscribes to. `Default` (empty channel until the first
    /// `subscribe`); methods take `&self`.
    pub stream: crate::stream::StreamBus,
    /// The resident iTerm daemon connection (#1383). Lazily started, kept alive so terminal
    /// commands are fast; killed on app shutdown. Owns one per-connection notification pump that
    /// pushes `TerminalEvent`s through the `crate::stream::emit` funnel. Methods take `&self`.
    pub terminal: crate::terminal::manager::ITermDaemonManager,
    /// The resident Web PTY session pool (#1372): the SECOND terminal backend's owned state — a
    /// `portable-pty`-spawned shell per session, each with a reader thread streaming output through
    /// the `crate::stream::emit` funnel. `Default` (empty until the first `create`); killed on app
    /// shutdown. Methods take `&self`.
    pub web_pty: crate::terminal::webpty_manager::WebPtyManager,
}
