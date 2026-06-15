//! Scheduled-pull loop + manual-pull trigger.
//!
//! A `tokio::time::interval` drives discovery, plus a manual `wake` (the "立即
//! 拉取" button) and live period changes (`reconfigure`). The loop never exits on
//! a discovery error — `discover_emit_snapshot` swallows it into a
//! [`PrEvent::Error`] and the loop continues. The first `interval` tick fires
//! immediately (t=0), so `start`/`reconfigure` each trigger an immediate
//! discovery ("启动即跑").
//!
//! **Graceful stop + unified cancellation domain (F1).** `stop()` never aborts:
//! it signals the `stop` Notify and drops the handle. The loop's inner
//! stop-select sits *both* on the idle wait *and* around the in-flight cycle, so
//! a stop mid-discovery returns immediately and drops the discovery future; the
//! `gh` child it owns dies via `kill_on_drop`. No orphaned subprocess.
//!
//! **Snapshot (F3).** Each successful discovery writes the resulting PR list to a
//! shared `snapshot` *before* emitting `prs:updated`. The frontend reads it on
//! mount via the `get_prs` command, so a `prs:updated` lost to a not-yet-mounted
//! listener no longer strands the UI on an empty list for a full period.
//!
//! **Auto-trigger (#8).** Each cycle also passes its dispatchable candidates (the
//! clean rows) to an injected [`Dispatcher`] hook. The hook is the seam that keeps
//! the `pr` slice review-agnostic: the loop knows nothing about how a review
//! starts, only that a closure consumes `Vec<Candidate>`. The composition root
//! ([`crate::dispatch`]) installs the real dispatcher via [`Scheduler::set_dispatcher`]
//! before `start`, so even the immediate first tick dispatches.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tauri::async_runtime::JoinHandle;
use tauri::Emitter; // for app.emit
use tokio::sync::Notify;
use tokio::time::MissedTickBehavior;

use crate::config::service as config_service;
use crate::error::AppResult;
use crate::events::{PrEvent, PRS_UPDATED_EVENT};
use crate::model::Candidate;

