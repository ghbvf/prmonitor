//! Outbox worker lifecycle (AB#1066): owns the background task that drains the `action_outbox`
//! queue + its wake/stop signals. A `tauri::State` field on [`crate::state::AppState`], started once
//! by the composition root in `lib.rs` `setup()` (with the injected executor) and killed on app
//! shutdown — mirroring the inbox's `InboxManager` lifecycle and the `pr::scheduler` loop shape.
//!
//! The worker loop is a `tokio::select!` over an interval TICK (so due retries fire even with no new
//! work), an enqueue WAKE (so a freshly produced action runs promptly), and a STOP signal. The
//! FIRST interval tick fires immediately, so on startup the worker drains any rows persisted before
//! a previous run exited — the "restart-resume" half of the acceptance.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use tauri::async_runtime::{spawn, JoinHandle};
use tauri::Manager;
use tokio::sync::Notify;
use tokio::time::{interval, MissedTickBehavior};

use crate::db::Database;
use crate::outbox::{service, ActionExecutor};

/// How often the worker wakes to sweep for due retries even with no new enqueue (AB#1066). Enqueue
/// also `wake()`s the loop, so this interval only bounds RETRY latency, not first-run latency.
const WORKER_TICK_SECS: u64 = 30;

/// Holds the outbox worker task + its signals (AB#1066). `&self` methods + interior mutability so it
/// lives in [`crate::state::AppState`] (which stays `Default`), mirroring `InboxManager` /
/// `WebhookManager`. Holds NO `review`/`pr` type — the executor is captured by the spawned task.
#[derive(Default)]
pub struct OutboxManager {
    /// Nudged by `enqueue` so a freshly produced action runs without waiting for the next tick.
    wake: Arc<Notify>,
    /// Signals the worker loop to exit (paired with the task `abort` on shutdown).
    stop: Arc<Notify>,
    /// The spawned worker task handle; `None` until `start`. `Some` after start so a second `start`
    /// is a no-op (idempotent) and `shutdown` can abort it.
    task: StdMutex<Option<JoinHandle<()>>>,
}

impl OutboxManager {
    /// Start the worker loop (composition root, in `setup()`). Idempotent: a second call while a
    /// task is live is a no-op. `executor` is the injected side-effect router (the only place naming
    /// `review::notify`, in `lib.rs`); the spawned task owns it, so the manager itself names no
    /// foreign type.
    pub fn start(&self, app: tauri::AppHandle, executor: ActionExecutor) {
        let mut task = self.task.lock().unwrap();
        if task.is_some() {
            return;
        }
        let wake = Arc::clone(&self.wake);
        let stop = Arc::clone(&self.stop);
        *task = Some(spawn(worker_loop(app, executor, wake, stop)));
    }

    /// Wake the worker to sweep now (called by `service::enqueue` and `outbox_retry`). A no-op if
    /// the worker hasn't started; `notify_one` stores a permit so a wake just before the loop parks
    /// is not lost.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Stop the worker (app shutdown). Signals the loop AND aborts the task as the hard backstop
    /// (the loop only `.await`s between DB transactions, so an abort can't tear a transaction). A
    /// no-op if never started.
    pub fn shutdown(&self) {
        self.stop.notify_waiters();
        if let Some(handle) = self.task.lock().unwrap().take() {
            handle.abort();
        }
    }
}

/// The worker loop body (AB#1066): tick / wake / stop → run one due-sweep cycle, repeat. Owns the
/// `app` + injected `executor`; re-resolves the [`Database`] from managed state each cycle (the
/// `'static` task can't borrow a caller's handle). The first interval tick is immediate, so a fresh
/// start drains the pending backlog at once (restart-resume).
async fn worker_loop(
    app: tauri::AppHandle,
    executor: ActionExecutor,
    wake: Arc<Notify>,
    stop: Arc<Notify>,
) {
    let mut ticker = interval(Duration::from_secs(WORKER_TICK_SECS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = wake.notified() => {}
            _ = stop.notified() => return,
        }
        let db = app.state::<Database>();
        // Run the cycle to completion, but let a stop signal cut it short (unified cancellation,
        // mirroring `pr::scheduler::run_loop`).
        tokio::select! {
            _ = service::run_due_once(&app, db.inner(), &executor) => {}
            _ = stop.notified() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The manager is `Default`-constructible and its signals are no-ops before `start` (AB#1066):
    // `wake` / `shutdown` on an unstarted manager must not panic (it lives in a `Default` AppState).
    #[test]
    fn default_wake_and_shutdown_are_noops_before_start() {
        let m = OutboxManager::default();
        m.wake();
        m.shutdown(); // takes a `None` task handle — no panic
    }
}
