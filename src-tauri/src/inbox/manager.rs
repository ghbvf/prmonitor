//! Inbox single-consumer worker + composition handles (AB#1065/#1379). The composition root
//! installs provider re-feed, Azure refresh, and rule-processing hooks once; both first processing
//! and explicit failed-row replay are executed only by this worker.
//!
//! `inbox_replay` is a Tauri command (NOT a webhook delivery), so it has no live route snapshot
//! and no in-flight dispatcher/refresher. It re-feeds a GitHub entry through the SAME dispatch path
//! the scheduler/webhook use (via the OPAQUE [`GithubRefeed`] closure the root installs — which
//! captures the composition-root refeed in `lib.rs`), and re-invokes the SAME `az` re-discovery (via
//! [`AzureRefresh`]). Candidate-backed rows replay through [`RuleProcessor`], so replay uses the
//! same rule-engine funnel as first processing. The composition
//! root installs these at `setup` time, exactly as it installs the webhook ingestor/refresher on the
//! pr slice's WebhookManager; the slice reads them back through this manager — a `tauri::State`
//! field on [`crate::state::AppState`].

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use crate::inbox::{AzureRefresh, GithubRefeed, RuleProcessor};
use tauri::async_runtime::{spawn, JoinHandle};
use tauri::Manager;
use tokio::sync::Notify;

const FAILURE_BACKOFF: Duration = Duration::from_millis(250);

/// Holds the worker hooks the composition root installs (AB#1065). `&self` methods +
/// interior mutability so it lives in [`crate::state::AppState`] (which stays `Default`),
/// mirroring the pr slice's WebhookManager `set_ingestor` lifecycle. Holds only the OPAQUE
/// inbox-local closure types — NO pr type — so the inbox slice stays decoupled.
#[derive(Default)]
pub struct InboxManager {
    /// The GitHub re-feed hook (wraps the SAME `ingest_webhook` the live
    /// ingress uses, captured in `lib.rs`). Installed once before any replay; `None` until then
    /// (so `#[derive(Default)]` holds — a replay before install is a fail-closed error, not a panic).
    github_refeed: StdMutex<Option<GithubRefeed>>,
    /// The Azure re-discovery hook (the SAME `discover_once` wrapper the Azure refresh uses).
    refresher: StdMutex<Option<AzureRefresh>>,
    /// The rule processor hook (composition root owns rule/outbox wiring).
    rule_processor: StdMutex<Option<RuleProcessor>>,
    wake: Arc<Notify>,
    stop: Arc<Notify>,
    task: StdMutex<Option<JoinHandle<()>>>,
}

impl InboxManager {
    /// Install the worker hooks (composition root, before the worker starts). Replaces prior hooks
    /// (last writer wins), mirroring `WebhookManager::set_ingestor`.
    pub fn set_hooks(
        &self,
        github_refeed: GithubRefeed,
        refresher: AzureRefresh,
        rule_processor: RuleProcessor,
    ) {
        *self.github_refeed.lock().unwrap() = Some(github_refeed);
        *self.refresher.lock().unwrap() = Some(refresher);
        *self.rule_processor.lock().unwrap() = Some(rule_processor);
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

    /// The installed rule processor hook, or `None` if not wired yet.
    pub fn rule_processor(&self) -> Option<RuleProcessor> {
        self.rule_processor.lock().unwrap().clone()
    }

    pub fn start(&self, app: tauri::AppHandle) {
        let mut task = self.task.lock().unwrap();
        if task.is_some() {
            return;
        }
        let wake = Arc::clone(&self.wake);
        let stop = Arc::clone(&self.stop);
        *task = Some(spawn(async move {
            loop {
                let state = app.state::<crate::state::AppState>();
                let Some(github) = state.inbox.github_refeed() else {
                    return;
                };
                let Some(azure) = state.inbox.refresher() else {
                    return;
                };
                let Some(rules) = state.inbox.rule_processor() else {
                    return;
                };
                let db = app.state::<crate::db::Database>();
                match crate::inbox::store::received_ids(db.inner()) {
                    Ok(ids) if !ids.is_empty() => {
                        let mut terminalize_failed = false;
                        for id in ids {
                            tokio::select! {
                                result = crate::inbox::service::process_received(
                                    &app,
                                    db.inner(),
                                    &github,
                                    &azure,
                                    &rules,
                                    id,
                                ) => {
                                    if let Err(e) = result {
                                        terminalize_failed = true;
                                        crate::inbox::service::announce_worker_error(
                                            &app, "worker", &e.message,
                                        );
                                    }
                                }
                                _ = stop.notified() => return,
                            }
                        }
                        if terminalize_failed {
                            tokio::select! {
                                _ = tokio::time::sleep(FAILURE_BACKOFF) => {},
                                _ = stop.notified() => return,
                            }
                        }
                        continue;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        crate::inbox::service::announce_worker_error(&app, "worker", &e.message);
                        tokio::select! {
                            _ = tokio::time::sleep(FAILURE_BACKOFF) => {},
                            _ = stop.notified() => return,
                        }
                        continue;
                    }
                }
                tokio::select! {
                    _ = wake.notified() => {},
                    _ = stop.notified() => return,
                }
            }
        }));
        self.wake();
    }

    pub fn wake(&self) {
        self.wake.notify_one();
    }

    pub fn shutdown(&self) {
        self.stop.notify_one();
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }
}
