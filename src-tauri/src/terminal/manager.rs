//! Resident iTerm daemon connection manager — a long-lived handle held in `AppState`
//! (mirrors `review::engines::codex::CodexManager`). A single `python3 iterm_daemon.py`
//! child is handshaken once and kept alive so terminal ops are fast (no respawn); every
//! command reuses the one connection. The child is killed when the app closes
//! ([`Self::shutdown`], wired to `RunEvent` in `lib.rs`).
//!
//! UNLIKE `CodexManager` (whose per-review pumps live in `review::session`), this manager
//! owns ONE per-connection pump: when it installs a fresh process it spawns a task that
//! maps every daemon [`ServerNotification`] (via the pure [`super::iterm::map_notification`])
//! into a [`crate::events::TerminalEvent`] and pushes it through the [`crate::stream::emit`]
//! funnel. There is a single screen stream per daemon (not per review), so one pump suffices.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Runtime};
use tokio::process::ChildStdin;
use tokio::sync::broadcast;

use super::process::{ITermDaemonProcess, TerminalDaemonStatus};
use super::protocol::{ServerNotification, ERR_DAEMON_CLOSED, ERR_DAEMON_STOPPED};
use super::rpc::RpcClient;
use crate::error::{AppError, AppResult};
use crate::events::{StreamEvent, TerminalEvent};

/// Whole-handshake budget for a cold `ensure_started` (spawn + initialize), so a probe
/// can't hang even if python / iTerm stalls.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// The resident process paired with its per-connection pump task. They are installed and torn
/// down as a UNIT (one `Option` under the std `Mutex`), so the pump can never outlive its
/// process: a stale pump from an old connection cannot emit a late connection-level `Error`
/// after a fast stop→restart→attach has re-installed a fresh session.
struct Resident {
    proc: ITermDaemonProcess,
    /// The pump task handle, `abort()`ed whenever this `Resident` is replaced (cold restart)
    /// or taken (`shutdown`/`stop`). Mirrors how `RpcClient` aborts its reader on drop.
    pump: tauri::async_runtime::JoinHandle<()>,
}

/// Owns the resident iTerm daemon connection. `&self` methods + interior mutability so it
/// can live in `AppState` (which stays `Default`).
#[derive(Default)]
pub struct ITermDaemonManager {
    /// The resident process + its pump, swapped/torn down atomically (single std `Mutex`).
    inner: Arc<Mutex<Option<Resident>>>,
    /// Serializes the cold-start slow path so concurrent `ensure_started` calls spawn at most
    /// one daemon (the std `Mutex` above can't be held across the spawn/handshake await).
    start_lock: tokio::sync::Mutex<()>,
    /// 用户显式停止标记（来自 `stop`/`stop_terminal_daemon`）。true 时被动 `status` 探测短路为
    /// 「已停止」、不自动拉起，`ensure_started` 直接拒绝。动作命令在调用前 `resume()`（把一次
    /// 终端操作当作显式启动意图，镜像手动 review `resume` codex 的语义）。默认 false = lazy。
    stopped: AtomicBool,
}

