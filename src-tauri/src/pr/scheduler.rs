//! Scheduled-pull loop + manual-pull trigger.
//!
//! A `tokio::time::interval` drives discovery, plus a manual `wake` (the "立即
//! 拉取" button) and live period changes (`reconfigure`). The loop never exits on
//! a discovery error — `run_and_emit` swallows it into a [`PrEvent::Error`] and
//! the loop continues; only `stop` (or an `abort`) tears it down. The first
//! `interval` tick fires immediately (t=0), so `start`/`reconfigure` each trigger
//! an immediate discovery ("启动即跑").

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

        let handle = tauri::async_runtime::spawn(run_loop(
            app,
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

    /// Stops the poll loop: signals `stop` then aborts the task. No-op if not
    /// running.
    pub fn stop(&self) {
        if let Some(task) = self.task.lock().unwrap().take() {
            task.stop.notify_one();
            task.handle.abort();
        }
    }

    /// Triggers an immediate discovery on the running loop (no-op if stopped).
    pub fn wake(&self) {
        if let Some(task) = self.task.lock().unwrap().as_ref() {
            task.wake.notify_one();
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

/// The poll loop. Outer loop rebuilds the ticker on `reconfigure`; inner loop
/// selects between the ticker, a manual wake, reconfigure (break to rebuild),
/// and stop (return to exit). A discovery error never breaks the loop.
async fn run_loop<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    wake: Arc<Notify>,
    reconfigure: Arc<Notify>,
    stop: Arc<Notify>,
) {
    loop {
        let period = resolve_period(config_service::load(&app).map(|c| c.poll_interval_secs));
        let mut ticker = tokio::time::interval(Duration::from_secs(period));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick()          => run_and_emit(&app).await,
                _ = wake.notified()        => run_and_emit(&app).await,
                _ = reconfigure.notified() => break,   // rebuild ticker w/ fresh period
                _ = stop.notified()        => return,
            }
        }
    }
}

/// Runs one discovery cycle and emits the result. A discovery failure is folded
/// into a [`PrEvent::Error`] rather than propagated, so the loop survives it.
async fn run_and_emit<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let event = match super::commands::discover_views(app).await {
        Ok(prs) => PrEvent::Updated { prs },
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
}