/// Abstract per-cycle dispatch hook: consumes the cycle's dispatchable
/// [`Candidate`]s and drives them to completion (in practice: auto-start their
/// reviews concurrently). Boxed-future + `Arc` so it is `Clone`able into the cycle
/// closure and erased of the review slice's types — the `pr` slice stays
/// review-agnostic (the only cross-slice contract it sees is `Candidate`). The
/// real implementation lives in the composition root ([`crate::dispatch`]).
pub type Dispatcher =
    Arc<dyn Fn(Vec<Candidate>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Default poll period when config is unreadable or non-positive. Mirrors
/// `AppConfig::default().poll_interval_secs`. A 0 period would make
/// `interval(Duration::from_secs(0))` hot-spin, so [`resolve_period`] clamps it.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 120;

/// The composition-root handle for the scheduled-pull loop. Lives in
/// [`crate::state::AppState`]; all methods take `&self` and use interior
/// mutability so a single shared `State<AppState>` can drive it.
#[derive(Default)]
pub struct Scheduler {
    task: StdMutex<Option<RunningTask>>,
    /// Latest successfully discovered PR list (F3). Shared with the loop, which
    /// writes it before each emit; read by `get_prs` so the frontend renders
    /// current state on mount without waiting for the next `prs:updated`.
    snapshot: Arc<StdMutex<Vec<crate::model::PullRequestView>>>,
    /// The auto-trigger dispatch hook (#8), installed by the composition root via
    /// [`Self::set_dispatcher`] before `start`. `Mutex<Option<_>>` defaults to
    /// `None` (so `#[derive(Default)]` still holds) — a `None` dispatcher means a
    /// cycle discovers + emits but starts no reviews (the pre-#8 behavior).
    dispatcher: StdMutex<Option<Dispatcher>>,
}

/// The live task plus the channels the loop selects on.
struct RunningTask {
    handle: JoinHandle<()>,
    wake: Arc<Notify>,
    reconfigure: Arc<Notify>,
    stop: Arc<Notify>,
}

impl Scheduler {
    /// Installs the auto-trigger dispatch hook (#8). Called once by the composition
    /// root *before* `start`, so the immediate first tick already dispatches.
    /// Replaces any prior hook (last writer wins); a never-set dispatcher leaves
    /// cycles discover-and-emit only.
    pub fn set_dispatcher(&self, d: Dispatcher) {
        *self.dispatcher.lock().unwrap() = Some(d);
    }

    /// Spawns the poll loop. Idempotent: if a task is already live this is a
    /// no-op (no double-spawn). A finished task slot is replaced.
    pub fn start<R: tauri::Runtime>(&self, app: tauri::AppHandle<R>) {
        let mut slot = self.task.lock().unwrap();
        if let Some(task) = slot.as_ref() {
            if !task.handle.inner().is_finished() {
                return; // already running
            }
        }

        let wake = Arc::new(Notify::new());
        let reconfigure = Arc::new(Notify::new());
        let stop = Arc::new(Notify::new());

        // Snapshot the installed dispatcher once into the cycle closure: the loop
        // task outlives this `start` call, so it captures an owned `Option<Dispatcher>`
        // rather than re-locking `self` each cycle. `None` ⇒ no auto-trigger.
        let dispatcher = self.dispatcher.lock().unwrap().clone();

        // Production wiring: the period comes from the live config each rebuild,
        // and each cycle discovers → writes the snapshot → emits → dispatches. Both
        // are injected into the generic `run_loop` so the lifecycle is testable (F4).
        let period_provider = {
            let app = app.clone();
            move || resolve_period(config_service::load(&app).map(|c| c.poll_interval_secs))
        };
        let on_cycle = {
            let app = app.clone();
            let snapshot = Arc::clone(&self.snapshot);
            move || {
                let app = app.clone();
                let snapshot = Arc::clone(&snapshot);
                let dispatcher = dispatcher.clone();
                async move { discover_emit_dispatch(&app, &snapshot, dispatcher.as_ref()).await }
            }
        };

        let handle = tauri::async_runtime::spawn(run_loop(
            period_provider,
            on_cycle,
            Arc::clone(&wake),
            Arc::clone(&reconfigure),
            Arc::clone(&stop),
        ));

        *slot = Some(RunningTask {
            handle,
            wake,
            reconfigure,
            stop,
        });
    }

    /// Returns the latest discovered PR list (F3). Cheap clone of the shared
    /// snapshot for the `get_prs` command.
    pub fn snapshot(&self) -> Vec<crate::model::PullRequestView> {
        self.snapshot.lock().unwrap().clone()
    }

    /// Stops the poll loop gracefully (F1). No-op if not running.
    pub fn stop(&self) {
        if let Some(task) = self.task.lock().unwrap().take() {
            task.stop.notify_one();
            // No abort: the loop's stop-select tears the in-flight cycle down,
            // and gh's kill_on_drop kills the child. Dropping `task` detaches it.
        }
    }

    /// Triggers an immediate discovery on the running loop. Returns whether a
    /// running task was actually woken: `false` when stopped (task is `None`),
    /// so the caller (`poll_now`) can surface an error instead of leaving the
    /// frontend waiting for a `prs:updated` event that will never arrive.
    pub fn wake(&self) -> bool {
        if let Some(task) = self.task.lock().unwrap().as_ref() {
            task.wake.notify_one();
            true
        } else {
            false
        }
    }

    /// Asks the running loop to rebuild its ticker with a fresh period (no-op if
    /// stopped). Because the first tick fires immediately, this also triggers an
    /// immediate discovery.
    pub fn reconfigure(&self) {
        if let Some(task) = self.task.lock().unwrap().as_ref() {
            task.reconfigure.notify_one();
        }
    }
}

/// The poll loop, with its period source and per-cycle action injected (F4) so
/// the lifecycle (start/wake/reconfigure/stop) is unit-testable without `gh`,
/// config, or an `AppHandle`. Outer loop rebuilds the ticker on `reconfigure`;
/// inner loop selects between the ticker, a manual wake, reconfigure (break to
/// rebuild), and stop (return to exit).
///
/// The cycle itself runs under a second stop-select: a `stop` arriving mid-cycle
/// returns immediately, dropping the `on_cycle` future — the unified
/// cancellation domain (F1) that lets `gh`'s `kill_on_drop` reap the child.
async fn run_loop<P, C, Fut>(
    period_provider: P,
    on_cycle: C,
    wake: Arc<Notify>,
    reconfigure: Arc<Notify>,
    stop: Arc<Notify>,
) where
    P: Fn() -> u64 + Send + 'static,
    C: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    loop {
        let period = period_provider();
        let mut ticker = tokio::time::interval(Duration::from_secs(period));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick()          => {}
                _ = wake.notified()        => {}
                _ = reconfigure.notified() => break,   // rebuild ticker w/ fresh period
                _ = stop.notified()        => return,
            }
            // Unified cancellation domain: stop interrupts an in-flight cycle,
            // dropping the discovery future → gh's kill_on_drop kills the child.
            tokio::select! {
                _ = on_cycle()      => {}
                _ = stop.notified() => return,
            }
        }
    }
}

