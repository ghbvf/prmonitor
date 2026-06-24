//! Inbox composition handles (AB#1065): the replay-time re-feed + Azure refresh hooks,
//! installed once by the composition root and read by the `inbox_replay` command.
//!
//! `inbox_replay` is a Tauri command (NOT a webhook delivery), so it has no live route snapshot
//! and no in-flight dispatcher/refresher. It re-feeds a GitHub entry through the SAME dispatch path
//! the scheduler/webhook use (via the OPAQUE [`GithubRefeed`] closure the root installs — which
//! captures the `ProjectDispatcher` in `lib.rs`), and re-invokes the SAME `az` re-discovery (via
//! [`AzureRefresh`]). Holding only these neutral closures keeps the inbox slice from naming ANY
//! pr-internal type. The composition root installs them at `setup` time, exactly as it installs
//! the webhook ingestor/refresher on the pr slice's WebhookManager; the slice reads them back
//! through this manager — a `tauri::State` field on [`crate::state::AppState`].

use std::sync::Mutex as StdMutex;

use crate::inbox::{AzureRefresh, GithubRefeed};

/// Holds the replay-time hooks the composition root installs (AB#1065). `&self` methods +
/// interior mutability so it lives in [`crate::state::AppState`] (which stays `Default`),
/// mirroring the pr slice's WebhookManager `set_ingestor` lifecycle. Holds only the OPAQUE
/// inbox-local closure types — NO pr type — so the inbox slice stays decoupled.
#[derive(Default)]
pub struct InboxManager {
    /// The GitHub re-feed hook (wraps the SAME `ProjectDispatcher` + `ingest_webhook` the live
    /// ingress uses, captured in `lib.rs`). Installed once before any replay; `None` until then
    /// (so `#[derive(Default)]` holds — a replay before install is a fail-closed error, not a panic).
    github_refeed: StdMutex<Option<GithubRefeed>>,
    /// The Azure re-discovery hook (the SAME `discover_once` wrapper the Azure refresh uses).
    refresher: StdMutex<Option<AzureRefresh>>,
}

impl InboxManager {
    /// Install the replay hooks (composition root, before any replay). Replaces any prior hooks
    /// (last writer wins), mirroring `WebhookManager::set_ingestor`.
    pub fn set_hooks(&self, github_refeed: GithubRefeed, refresher: AzureRefresh) {
        *self.github_refeed.lock().unwrap() = Some(github_refeed);
        *self.refresher.lock().unwrap() = Some(refresher);
    }

    /// The installed GitHub re-feed hook, or `None` if the root hasn't wired it yet (a replay then
    /// fails closed). A clone so the lock is released before the async replay runs.
    pub fn github_refeed(&self) -> Option<GithubRefeed> {
        self.github_refeed.lock().unwrap().clone()
    }

    /// The installed Azure refresh hook, or `None` if not wired yet.
    pub fn refresher(&self) -> Option<AzureRefresh> {
        self.refresher.lock().unwrap().clone()
    }
}
