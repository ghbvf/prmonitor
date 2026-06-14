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
        // Fast path: an already-live resident connection.
        {
            let guard = self.inner.lock().unwrap();
            if let Some(proc) = guard.as_ref() {
                if proc.is_connected() {
                    return Ok(proc.info.clone());
                }
            }
        }

        // Slow path: spawn OUTSIDE the lock (a std mutex can't be held across the
        // spawn/handshake await).
        let proc =
            tokio::time::timeout(HANDSHAKE_TIMEOUT, CodexProcess::spawn(codex_bin, repo_root))
                .await
                .map_err(|_| AppError::new("codex app-server 握手超时".to_string()))??;
        let info = proc.info.clone();

        let mut guard = self.inner.lock().unwrap();
        // Double-check: another task may have established a live connection while
        // we were spawning. If so, drop ours (kill_on_drop reaps the redundant
        // child) and reuse theirs.
        if let Some(existing) = guard.as_ref() {
            if existing.is_connected() {
                return Ok(existing.info.clone());
            }
        }
        *guard = Some(proc);
        Ok(info)
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
    /// `RunEvent` handler). Explicit `kill().await` for determinism;
    /// `kill_on_drop(true)` is the backstop. No-op if nothing is running.
    pub fn shutdown(&self) {
        let taken = self.inner.lock().unwrap().take();
        if let Some(mut proc) = taken {
            tauri::async_runtime::block_on(async move {
                proc.kill().await;
            });
        }
    }
}
