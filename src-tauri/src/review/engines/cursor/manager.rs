//! Resident Cursor ACP connection manager — a long-lived handle held in
//! `AppState` (mirrors [`super::super::codex::CodexManager`]). A single `agent acp`
//! process is handshaken once and kept alive so reviews start fast; many
//! `session/new`s reuse the one connection.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
/// Shared unavailable message (StatusBar / resume / connection).
pub(crate) const CURSOR_ACP_UNAVAILABLE: &str = "cursor ACP 连接不可用";
/// Model switch refused while a Cursor review is in flight (AB#1753 F5).
pub(crate) const CURSOR_ACP_MODEL_BUSY_MSG: &str =
    "cursor ACP 模型与常驻进程不一致，请先停止进行中的 review 或停止 ACP 后再切换模型";

/// Owns the resident Cursor ACP connection. `&self` methods + interior mutability
/// so it can live in `AppState` (which stays `Default`).
#[derive(Default)]
pub struct CursorManager {
    inner: Arc<Mutex<Option<CursorProcess>>>,
    start_lock: tokio::sync::Mutex<()>,
    /// 用户显式停止标记。true 时 `connection` 拒绝、被动 `status` 不复活。
    stopped: AtomicBool,
    /// Monotonic generation: bumped once per successfully installed live process
    /// (first start → 1). Sessions bind the value at `session/new` for resume gating.
    generation: AtomicU64,
}

impl CursorManager {
    /// Ensure resident process is live. `busy` = in-flight Cursor review — refuse
    /// model-mismatch respawn instead of killing the turn (AB#1753 F5).
    pub(super) async fn ensure_started(
        &self,
        agent: &ResolvedCli,
        repo_root: &str,
        model: &str,
        busy: bool,
    ) -> AppResult<InitializeResult> {
        let desired = model.trim();
        // Blank desired = no model preference — reuse any live process.
        if desired.is_empty() {
            if let Some(info) = self.live_info() {
                return Ok(info);
            }
        } else if let Some(info) = self.live_info_if_model(desired) {
            return Ok(info);
        }

        let _start = self.start_lock.lock().await;
        self.ensure_started_inner(agent, repo_root, model, busy)
            .await
    }

