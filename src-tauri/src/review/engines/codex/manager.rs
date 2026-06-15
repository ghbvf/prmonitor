//! Resident codex app-server connection manager — a long-lived handle held in
//! `AppState` (mirrors `pr::scheduler::Scheduler`). A single `codex app-server`
//! process is handshaken once and kept alive so reviews start fast (no respawn);
//! many `thread/start`s reuse the one connection. The child is killed when the
//! app closes ([`Self::shutdown`], wired to `RunEvent` in `lib.rs`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::process::{CodexProcess, CodexStatus};
use super::protocol::InitializeResult;
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
}

impl CodexManager {
    /// Ensure the resident connection is live, returning its handshake info.
    /// Reuses an existing connected process; otherwise spawns + handshakes once
    /// and caches it. Self-heals: if the prior process died (reader cleared its
    /// liveness flag), this respawns.
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

    /// Handshake info iff the resident connection is currently live. Synchronous
    /// (std `Mutex`); the critical section never `.await`s.
    fn live_info(&self) -> Option<InitializeResult> {
        let guard = self.inner.lock().unwrap();
        let proc = guard.as_ref()?;
        proc.is_connected().then(|| proc.info.clone())
    }

    /// Probe codex availability for the StatusBar: ensure the resident connection
    /// and report `available` + version (`userAgent`). Never errors — every
    /// failure maps to `available: false` with a Chinese message (mirrors the pr
    /// slice's `gh_auth_status`).
    pub async fn status(&self, codex_bin: &str, repo_root: &str) -> CodexStatus {
        match self.ensure_started(codex_bin, repo_root).await {
            Ok(info) => {
                let message = if info.user_agent.is_empty() {
                    "codex app-server 已就绪".to_string()
                } else {
                    info.user_agent
                };
                CodexStatus {
                    available: true,
                    message,
                }
            }
            Err(e) => CodexStatus {
                available: false,
                message: e.message,
            },
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