/// Runs one discovery cycle: writes the snapshot (F3), emits the result, then
/// auto-triggers the dispatchable candidates (#8). A discovery failure folds into
/// a [`PrEvent::Error`] (the loop survives it) and leaves the snapshot intact —
/// the last good list stays readable via `get_prs` — and yields no dispatchable
/// candidates (nothing is auto-started on a failed cycle). Writing the snapshot
/// *before* the emit means a lost `prs:updated` is still covered by `get_prs`.
///
/// The dispatch is *spawned detached* (not awaited) after the emit — see the body
/// for why the scheduler's stop must not be able to cancel a start in flight; an
/// empty dispatchable list skips the hook entirely.
async fn discover_emit_dispatch<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    snapshot: &Arc<StdMutex<Vec<crate::model::PullRequestView>>>,
    dispatcher: Option<&Dispatcher>,
) {
    let (event, dispatchable) = match super::commands::discover(app).await {
        Ok((prs, dispatchable)) => {
            *snapshot.lock().unwrap() = prs.clone();
            (PrEvent::Updated { prs }, dispatchable)
        }
        // On discovery error the dispatchable list is empty — nothing auto-starts.
        Err(e) => (PrEvent::Error { message: e.message }, Vec::new()),
    };
    let _ = app.emit(PRS_UPDATED_EVENT, &event); // ignore emit error (window may be gone)

    if let Some(d) = dispatcher {
        if !dispatchable.is_empty() {
            // Spawn the dispatch DETACHED rather than awaiting it inline. This cycle
            // runs inside the loop's stop-cancellable `select!` (the F1 cancellation
            // domain that lets a stop reap the in-flight `gh` child). Awaiting
            // `start_review` here would put it in that same domain, so a
            // `stop_polling` landing mid-start would drop a half-started review —
            // leaving a `Starting` session that `stop_review` can't interrupt
            // (`begin_interrupt` is a no-op on `Starting`). The dispatcher future is
            // `Send + 'static`, so the spawned task runs to its terminal
            // (`Running`/`Failed`) regardless of the poll loop. Cross-cycle dedup is
            // unaffected: an in-flight dispatch is still covered by the next
            // discovery's ledger gate and the dispatcher's own in-flight registry
            // guard. Discovery itself stays cancellable (it is awaited above), so a
            // stop still reaps `gh`. The JoinHandle is dropped explicitly (detached):
            // the task runs to completion regardless of the poll loop.
            drop(tauri::async_runtime::spawn(d(dispatchable)));
        }
    }
}