impl ITermDaemonManager {
    /// Ensure the resident connection is live and return a cloned, callable handle to its
    /// JSON-RPC client. Reuses an existing connected process; otherwise spawns + handshakes
    /// once, caches it, AND spawns the per-connection pump (so screen-update notifications
    /// reach the frontend). Self-heals: if the prior process died (reader cleared its liveness
    /// flag), this respawns. REFUSES when the user explicitly stopped the daemon — a terminal
    /// command `resume()`s first (its action is explicit intent), a passive `status` does not.
    ///
    /// Stays `pub` (not `pub(crate)`): the `#[ignore]`d integration test `tests/iterm_daemon.rs`
    /// calls it directly, and `tests/` is a separate crate that can only reach `pub` items.
    pub async fn ensure_started<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        python_bin: &str,
        script: &str,
    ) -> AppResult<Arc<RpcClient<ChildStdin>>> {
        // Authoritative stop refusal: a user-stopped daemon is not revived here. Action
        // commands `resume()` before reaching this; a passive `status` short-circuits earlier.
        if self.stopped.load(Ordering::SeqCst) {
            return Err(AppError::new(ERR_DAEMON_STOPPED.to_string()));
        }
        // Fast path: an already-live resident connection (sync std-Mutex read).
        if let Some(client) = self.live_client() {
            return Ok(client);
        }

        // Slow path: serialize cold start so concurrent callers spawn at most one daemon.
        let _start = self.start_lock.lock().await;
        if let Some(client) = self.live_client() {
            return Ok(client);
        }

        // Spawn OUTSIDE the std Mutex (it can't be held across the await) but UNDER
        // `start_lock`, so no other task spawns concurrently.
        let proc = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            ITermDaemonProcess::spawn(python_bin, script),
        )
        .await
        .map_err(|_| AppError::new("iTerm daemon 握手超时".to_string()))??;
        let client = proc.client();
        // Subscribe BEFORE installing so the pump (spawned below) sees every notification.
        let rx = client.subscribe();

        // Install, re-checking the user-stop flag in the SAME std-Mutex critical section
        // (cold-start race close, mirroring CodexManager): a `stop` landing during our
        // spawn/handshake await is observed here → discard the freshly-spawned process rather
        // than resurrect a daemon the user just stopped.
        let mut guard = self.inner.lock().unwrap();
        if self.stopped.load(Ordering::SeqCst) {
            drop(guard);
            proc.kill_and_reap();
            return Err(AppError::new("iTerm daemon 在启动期间被停止".to_string()));
        }
        // One pump per connection: forwards this daemon's screen/session/error notifications.
        // Spawn it and install it together with the process under the SAME std-Mutex critical
        // section — the spawn is synchronous (no `.await` here), so the swap stays atomic: a
        // concurrent `stop`/`shutdown` (which also locks `inner`) either runs fully before this
        // (and is then caught by the `stopped` re-check above) or fully after (aborting THIS
        // pump). The prior connection's pump is aborted (and its process killed) right after
        // the guard is released, so no stale pump can outlive its process and emit a late
        // connection-level error after a stop→restart→attach.
        let pump_task = tauri::async_runtime::spawn(pump(rx, app.clone()));
        let dead = guard.replace(Resident {
            proc,
            pump: pump_task,
        });
        drop(guard);
        if let Some(dead) = dead {
            dead.pump.abort();
            dead.proc.kill_and_reap();
        }
        Ok(client)
    }

    /// A cloned client handle iff the resident connection is currently live. Synchronous (std
    /// `Mutex`); the critical section never `.await`s.
    fn live_client(&self) -> Option<Arc<RpcClient<ChildStdin>>> {
        let guard = self.inner.lock().unwrap();
        let resident = guard.as_ref()?;
        resident.proc.is_connected().then(|| resident.proc.client())
    }

    /// The status message for a currently-live connection (iTerm version, or a ready fallback).
    fn live_message(&self) -> Option<String> {
        let guard = self.inner.lock().unwrap();
        let resident = guard.as_ref()?;
        resident.proc.is_connected().then(|| {
            if resident.proc.info.iterm_version.trim().is_empty() {
                "iTerm daemon 已就绪".to_string()
            } else {
                format!("iTerm {}", resident.proc.info.iterm_version)
            }
        })
    }

    /// Clear the user-stop flag (explicit-start intent). Called by every ACTION command before
    /// acquiring the connection, so a terminal action overrides a prior `stop` — the analogue
    /// of the manual review commands `resume()`ing a user-stopped codex.
    pub(crate) fn resume(&self) {
        self.stopped.store(false, Ordering::SeqCst);
    }

    /// Probe daemon availability for the StatusBar: ensure the resident connection and report
    /// `available` + iTerm version. Honors the user-stop flag — a passive probe never revives a
    /// stopped daemon. Never errors (every failure maps to `available: false` with a Chinese
    /// message), mirroring `CodexManager::status`.
    pub async fn status<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        python_bin: &str,
        script: &str,
    ) -> TerminalDaemonStatus {
        if self.stopped.load(Ordering::SeqCst) {
            return TerminalDaemonStatus {
                available: false,
                desired_running: false,
                message: ERR_DAEMON_STOPPED.to_string(),
            };
        }
        self.status_inner(app, python_bin, script).await
    }

    /// The probe body shared by `status` (passive) and `start` (explicit): ensure the resident
    /// connection and map the outcome to a `desired_running: true` status (success or spawn
    /// failure are both an intent-to-run state — only an explicit `stop` clears the intent).
    async fn status_inner<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        python_bin: &str,
        script: &str,
    ) -> TerminalDaemonStatus {
        match self.ensure_started(app, python_bin, script).await {
            Ok(_) => TerminalDaemonStatus {
                available: true,
                desired_running: true,
                message: self
                    .live_message()
                    .unwrap_or_else(|| "iTerm daemon 已就绪".to_string()),
            },
            Err(e) => TerminalDaemonStatus {
                available: false,
                desired_running: true,
                message: e.message,
            },
        }
    }

    /// Explicitly (re)start the resident daemon: clear the user-stop flag and ensure the
    /// connection. Returns the latest status (`desired_running: true`).
    pub async fn start<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        python_bin: &str,
        script: &str,
    ) -> TerminalDaemonStatus {
        self.resume();
        self.status_inner(app, python_bin, script).await
    }

    /// Explicitly stop the resident daemon: set the user-stop flag (so passive `status` probes
    /// no longer revive it) and kill the child. An action command still forces a restart by
    /// clearing the flag.
    pub fn stop(&self) -> TerminalDaemonStatus {
        self.stopped.store(true, Ordering::SeqCst);
        self.shutdown();
        TerminalDaemonStatus {
            available: false,
            desired_running: false,
            message: ERR_DAEMON_STOPPED.to_string(),
        }
    }

    /// Kill the resident child on app shutdown (called from `lib.rs`'s sync `RunEvent`
    /// handler) AND abort its per-connection pump. `kill_and_reap` sends SIGKILL synchronously
    /// then reaps on a detached task; `kill_on_drop` and OS-on-exit are the backstops. Aborting
    /// the pump stops it from emitting a stale connection-level error post-teardown. No-op if
    /// nothing is running.
    pub fn shutdown(&self) {
        let resident = self.inner.lock().unwrap().take();
        if let Some(resident) = resident {
            // Abort the pump BEFORE reaping so a stop→restart→attach fast path can't let this
            // connection's pump emit a stale connection-level `Error` after the new session
            // attached. Pump + process tear down as a unit (mirrors `RpcClient`'s reader abort).
            resident.pump.abort();
            resident.proc.kill_and_reap();
        }
    }
}

