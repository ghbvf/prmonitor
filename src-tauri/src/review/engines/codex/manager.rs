//! Resident codex app-server connection manager — a long-lived handle held in
//! `AppState` (mirrors `pr::scheduler::Scheduler`). A single `codex app-server`
//! process is handshaken once and kept alive so reviews start fast (no respawn);
//! many `thread/start`s reuse the one connection. The child is killed when the
//! app closes ([`Self::shutdown`], wired to `RunEvent` in `lib.rs`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::process::ChildStdin;

use super::process::{CodexProcess, CodexStatus};
use super::protocol::InitializeResult;
use super::rpc::RpcClient;
use crate::error::{AppError, AppResult};

/// Whole-handshake budget for the first `ensure_started` (spawn + initialize),
/// so the StatusBar probe can't hang even if the binary stalls.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Owns the resident codex connection. `&self` methods + interior mutability so
/// it can live in `AppState` (which stays `Default`).
#[derive(Default)]
pub struct CodexManager {
    inner: Arc<Mutex<Option<CodexProcess>>>,
    /// Serializes the cold-start slow path so concurrent `ensure_started` calls
    /// spawn at most one app-server. The std `Mutex` above can't be held across
    /// the spawn/handshake await; this async lock fills that gap while leaving the
    /// fast path and `shutdown` synchronous on the std `Mutex`.
    start_lock: tokio::sync::Mutex<()>,
    /// 用户显式停止标记（来自 `stop`/`stop_codex`）。true 时：
    /// - 被动 `status` 探测短路为「已停止」，不自动拉起；
    /// - auto-dispatch 在上游 `run_auto_dispatch` 经 `is_stopped()` gate 静默跳过本轮（PR #47 F1），不复活；
    /// - 中断（`stop_review`）走 `existing_client()`，只对在跑的进程有意义、不复活（PR #47 F2）。
    ///
    /// 只有**手动** review 路径会清此位强制启动：`connection`（手动 `start_review` 调用）与
    /// `start`（手动 `start_codex`）——「用户显式停止 → 自动派发尊重之，手动 review 仍强制启动」。
    /// 默认 false = 保持现状 lazy 行为。
    stopped: AtomicBool,
}

impl CodexManager {
    /// Ensure the resident connection is live, returning its handshake info.
    /// Reuses an existing connected process; otherwise spawns + handshakes once
    /// and caches it. Self-heals: if the prior process died (reader cleared its
    /// liveness flag), this respawns.
    ///
    /// Stays `pub` (not `pub(crate)`): the `#[ignore]`d integration test
    /// `tests/codex_handshake.rs::real_app_server_handshake_thread_and_reuse` calls
    /// `CodexManager::ensure_started` directly, and `tests/` is a separate crate that
    /// can only reach `pub` items (`#[ignore]` skips at run time, not compile time).
    /// Narrowing it would break that test's compile (PR #47 F8 — visibility tightening
    /// blocked by the live external-crate use).
    pub async fn ensure_started(
        &self,
        codex_bin: &str,
        repo_root: &str,
    ) -> AppResult<InitializeResult> {
        // Fast path: an already-live resident connection (sync std-Mutex read).
        if let Some(info) = self.live_info() {
            return Ok(info);
        }

        // Slow path: serialize cold start so concurrent callers spawn at most one
        // app-server. The first caller spawns while holding `start_lock`; the
        // others block here, then the re-check below sees the live connection.
        let _start = self.start_lock.lock().await;

        // Re-check under the start lock: a prior holder may have just established
        // the connection while we waited.
        if let Some(info) = self.live_info() {
            return Ok(info);
        }

        // Spawn OUTSIDE the std Mutex (it can't be held across the await) but
        // UNDER `start_lock`, so no other task spawns concurrently.
        let proc =
            tokio::time::timeout(HANDSHAKE_TIMEOUT, CodexProcess::spawn(codex_bin, repo_root))
                .await
                .map_err(|_| AppError::new("codex app-server 握手超时".to_string()))??;
        let info = proc.info.clone();

        // Install the new connection. We only reach here when the cell was empty
        // or held a dead process (the re-check returned early otherwise) and
        // `start_lock` keeps this path single-writer — so reap any dead
        // predecessor explicitly rather than leaning on drop alone.
        let dead = self.inner.lock().unwrap().replace(proc);
        if let Some(dead) = dead {
            dead.kill_and_reap();
        }
        Ok(info)
    }

