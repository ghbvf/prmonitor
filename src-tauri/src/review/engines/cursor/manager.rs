//! Resident Cursor ACP connection manager — a long-lived handle held in
//! `AppState` (mirrors [`super::super::codex::CodexManager`]). A single `agent acp`
//! process is handshaken once and kept alive so reviews start fast; many
//! `session/new`s reuse the one connection.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::process::ChildStdin;

use super::process::{CursorProcess, CursorStatus};
use super::protocol::InitializeResult;
use super::rpc::RpcClient;
use crate::config::service::ResolvedCli;
use crate::error::{AppError, AppResult};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Owns the resident Cursor ACP connection. `&self` methods + interior mutability
/// so it can live in `AppState` (which stays `Default`).
#[derive(Default)]
pub struct CursorManager {
    inner: Arc<Mutex<Option<CursorProcess>>>,
    start_lock: tokio::sync::Mutex<()>,
    /// 用户显式停止标记。true 时 `connection` 拒绝、被动 `status` 不复活。
    stopped: AtomicBool,
}

impl CursorManager {
    pub(super) async fn ensure_started(
        &self,
        agent: &ResolvedCli,
        repo_root: &str,
    ) -> AppResult<InitializeResult> {
        if let Some(info) = self.live_info() {
            return Ok(info);
        }

        let _start = self.start_lock.lock().await;

        if let Some(info) = self.live_info() {
            return Ok(info);
        }

        let proc = tokio::time::timeout(HANDSHAKE_TIMEOUT, CursorProcess::spawn(agent, repo_root))
            .await
            .map_err(|_| AppError::new("cursor ACP 握手超时".to_string()))??;
        let info = proc.info.clone();

        let mut guard = self.inner.lock().unwrap();
        if self.stopped.load(Ordering::SeqCst) {
            drop(guard);
            proc.kill_and_reap();
            return Err(AppError::new("cursor ACP 在启动期间被停止".to_string()));
        }
        let dead = guard.replace(proc);
        drop(guard);
        if let Some(dead) = dead {
            dead.kill_and_reap();
        }
        Ok(info)
    }

    /// Ensure the resident connection is live and return a cloned RPC client.
    /// Refuses when user-stopped (does not clear the flag).
    pub async fn connection(
        &self,
        agent: &ResolvedCli,
        repo_root: &str,
    ) -> AppResult<Arc<RpcClient<ChildStdin>>> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(AppError::new("cursor ACP 已停止".to_string()));
        }
        self.ensure_started(agent, repo_root).await?;
        let guard = self.inner.lock().unwrap();
        let proc = guard
            .as_ref()
            .ok_or_else(|| AppError::new("cursor ACP 连接不可用".to_string()))?;
        Ok(proc.client())
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    pub(crate) fn resume(&self) {
        self.stopped.store(false, Ordering::SeqCst);
    }

    /// Live client only — no spawn, no `stopped` clear (interrupt / cancel path).
    pub(crate) fn existing_client(&self) -> Option<Arc<RpcClient<ChildStdin>>> {
        let guard = self.inner.lock().unwrap();
        let proc = guard.as_ref()?;
        proc.is_connected().then(|| proc.client())
    }

    fn live_info(&self) -> Option<InitializeResult> {
        let guard = self.inner.lock().unwrap();
        let proc = guard.as_ref()?;
        proc.is_connected().then(|| proc.info.clone())
    }

    pub(crate) fn resident_status(&self) -> Option<CursorStatus> {
        if self.stopped.load(Ordering::SeqCst) {
            return Some(CursorStatus {
                available: false,
                desired_running: false,
                message: "cursor ACP 已停止".to_string(),
            });
        }
        self.live_info().map(|info| CursorStatus {
            available: true,
            desired_running: true,
            message: status_message(&info),
        })
    }

    pub fn active_fingerprint(&self) -> Option<String> {
        let guard = self.inner.lock().unwrap();
        let process = guard.as_ref()?;
        process
            .is_connected()
            .then(|| process.fingerprint().to_string())
    }

    pub async fn status(&self, agent: &ResolvedCli, repo_root: &str) -> CursorStatus {
        if let Some(status) = self.resident_status() {
            return status;
        }
        self.status_inner(agent, repo_root).await
    }

    async fn status_inner(&self, agent: &ResolvedCli, repo_root: &str) -> CursorStatus {
        match self.ensure_started(agent, repo_root).await {
            Ok(info) => CursorStatus {
                available: true,
                desired_running: true,
                message: status_message(&info),
            },
            Err(e) => CursorStatus {
                available: false,
                desired_running: true,
                message: e.message,
            },
        }
    }

    pub async fn start(&self, agent: &ResolvedCli, repo_root: &str) -> CursorStatus {
        self.resume();
        self.status_inner(agent, repo_root).await
    }

    pub fn stop(&self) -> CursorStatus {
        self.stopped.store(true, Ordering::SeqCst);
        self.shutdown();
        CursorStatus {
            available: false,
            desired_running: false,
            message: "cursor ACP 已停止".to_string(),
        }
    }

    pub fn shutdown(&self) {
        let proc = self.inner.lock().unwrap().take();
        if let Some(proc) = proc {
            proc.kill_and_reap();
        }
    }
}

fn status_message(info: &InitializeResult) -> String {
    if info.protocol_version == 0 {
        "cursor ACP 已就绪".to_string()
    } else {
        format!("cursor ACP v{}", info.protocol_version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missing_agent() -> ResolvedCli {
        ResolvedCli::for_test(std::env::temp_dir().join("prmonitor-no-such-agent-bin"))
    }

    #[tokio::test]
    async fn status_reports_stopped_without_spawning() {
        let m = CursorManager::default();
        m.stop();
        let resident = m.resident_status().expect("stopped resident snapshot");
        assert!(!resident.available);
        assert!(!resident.desired_running);
        assert_eq!(resident.message, "cursor ACP 已停止");
        let s = m.status(&missing_agent(), "").await;
        assert!(!s.available);
        assert!(!s.desired_running);
    }

    #[tokio::test]
    async fn status_attempts_start_when_not_stopped() {
        let m = CursorManager::default();
        let s = m.status(&missing_agent(), "").await;
        assert!(!s.available);
        assert!(s.desired_running);
    }

    #[tokio::test]
    async fn connection_refuses_when_stopped_and_does_not_clear() {
        let m = CursorManager::default();
        m.stop();
        assert!(m.is_stopped());
        let r = m.connection(&missing_agent(), "").await;
        assert!(r.is_err(), "stopped → connection refuses");
        assert!(m.is_stopped());
    }

    #[tokio::test]
    async fn resume_clears_stopped() {
        let m = CursorManager::default();
        m.stop();
        assert!(m.is_stopped());
        m.resume();
        assert!(!m.is_stopped());
    }

    #[tokio::test]
    async fn existing_client_is_none_when_never_started() {
        let m = CursorManager::default();
        assert!(m.existing_client().is_none());
    }

    #[tokio::test]
    async fn start_clears_stopped_then_attempts() {
        let m = CursorManager::default();
        m.stop();
        let s = m.start(&missing_agent(), "").await;
        assert!(s.desired_running);
        assert!(!s.available);
    }
}