/// Clamps a loaded poll period to a usable value: a positive load passes
/// through; 0 or an error falls back to [`DEFAULT_POLL_INTERVAL_SECS`] (a 0
/// period would hot-spin the interval).
fn resolve_period(loaded: AppResult<u64>) -> u64 {
    match loaded {
        Ok(s) if s > 0 => s,
        _ => DEFAULT_POLL_INTERVAL_SECS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppError;
    use tokio::sync::mpsc;

    #[test]
    fn resolve_period_clamps_zero_to_default() {
        assert_eq!(resolve_period(Ok(0)), DEFAULT_POLL_INTERVAL_SECS);
    }

    #[test]
    fn resolve_period_clamps_error_to_default() {
        assert_eq!(
            resolve_period(Err(AppError::new("x"))),
            DEFAULT_POLL_INTERVAL_SECS
        );
    }

    #[test]
    fn resolve_period_passes_through_positive() {
        assert_eq!(resolve_period(Ok(30)), 30);
    }

    #[test]
    fn default_scheduler_is_not_running() {
        let scheduler = Scheduler::default();
        assert!(scheduler.task.lock().unwrap().is_none());
    }

    #[test]
    fn default_scheduler_has_no_dispatcher() {
        // `#[derive(Default)]` must keep working with the new field: an
        // un-installed dispatcher is `None`, so cycles discover-and-emit only.
        let scheduler = Scheduler::default();
        assert!(scheduler.dispatcher.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn set_dispatcher_stores_and_retrieves_the_hook() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // The hook plumbing in isolation: `set_dispatcher` stores a counting
        // closure; retrieving it (the same `lock().clone()` `start` does) and
        // invoking it with sample candidates must run the closure. This verifies
        // storage/retrieval without needing an `AppHandle` or real dispatch.
        let scheduler = Scheduler::default();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(AtomicUsize::new(0));
        let dispatcher: Dispatcher = {
            let count = Arc::clone(&count);
            let seen = Arc::clone(&seen);
            Arc::new(move |cands: Vec<Candidate>| {
                let count = Arc::clone(&count);
                let seen = Arc::clone(&seen);
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    seen.fetch_add(cands.len(), Ordering::SeqCst);
                })
            })
        };
        scheduler.set_dispatcher(dispatcher);

        let stored = scheduler
            .dispatcher
            .lock()
            .unwrap()
            .clone()
            .expect("set_dispatcher stores the hook");
        stored(vec![candidate(1, "review"), candidate(2, "check")]).await;

        assert_eq!(count.load(Ordering::SeqCst), 1, "hook ran once");
        assert_eq!(seen.load(Ordering::SeqCst), 2, "hook saw both candidates");
    }

    fn candidate(number: u64, kind: &str) -> Candidate {
        Candidate {
            number,
            head_sha: "sha".to_string(),
            head_ref: "ref".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            kind: kind.to_string(),
        }
    }

    // ── Lifecycle tests (F4) ───────────────────────────────────────────────
    // Drive `run_loop` directly with injected boundaries: a period provider
    // returning 3600s (the real ticker won't fire during the test, so every
    // cycle observed is driven by an explicit `wake`/`reconfigure`/immediate
    // first tick), and an `on_cycle` that signals an `mpsc` channel. Assertions
    // use `timeout` on `recv`/the JoinHandle, never sleeps, so they're
    // deterministic. These cover the seam F1 (graceful stop) and F4 (DI) open.

    /// Spawns `run_loop` with an injected cycle-counter channel and a 3600s
    /// period (so only explicit signals or the immediate first tick drive a
    /// cycle). Returns the cycle-recv channel, the three control Notifies, and
    /// the JoinHandle.
    #[allow(clippy::type_complexity)]
    fn spawn_test_loop() -> (
        mpsc::UnboundedReceiver<()>,
        Arc<Notify>,
        Arc<Notify>,
        Arc<Notify>,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel::<()>();
        let wake = Arc::new(Notify::new());
        let reconfigure = Arc::new(Notify::new());
        let stop = Arc::new(Notify::new());

        let on_cycle = move || {
            let tx = tx.clone();
            async move {
                let _ = tx.send(());
            }
        };

        let handle = tokio::spawn(run_loop(
            || 3600, // huge period: real ticker never fires during the test
            on_cycle,
            Arc::clone(&wake),
            Arc::clone(&reconfigure),
            Arc::clone(&stop),
        ));
        (rx, wake, reconfigure, stop, handle)
    }

    /// Awaits one cycle signal, failing the test if none arrives in time.
    async fn expect_cycle(rx: &mut mpsc::UnboundedReceiver<()>) {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a cycle should run before timeout")
            .expect("on_cycle channel should stay open");
    }

    #[tokio::test]
    async fn run_loop_runs_immediate_first_cycle() {
        let (mut rx, _wake, _reconfigure, stop, handle) = spawn_test_loop();
        // The interval's first tick fires at t=0 → "启动即跑".
        expect_cycle(&mut rx).await;
        stop.notify_one();
        handle.await.expect("loop should join after stop");
    }

    #[tokio::test]
    async fn run_loop_wake_triggers_a_cycle() {
        let (mut rx, wake, _reconfigure, stop, handle) = spawn_test_loop();
        expect_cycle(&mut rx).await; // immediate first tick
        wake.notify_one();
        expect_cycle(&mut rx).await; // manual wake → another cycle
        stop.notify_one();
        handle.await.expect("loop should join after stop");
    }

    #[tokio::test]
    async fn run_loop_reconfigure_rebuilds_and_runs_immediate_cycle() {
        let (mut rx, _wake, reconfigure, stop, handle) = spawn_test_loop();
        expect_cycle(&mut rx).await; // immediate first tick
        reconfigure.notify_one();
        // Rebuilding the ticker fires a fresh immediate first tick → a cycle.
        expect_cycle(&mut rx).await;
        stop.notify_one();
        handle.await.expect("loop should join after stop");
    }

    #[tokio::test]
    async fn run_loop_stop_makes_the_task_finish() {
        let (mut rx, _wake, _reconfigure, stop, handle) = spawn_test_loop();
        expect_cycle(&mut rx).await; // ensure the loop is up
        stop.notify_one();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("stop should let the spawned task finish")
            .expect("loop task should not panic");
    }
}