    /// Ensure the resident connection is live and return a cloned, callable handle
    /// to its JSON-RPC client. The clone happens under the std `Mutex` (sync, no
    /// await), so the session layer can issue `turn/start` etc. and subscribe to
    /// notifications without holding the lock across an `.await`. Self-heals via
    /// [`Self::ensure_started`]. The returned handle stays usable even if the
    /// process later dies — its requests then fail fast and the next call respawns.
    pub async fn connection(
        &self,
        codex_bin: &str,
        repo_root: &str,
    ) -> AppResult<Arc<RpcClient<ChildStdin>>> {
        // This is the MANUAL-review force-start path (manual `start_review`). A user
        // who asks to review overrides a prior `stop`, so clear the user-stop flag
        // to force a start. Auto-dispatch does NOT come here when stopped — it is
        // gated upstream by `is_stopped()` in `run_auto_dispatch` (PR #47 F1), so it
        // never clears the flag / revives a stopped server. Interrupts use
        // `existing_client()` (no spawn, no flag-clear; PR #47 F2). `ensure_started`
        // itself is unchanged (still lazy / self-heal).
        self.stopped.store(false, Ordering::SeqCst);
        self.ensure_started(codex_bin, repo_root).await?;
        let guard = self.inner.lock().unwrap();
        let proc = guard
            .as_ref()
            .ok_or_else(|| AppError::new("codex app-server 连接不可用".to_string()))?;
        Ok(proc.client())
    }

    /// Whether the user has explicitly stopped codex (`stop`/`stop_codex`). Read by
    /// the composition root's `run_auto_dispatch` (PR #47 F1) to gate auto-dispatch:
    /// a stopped codex is NOT auto-revived by a dispatchable PR — only manual review
    /// (`connection`) and manual `start` force a restart.
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// 返回当前**已存活**连接的可调用 client；不存在/已死则 None。
    /// 不 spawn、不清 `stopped`——用于中断等「只对在跑的进程有意义」的操作，
    /// 避免为一个已死会话复活 app-server（见 PR #47 F2）。
    pub(crate) fn existing_client(&self) -> Option<Arc<RpcClient<ChildStdin>>> {
        let guard = self.inner.lock().unwrap();
        let proc = guard.as_ref()?;
        proc.is_connected().then(|| proc.client())
    }

    /// Handshake info iff the resident connection is currently live. Synchronous
    /// (std `Mutex`); the critical section never `.await`s.
    fn live_info(&self) -> Option<InitializeResult> {
        let guard = self.inner.lock().unwrap();
        let proc = guard.as_ref()?;
        proc.is_connected().then(|| proc.info.clone())
    }

    /// Probe codex availability for the StatusBar: ensure the resident connection
    /// and report `available` + version (`userAgent`). Honors the user-stop flag —
    /// when `stop` was called this short-circuits to a stopped status WITHOUT
    /// calling `ensure_started`, so a passive probe never revives a stopped server.
    /// Never errors — every failure maps to `available: false` with a Chinese
    /// message (mirrors the pr slice's `gh_auth_status`).
    pub async fn status(&self, codex_bin: &str, repo_root: &str) -> CodexStatus {
        if self.stopped.load(Ordering::SeqCst) {
            return CodexStatus {
                available: false,
                desired_running: false,
                message: "codex app-server 已停止".to_string(),
            };
        }
        self.status_inner(codex_bin, repo_root).await
    }

    /// The probe body shared by `status` (passive) and `start` (explicit): ensure
    /// the resident connection and map the outcome to a `desired_running: true`
    /// status (success or spawn failure are both an intent-to-run state — only an
    /// explicit `stop` clears the intent). Calls `ensure_started`, so it CAN spawn.
    async fn status_inner(&self, codex_bin: &str, repo_root: &str) -> CodexStatus {
        match self.ensure_started(codex_bin, repo_root).await {
            Ok(info) => {
                let message = if info.user_agent.is_empty() {
                    "codex app-server 已就绪".to_string()
                } else {
                    info.user_agent
                };
                CodexStatus {
                    available: true,
                    desired_running: true,
                    message,
                }
            }
            Err(e) => CodexStatus {
                available: false,
                desired_running: true,
                message: e.message,
            },
        }
    }

