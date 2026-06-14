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

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tauri::async_runtime::JoinHandle;
use tauri::Emitter; // for app.emit
use tokio::sync::Notify;
use tokio::time::MissedTickBehavior;

use crate::config::service as config_service;
use crate::error::AppResult;
use crate::events::{PrEvent, PRS_UPDATED_EVENT};

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
}

/// The live task plus the channels the loop selects on.
struct RunningTask {
    handle: JoinHandle<()>,
    wake: Arc<Notify>,
    reconfigure: Arc<Notify>,
    stop: Arc<Notify>,
}

impl Scheduler {
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

        // Production wiring: the period comes from the live config each rebuild,
        // and each cycle discovers → writes the snapshot → emits. Both are
        // injected into the generic `run_loop` so the lifecycle is testable (F4).
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
                async move { discover_emit_snapshot(&app, &snapshot).await }
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

/// Runs one discovery cycle: writes the snapshot (F3) then emits the result. A
/// discovery failure folds into a [`PrEvent::Error`] (the loop survives it) and
/// leaves the snapshot intact — the last good list stays readable via `get_prs`.
/// Writing the snapshot *before* the emit means a lost `prs:updated` is still
/// covered by `get_prs`.
async fn discover_emit_snapshot<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    snapshot: &Arc<StdMutex<Vec<crate::model::PullRequestView>>>,
) {
    let event = match super::commands::discover_views(app).await {
        Ok(prs) => {
            *snapshot.lock().unwrap() = prs.clone();
            PrEvent::Updated { prs }
        }
        Err(e) => PrEvent::Error { message: e.message },
    };
    let _ = app.emit(PRS_UPDATED_EVENT, &event); // ignore emit error (window may be gone)
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
