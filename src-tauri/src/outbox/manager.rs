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

use crate::config::service as config_service;
use crate::db::Database;
use crate::outbox::{service, ActionExecutor, ClaimReleaser};

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
    ///
    /// **AB#1204 ordering guard (F3, Medium runtime guard).** `before_worker` is an OPAQUE
    /// composition-root closure (same review-blind injection shape as [`ActionExecutor`] /
    /// [`ClaimReleaser`] — the manager sees only `FnOnce`, never a `review` type) that MUST run
    /// before the worker can process ANY row. We invoke it SYNCHRONOUSLY here, BEFORE the worker task
    /// is spawned — so it is impossible for the worker to claim/execute a row before it has run. In
    /// `lib.rs` this closure is the startup `fail_orphaned_sessions` reconcile: a crash-replayed
    /// outbox review re-enters `start_for_outbox` and reads its claim's `review_session` row, which
    /// MUST already be reconciled to a stable terminal status (a mid-flight session flipped to
    /// `failed`, not left `running`) for `replayed_review_should_suppress` to decide correctly.
    /// Folding the order INTO `start` replaces the previous Soft "keep this call above `outbox.start`"
    /// comment guard: reconcile-not-run-before-worker is now unrepresentable rather than convention.
    pub fn start(
        &self,
        app: tauri::AppHandle,
        executor: ActionExecutor,
        release_terminal: ClaimReleaser,
        before_worker: impl FnOnce() + Send + 'static,
    ) {
        // The real spawn step is deferred to `start_with_spawn` so the order-sensitive control flow
        // (idempotency gate → `before_worker` → spawn) is unit-testable with a FAKE spawn closure
        // (no `AppHandle` / tokio runtime needed) — see the `before_worker_runs_before_spawn` test.
        let wake = Arc::clone(&self.wake);
        let stop = Arc::clone(&self.stop);
        self.start_with_spawn(before_worker, move || {
            spawn(worker_loop(app, executor, release_terminal, wake, stop))
        });
    }

    /// The order-sensitive core of [`start`](Self::start), parameterized over the SPAWN step so the
    /// AB#1204 F3 ordering invariant is unit-testable without an `AppHandle`. Idempotent: a second
    /// call while a task is live is a no-op (and does NOT re-run `before_worker`). Otherwise it runs
    /// `before_worker()` SYNCHRONOUSLY, THEN `spawn_worker()` to start the task — so the worker can
    /// never process a row before the pre-worker reconcile has completed.
    fn start_with_spawn(
        &self,
        before_worker: impl FnOnce(),
        spawn_worker: impl FnOnce() -> JoinHandle<()>,
    ) {
        let mut task = self.task.lock().unwrap();
        if task.is_some() {
            return;
        }
        // Run the injected pre-worker step (AB#1204: orphan-session reconcile) SYNCHRONOUSLY before
        // the worker exists. The worker task is spawned only AFTER this returns, so no row can be
        // processed ahead of it — the load-bearing ORDERING is enforced by control flow, not a note.
        before_worker();
        *task = Some(spawn_worker());
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
    ///
    /// Uses `notify_one` (NOT `notify_waiters`): `notify_one` STORES a permit when no waiter is
    /// currently parked, so a stop that fires while the loop is BETWEEN its two `select!` blocks is
    /// consumed by the next `stop.notified()` rather than lost — the same permit-storing stop the
    /// `pr::scheduler` loop relies on. (The `abort` is still the hard backstop, but the graceful
    /// path is now reliable on its own.)
    pub fn shutdown(&self) {
        self.stop.notify_one();
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
    release_terminal: ClaimReleaser,
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
        // Resolve the staleness TTL from LIVE config each cycle (AB#1182), through the config
        // slice's PUBLIC `load` seam (the same entry `pr::scheduler` uses for its poll interval),
        // so a config edit applies without a restart. A config-read failure degrades to the default
        // TTL rather than skipping the cycle — but is LOGGED (review F4), so a persistent read
        // failure doesn't silently pin the default forever. The fallback default also comes through
        // the config PUBLIC service seam (`default_outbox_config`, AB#1182 F1) — the outbox slice
        // never reaches into `config::model` for the constant.
        let notification_ttl_secs = match config_service::load(&app) {
            Ok(cfg) => cfg.outbox.notification_ttl_secs,
            Err(e) => {
                eprintln!(
                    "outbox: 读取配置失败，使用默认 staleness TTL：{}",
                    e.message
                );
                config_service::default_outbox_config().notification_ttl_secs
            }
        };
        // Run the cycle to completion, but let a stop signal cut it short (unified cancellation,
        // mirroring `pr::scheduler::run_loop`).
        tokio::select! {
            _ = service::run_due_once(&app, db.inner(), &executor, &release_terminal, notification_ttl_secs) => {}
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

    // AB#1204 F3 ordering guard (Medium runtime guard): `start` runs `before_worker` BEFORE spawning
    // the worker — so the orphan-session reconcile cannot be raced by row processing. Driven through
    // `start_with_spawn` with a FAKE spawn closure (no AppHandle): a shared order log records that
    // "reconcile" (the `before_worker` body) is pushed BEFORE "spawn-worker" (the spawn step).
    #[tokio::test]
    async fn before_worker_runs_before_spawn() {
        use std::sync::Mutex as StdMutex;

        let order: Arc<StdMutex<Vec<&'static str>>> = Arc::new(StdMutex::new(Vec::new()));
        let m = OutboxManager::default();

        let before = {
            let order = Arc::clone(&order);
            move || order.lock().unwrap().push("reconcile")
        };
        let spawn_worker = {
            let order = Arc::clone(&order);
            move || -> JoinHandle<()> {
                order.lock().unwrap().push("spawn-worker");
                // A real (trivial) worker handle so `task` is `Some` afterwards (idempotency).
                spawn(async {})
            }
        };

        m.start_with_spawn(before, spawn_worker);
        assert_eq!(
            *order.lock().unwrap(),
            vec!["reconcile", "spawn-worker"],
            "before_worker (reconcile) must run BEFORE the worker is spawned"
        );

        // Idempotent second start: a live task short-circuits, so `before_worker` does NOT re-run
        // (a re-run would re-reconcile pointlessly and, worse, imply the worker could restart).
        let reran = Arc::new(StdMutex::new(false));
        let before2 = {
            let reran = Arc::clone(&reran);
            move || *reran.lock().unwrap() = true
        };
        m.start_with_spawn(before2, || -> JoinHandle<()> {
            panic!("a second start while a task is live must NOT spawn again")
        });
        assert!(
            !*reran.lock().unwrap(),
            "an idempotent second start must not re-run before_worker"
        );
    }
}