    /// Caller MUST hold `start_lock`. No nested lock — used by [`Self::ensure_started`]
    /// and [`Self::connection`] (AB#1753 F3).
    async fn ensure_started_inner(
        &self,
        agent: &ResolvedCli,
        repo_root: &str,
        model: &str,
        busy: bool,
    ) -> AppResult<InitializeResult> {
        let desired = model.trim();
        if desired.is_empty() {
            if let Some(info) = self.live_info() {
                return Ok(info);
            }
        } else if let Some(info) = self.live_info_if_model(desired) {
            return Ok(info);
        } else if let Some(live_model) = self.live_spawn_model() {
            if live_model != desired {
                if busy {
                    return Err(AppError::new(CURSOR_ACP_MODEL_BUSY_MSG.to_string()));
                }
                self.shutdown();
            }
        }

        if let Some(info) = self.live_info() {
            return Ok(info);
        }

        let proc = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            CursorProcess::spawn(agent, repo_root, model),
        )
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
        // Bump once per newly installed live process (cold → gen=1).
        self.generation.fetch_add(1, Ordering::SeqCst);
        drop(guard);
        if let Some(dead) = dead {
            dead.kill_and_reap();
        }
        Ok(info)
    }

    /// Ensure the resident connection is live and return the RPC client bound to the
    /// same process generation that was just installed / observed (AB#1754 F4).
    /// Holds `start_lock` across ensure + client/gen/model read to close TOCTOU (F3).
    /// Refuses when user-stopped (does not clear the flag).
    pub async fn connection(
        &self,
        agent: &ResolvedCli,
        repo_root: &str,
        model: &str,
        busy: bool,
    ) -> AppResult<(Arc<RpcClient<ChildStdin>>, u64)> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(AppError::new(STOPPED_MSG.to_string()));
        }
        let _start = self.start_lock.lock().await;
        self.ensure_started_inner(agent, repo_root, model, busy)
            .await?;
        let guard = self.inner.lock().unwrap();
        let proc = guard
            .as_ref()
            .ok_or_else(|| AppError::new(CURSOR_ACP_UNAVAILABLE.to_string()))?;
        if !proc.is_connected() {
            return Err(AppError::new(CURSOR_ACP_UNAVAILABLE.to_string()));
        }
        let desired = model.trim();
        if !desired.is_empty() && proc.spawn_model() != desired {
            // Should be unreachable after ensure_started_inner; fail closed on TOCTOU.
            return Err(AppError::new(CURSOR_ACP_UNAVAILABLE.to_string()));
        }
        let client = proc.client();
        let gen = self.generation.load(Ordering::SeqCst);
        Ok((client, gen))
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    pub(crate) fn resume(&self) {
        self.stopped.store(false, Ordering::SeqCst)
    }

    /// Live client only — no spawn, no `stopped` clear (interrupt / cancel / resume path).
    pub(crate) fn existing_client(&self) -> Option<Arc<RpcClient<ChildStdin>>> {
        let guard = self.inner.lock().unwrap();
        let proc = guard.as_ref()?;
        proc.is_connected().then(|| proc.client())
    }

    /// Whether a connected ACP process is currently installed.
    pub(crate) fn has_live(&self) -> bool {
        self.live_info().is_some()
    }

    /// Spawn-time `--model` of the live connected process, if any.
    pub(crate) fn active_spawn_model(&self) -> Option<String> {
        self.live_spawn_model()
    }

    /// Generation of the live connected process, if any. `None` when cold / stopped /
    /// shutdown (even if the atomic still holds the last bumped value).
    pub fn active_generation(&self) -> Option<u64> {
        self.live_info()?;
        Some(self.generation.load(Ordering::SeqCst))
    }

    fn live_info(&self) -> Option<InitializeResult> {
        let guard = self.inner.lock().unwrap();
        let proc = guard.as_ref()?;
        proc.is_connected().then(|| proc.info.clone())
    }

    fn live_info_if_model(&self, desired: &str) -> Option<InitializeResult> {
        let guard = self.inner.lock().unwrap();
        let proc = guard.as_ref()?;
        if !proc.is_connected() || proc.spawn_model() != desired {
            return None;
        }
        Some(proc.info.clone())
    }

    fn live_spawn_model(&self) -> Option<String> {
        let guard = self.inner.lock().unwrap();
        let proc = guard.as_ref()?;
        proc.is_connected().then(|| proc.spawn_model().to_string())
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

    pub async fn status(
        &self,
        agent: &ResolvedCli,
        repo_root: &str,
        model: &str,
        busy: bool,
    ) -> CursorStatus {
        if let Some(status) = self.resident_status() {
            return status;
        }
        self.status_inner(agent, repo_root, model, busy).await
    }

    async fn status_inner(
        &self,
        agent: &ResolvedCli,
        repo_root: &str,
        model: &str,
        busy: bool,
    ) -> CursorStatus {
        match self.ensure_started(agent, repo_root, model, busy).await {
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

    pub async fn start(
        &self,
        agent: &ResolvedCli,
        repo_root: &str,
        model: &str,
        busy: bool,
    ) -> CursorStatus {
        self.resume();
        self.status_inner(agent, repo_root, model, busy).await
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

    #[test]
    fn has_live_false_when_cold() {
        let m = CursorManager::default();
        assert!(!m.has_live());
        assert!(m.active_spawn_model().is_none());
    }

    #[test]
    fn generation_none_when_cold() {
        let m = CursorManager::default();
        assert_eq!(m.active_generation(), None);
    }

    #[tokio::test]
    async fn status_reports_stopped_without_spawning() {
        let m = CursorManager::default();
        m.stop();
        let resident = m.resident_status().expect("stopped resident snapshot");
        assert!(!resident.available);
        assert!(!resident.desired_running);
        assert_eq!(resident.message, STOPPED_MSG);
        let s = m.status(&missing_agent(), "", "", false).await;
        assert!(!s.available);
        assert!(!s.desired_running);
        assert_eq!(s.message, STOPPED_MSG);
    }

    #[tokio::test]
    async fn status_attempts_start_when_not_stopped() {
        let m = CursorManager::default();
        let s = m.status(&missing_agent(), "", "", false).await;
        assert!(!s.available);
        assert!(s.desired_running);
    }

    #[tokio::test]
    async fn connection_refuses_when_stopped_and_does_not_clear() {
        let m = CursorManager::default();
        m.stop();
        assert!(m.is_stopped());
        let r = m.connection(&missing_agent(), "", "", false).await;
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
        let s = m.start(&missing_agent(), "", "", false).await;
        assert!(s.desired_running);
        assert!(!s.available);
    }

    #[cfg(unix)]
    fn fake_agent(path: &std::path::Path, protocol_version: u32) {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

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

    #[cfg(unix)]
    #[tokio::test]
    async fn generation_bumps_on_each_successful_start() {
        use std::fs;

        let root = std::env::temp_dir().join(format!(
            "prmonitor-cursor-gen-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let agent_dir = root.join("agent-dir");
        let repo = root.join("repo");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let agent_path = agent_dir.join("agent");
        fake_agent(&agent_path, 1);
        let agent = ResolvedCli::for_test(agent_path);
        let manager = CursorManager::default();
        let repo_root = repo.to_str().unwrap();

        assert_eq!(manager.active_generation(), None);
        manager
            .ensure_started(&agent, repo_root, "", false)
            .await
            .expect("first start");
        let gen1 = manager.active_generation().expect("live after first start");
        assert_eq!(gen1, 1);

        manager.shutdown();
        assert_eq!(manager.active_generation(), None);

        manager
            .ensure_started(&agent, repo_root, "", false)
            .await
            .expect("second start");
        let gen2 = manager
            .active_generation()
            .expect("live after second start");
        assert_eq!(gen2, 2);
        assert_ne!(gen2, gen1);

        manager.shutdown();
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn model_mismatch_respawns_and_bumps_generation() {
        use std::fs;

        let root = std::env::temp_dir().join(format!(
            "prmonitor-cursor-model-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let agent_dir = root.join("agent-dir");
        let repo = root.join("repo");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let agent_path = agent_dir.join("agent");
        fake_agent(&agent_path, 1);
        let agent = ResolvedCli::for_test(agent_path);
        let manager = CursorManager::default();
        let repo_root = repo.to_str().unwrap();

        manager
            .ensure_started(&agent, repo_root, "", false)
            .await
            .expect("start blank model");
        let gen1 = manager.active_generation().expect("gen after blank");
        assert_eq!(gen1, 1);
        {
            let guard = manager.inner.lock().unwrap();
            assert_eq!(guard.as_ref().unwrap().spawn_model(), "");
        }

        manager
            .ensure_started(&agent, repo_root, "composer-2-fast", false)
            .await
            .expect("respawn with model");
        let gen2 = manager.active_generation().expect("gen after model change");
        assert!(gen2 > gen1, "model mismatch must bump generation");
        {
            let guard = manager.inner.lock().unwrap();
            assert_eq!(guard.as_ref().unwrap().spawn_model(), "composer-2-fast");
        }

        manager.shutdown();
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn blank_model_reuses_live_non_empty_spawn_model() {
        use std::fs;

        let root = std::env::temp_dir().join(format!(
            "prmonitor-cursor-blank-reuse-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let agent_dir = root.join("agent-dir");
        let repo = root.join("repo");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let agent_path = agent_dir.join("agent");
        fake_agent(&agent_path, 1);
        let agent = ResolvedCli::for_test(agent_path);
        let manager = CursorManager::default();
        let repo_root = repo.to_str().unwrap();

        manager
            .ensure_started(&agent, repo_root, "composer-2-fast", false)
            .await
            .expect("start with model");
        let gen1 = manager.active_generation().expect("gen after model start");
        assert_eq!(
            manager.active_spawn_model().as_deref(),
            Some("composer-2-fast")
        );

        manager
            .ensure_started(&agent, repo_root, "", false)
            .await
            .expect("blank must reuse");
        let gen2 = manager.active_generation().expect("still live");
        assert_eq!(gen2, gen1, "blank desired must not respawn");
        assert_eq!(
            manager.active_spawn_model().as_deref(),
            Some("composer-2-fast"),
            "blank desired keeps live spawn_model"
        );

        manager.shutdown();
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connection_returns_generation_bound_to_client() {
        use std::fs;

        let root = std::env::temp_dir().join(format!(
            "prmonitor-cursor-conn-gen-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let agent_dir = root.join("agent-dir");
        let repo = root.join("repo");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let agent_path = agent_dir.join("agent");
        fake_agent(&agent_path, 1);
        let agent = ResolvedCli::for_test(agent_path);
        let manager = CursorManager::default();
        let repo_root = repo.to_str().unwrap();

        let (_client, gen) = manager
            .connection(&agent, repo_root, "composer-2-fast", false)
            .await
            .expect("connection");
        assert_eq!(gen, 1);
        assert_eq!(manager.active_generation(), Some(1));

        manager.shutdown();
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resume_must_not_silent_respawn_on_model_change() {
        // F1: after a session stamps gen N, a later non-empty model change bumps the
        // resident process. Resume must use existing_client + generation gate — never
        // connection() — so it fail-closes instead of silent-respawn-and-prompt.
        use std::fs;

        let root = std::env::temp_dir().join(format!(
            "prmonitor-cursor-resume-gate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let agent_dir = root.join("agent-dir");
        let repo = root.join("repo");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let agent_path = agent_dir.join("agent");
        fake_agent(&agent_path, 1);
        let agent = ResolvedCli::for_test(agent_path);
        let manager = CursorManager::default();
        let repo_root = repo.to_str().unwrap();

        let (_client, gen1) = manager
            .connection(&agent, repo_root, "composer-2-fast", false)
            .await
            .expect("start");
        assert_eq!(gen1, 1);

        // Simulate StatusBar / another start requesting a different non-empty model.
        manager
            .ensure_started(&agent, repo_root, "composer-2", false)
            .await
            .expect("model change respawns");
        let gen2 = manager.active_generation().expect("new gen");
        assert!(gen2 > gen1);

        // Resume-style checks: generation gate fails; existing_client must not respawn.
        assert_ne!(manager.active_generation(), Some(gen1));
        assert!(
            manager.existing_client().is_some(),
            "live client after respawn"
        );
        assert_eq!(manager.active_spawn_model().as_deref(), Some("composer-2"));
        // Blank ensure_started must not hide the mismatch by tearing down again.
        let gen_before_blank = manager.active_generation();
        manager
            .ensure_started(&agent, repo_root, "", false)
            .await
            .expect("blank reuses");
        assert_eq!(manager.active_generation(), gen_before_blank);

        manager.shutdown();
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn changed_fingerprint_reuses_live_process_until_stop() {
        use std::fs;

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
                .ensure_started(&first, repo_root, "", false)
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
                .ensure_started(&second, repo_root, "", false)
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

    #[cfg(unix)]
    #[tokio::test]
    async fn busy_model_mismatch_refuses_respawn() {
        use std::fs;

        let root = std::env::temp_dir().join(format!(
            "prmonitor-cursor-busy-model-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let agent_dir = root.join("agent-dir");
        let repo = root.join("repo");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&repo).unwrap();
        let agent_path = agent_dir.join("agent");
        fake_agent(&agent_path, 1);
        let agent = ResolvedCli::for_test(agent_path);
        let manager = CursorManager::default();
        let repo_root = repo.to_str().unwrap();

        manager
            .ensure_started(&agent, repo_root, "composer-2-fast", false)
            .await
            .expect("start");
        let gen = manager.active_generation();

        let err = manager
            .ensure_started(&agent, repo_root, "composer-2", true)
            .await
            .expect_err("busy mismatch must refuse");
        assert_eq!(err.message, CURSOR_ACP_MODEL_BUSY_MSG);
        assert_eq!(
            manager.active_generation(),
            gen,
            "must not respawn when busy"
        );
        assert_eq!(
            manager.active_spawn_model().as_deref(),
            Some("composer-2-fast")
        );

        manager.shutdown();
        let _ = fs::remove_dir_all(root);
    }
}