/// Per-connection pump: forward this daemon's notifications to the frontend as
/// [`TerminalEvent`]s through the [`crate::stream::emit`] funnel until the transport tears
/// down. Maps via the pure [`super::iterm::map_notification`] (unit-tested there); ends on a
/// `ConnectionClosed` (synthetic, reader exited but client alive) or a fully-closed broadcast.
async fn pump<R: Runtime>(mut rx: broadcast::Receiver<Arc<ServerNotification>>, app: AppHandle<R>) {
    loop {
        match rx.recv().await {
            Ok(note) => {
                if let Some(ev) = super::iterm::map_notification(&note) {
                    crate::stream::emit(&app, StreamEvent::Terminal(ev));
                }
                // `map_notification` mapped the synthetic close to a connection-level Error
                // (emitted above); end the pump now that the transport is gone.
                if matches!(note.as_ref(), ServerNotification::ConnectionClosed) {
                    break;
                }
            }
            // Fell behind the ring; the dropped notifications are lost frames but the next
            // `screenUpdate` is a FULL snapshot (not a delta), so the view self-heals — keep
            // pumping rather than tearing down.
            Err(broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("iTerm daemon pump 滞后，丢弃 {n} 条通知");
            }
            // Every `Sender` dropped (the whole `RpcClient` was torn down, e.g. shutdown):
            // surface a connection-level error, same terminal outcome as `ConnectionClosed`.
            Err(broadcast::error::RecvError::Closed) => {
                crate::stream::emit(
                    &app,
                    StreamEvent::Terminal(TerminalEvent::Error {
                        session_id: None,
                        message: ERR_DAEMON_CLOSED.to_string(),
                    }),
                );
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // CI-safe: the user-stop short-circuit returns before any spawn, and the not-stopped
    // paths use a bin name that doesn't exist so `spawn` errors immediately (no real python,
    // no handshake-timeout wait). A `mock_app` supplies the `AppHandle` the signatures need;
    // none of these paths reach the pump (which would touch `AppState`), so the bare mock app
    // is enough.

    #[tokio::test]
    async fn status_reports_stopped_without_spawning() {
        let app = tauri::test::mock_app();
        let m = ITermDaemonManager::default();
        m.stop();
        let s = m
            .status(app.handle(), "prmonitor-no-such-python-bin", "/nope.py")
            .await;
        assert!(!s.available);
        assert!(!s.desired_running);
        assert_eq!(s.message, ERR_DAEMON_STOPPED);
    }

    #[tokio::test]
    async fn status_attempts_start_when_not_stopped() {
        let app = tauri::test::mock_app();
        let m = ITermDaemonManager::default();
        let s = m
            .status(app.handle(), "prmonitor-no-such-python-bin", "/nope.py")
            .await;
        assert!(!s.available);
        assert!(s.desired_running);
    }

    #[tokio::test]
    async fn ensure_started_refuses_when_stopped() {
        let app = tauri::test::mock_app();
        let m = ITermDaemonManager::default();
        m.stop();
        let r = m
            .ensure_started(app.handle(), "prmonitor-no-such-python-bin", "/nope.py")
            .await;
        assert!(r.is_err(), "stopped → ensure_started refuses");
    }

    #[tokio::test]
    async fn start_clears_stopped_then_attempts() {
        let app = tauri::test::mock_app();
        let m = ITermDaemonManager::default();
        m.stop();
        let s = m
            .start(app.handle(), "prmonitor-no-such-python-bin", "/nope.py")
            .await;
        // `start` resumed, so the spawn was attempted (desired) but the bogus bin fails fast.
        assert!(s.desired_running);
        assert!(!s.available);
    }

    #[test]
    fn shutdown_and_stop_without_resident_do_not_panic() {
        // No `Resident` installed → `take()` yields None, so the new pump-abort path aborts
        // nothing and `stop`→`shutdown` is a clean no-op (covers the `None` arm added with the
        // pump handle). Sync: builds the manager without a runtime.
        let m = ITermDaemonManager::default();
        m.shutdown();
        let s = m.stop();
        assert!(!s.available);
        assert!(!s.desired_running);
    }

    #[tokio::test]
    async fn resume_clears_stopped_so_ensure_started_attempts() {
        let app = tauri::test::mock_app();
        let m = ITermDaemonManager::default();
        m.stop();
        m.resume();
        // After resume, ensure_started no longer short-circuits on the stop flag — it proceeds
        // to the (fast-failing bogus) spawn instead of returning the "已停止" refusal.
        let r = m
            .ensure_started(app.handle(), "prmonitor-no-such-python-bin", "/nope.py")
            .await;
        // `Ok` is an `Arc<RpcClient>` (not `Debug`), so match rather than `unwrap_err`.
        let err = match r {
            Ok(_) => panic!("bogus python bin must fail to spawn"),
            Err(e) => e,
        };
        assert_ne!(
            err.message, ERR_DAEMON_STOPPED,
            "resume must clear the stop refusal (error is now a spawn failure)"
        );
    }
}