    /// Explicitly (re)start the resident server: clear the user-stop flag and
    /// ensure the connection. Returns the latest status (`desired_running: true`).
    pub async fn start(&self, codex_bin: &str, repo_root: &str) -> CodexStatus {
        self.stopped.store(false, Ordering::SeqCst);
        self.status_inner(codex_bin, repo_root).await
    }

    /// Explicitly stop the resident server: set the user-stop flag (so passive
    /// `status` probes no longer revive it) and kill the child. An explicit review
    /// (`connection`) still forces a restart by clearing the flag.
    pub fn stop(&self) -> CodexStatus {
        self.stopped.store(true, Ordering::SeqCst);
        self.shutdown();
        CodexStatus {
            available: false,
            desired_running: false,
            message: "codex app-server 已停止".to_string(),
        }
    }

    /// Kill the resident child on app shutdown (called from `lib.rs`'s sync
    /// `RunEvent` handler). `kill_and_reap` sends SIGKILL synchronously (safe from
    /// any context, no `block_on`) then reaps on a detached task; `kill_on_drop`
    /// and OS-on-exit are the backstops. No-op if nothing is running.
    pub fn shutdown(&self) {
        let proc = self.inner.lock().unwrap().take();
        if let Some(proc) = proc {
            proc.kill_and_reap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // CI-safe: the user-stop short-circuit returns before any spawn, and the
    // not-stopped paths use a bin name that doesn't exist so `spawn` errors
    // immediately (no real binary, no handshake-timeout wait).

    #[tokio::test]
    async fn status_reports_stopped_without_spawning() {
        let m = CodexManager::default();
        m.stop();
        let s = m.status("prmonitor-no-such-codex-bin", "").await;
        assert!(!s.available);
        assert!(!s.desired_running);
        assert_eq!(s.message, "codex app-server 已停止");
    }

    #[tokio::test]
    async fn status_attempts_start_when_not_stopped() {
        let m = CodexManager::default();
        let s = m.status("prmonitor-no-such-codex-bin", "").await;
        assert!(!s.available);
        assert!(s.desired_running);
    }

    #[tokio::test]
    async fn start_clears_stopped_then_attempts() {
        let m = CodexManager::default();
        m.stop();
        let s = m.start("prmonitor-no-such-codex-bin", "").await;
        assert!(s.desired_running);
        assert!(!s.available);
    }

    #[tokio::test]
    async fn connection_clears_stopped_then_starts() {
        // The MANUAL-review path force-starts: even after a `stop`, `connection`
        // clears `stopped` before attempting the spawn. The spawn itself fails (no
        // real binary) and returns Err, but the flag must already be cleared — that
        // is the "manual review overrides a prior stop" semantics (PR #47 F1/F8).
        let m = CodexManager::default();
        m.stop();
        assert!(m.is_stopped());
        let r = m.connection("prmonitor-no-such-codex-bin", "").await;
        assert!(r.is_err(), "no real binary → spawn fails");
        assert!(
            !m.is_stopped(),
            "connection clears the user-stop flag (force-start) before spawning"
        );
    }

    #[tokio::test]
    async fn existing_client_is_none_when_never_started() {
        // No process was ever spawned, so there is no live connection to interrupt.
        let m = CodexManager::default();
        assert!(m.existing_client().is_none());
    }

    #[tokio::test]
    async fn existing_client_does_not_clear_stopped() {
        // The interrupt path must NOT revive a stopped server: `existing_client`
        // only returns a live connection and never clears `stopped`. After `stop`
        // there is no process AND the flag stays set (PR #47 F2).
        let m = CodexManager::default();
        m.stop();
        assert!(m.existing_client().is_none());
        assert!(m.is_stopped());
    }
}
