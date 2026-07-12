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
const STOPPED_MSG: &str = "cursor ACP 已停止";

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
            return Err(AppError::new(STOPPED_MSG.to_string()));
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
                message: STOPPED_MSG.to_string(),
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
            message: STOPPED_MSG.to_string(),
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
        assert_eq!(resident.message, STOPPED_MSG);
        let s = m.status(&missing_agent(), "").await;
        assert!(!s.available);
        assert!(!s.desired_running);
        assert_eq!(s.message, STOPPED_MSG);
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
    async fn existing_client_does_not_clear_stopped() {
        // Interrupt path must NOT revive a stopped server (parity with Codex PR #47 F2).
        let m = CursorManager::default();
        m.stop();
        assert!(m.existing_client().is_none());
        assert!(m.is_stopped());
    }

    #[tokio::test]
    async fn start_clears_stopped_then_attempts() {
        let m = CursorManager::default();
        m.stop();
        let s = m.start(&missing_agent(), "").await;
        assert!(s.desired_running);
        assert!(!s.available);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn changed_fingerprint_reuses_live_process_until_stop() {
        use std::os::unix::fs::PermissionsExt;
        use std::{fs, path::Path};

        fn fake_agent(path: &Path, protocol_version: u32) {
            let script = format!(
                r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":{protocol_version},"authMethods":[]}}}}'
      ;;
    *'"method":"authenticate"'*)
      printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{}}}}'
      ;;
  esac
done
"#
            );
            fs::write(path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let root = std::env::temp_dir().join(format!(
            "prmonitor-cursor-manager-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first_dir = root.join("first");
        let second_dir = root.join("second");
        let repo = root.join("repo");
        fs::create_dir_all(&first_dir).unwrap();
        fs::create_dir_all(&second_dir).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let first_path = first_dir.join("agent");
        let second_path = second_dir.join("agent");
        fake_agent(&first_path, 1);
        fake_agent(&second_path, 2);
        let first = ResolvedCli::for_test(first_path);
        let second = ResolvedCli::for_test(second_path);
        let manager = CursorManager::default();
        let repo_root = repo.to_str().unwrap();

        assert_eq!(
            manager
                .ensure_started(&first, repo_root)
                .await
                .unwrap()
                .protocol_version,
            1
        );
        assert_eq!(
            manager.active_fingerprint().as_deref(),
            Some(first.fingerprint())
        );
        assert_eq!(
            manager
                .ensure_started(&second, repo_root)
                .await
                .unwrap()
                .protocol_version,
            1,
            "a config change must not kill or replace the live resident"
        );
        assert_eq!(
            manager.active_fingerprint().as_deref(),
            Some(first.fingerprint())
        );
        manager.shutdown();
        let _ = fs::remove_dir_all(root);
    }
}
