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
use crate::config::service::ResolvedCli;
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
    /// - `connection`（每个 review start 的统一收口）直接拒绝（返回 Err），不复活——这是
    ///   outbox executor「停止后不复活」的**race-free 真值源**（PR #47 F1）；
    /// - `ensure_started` 在装载进程前于 inner 锁内复查此位，停止则丢弃刚 spawn 的进程，
    ///   闭合 cold-start 竞态（PR #47 F1）；
    /// - 中断（`stop_review`）走 `existing_client()`，只对在跑的进程有意义、不复活（PR #47 F2）。
    ///
    /// 只有**手动** review 路径清此位（`resume`）强制启动：`start_review` / `start_codex`
    /// 命令在取连接前显式 `resume()`——「用户显式停止 → 自动派发尊重之，手动 review 仍强制启动」。
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
    pub(super) async fn ensure_started(
        &self,
        codex: &ResolvedCli,
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
        let proc = tokio::time::timeout(HANDSHAKE_TIMEOUT, CodexProcess::spawn(codex, repo_root))
            .await
            .map_err(|_| AppError::new("codex app-server 握手超时".to_string()))??;
        let info = proc.info.clone();

        // Install the new connection, re-checking the user-stop flag in the SAME std
        // Mutex critical section (PR #47 F1 cold-start race close). A `stop` can land
        // during our spawn/handshake await: `start_lock` serializes cold starts but
        // `stop` deliberately does NOT take it (it must stay sync). `stop` stores
        // `stopped = true` BEFORE taking this lock (see `stop`/`shutdown`), so:
        //   - if its store happened-before this read, we observe it here and DISCARD
        //     the freshly-spawned process (kill it, return) rather than resurrecting a
        //     server the user just stopped;
        //   - if `stop` instead lands just after we install, its own `take()` reaps our
        //     process.
        // Either ordering ⇒ no orphan child, no stale `stopped`+running combination.
        // We only reach here when the cell was empty or held a dead process and
        // `start_lock` keeps this path single-writer — so reap any dead predecessor
        // explicitly rather than leaning on drop alone.
        let mut guard = self.inner.lock().unwrap();
        if self.stopped.load(Ordering::SeqCst) {
            drop(guard);
            proc.kill_and_reap();
            return Err(AppError::new(
                "codex app-server 在启动期间被停止".to_string(),
            ));
        }
        let dead = guard.replace(proc);
        drop(guard);
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
        codex: &ResolvedCli,
        repo_root: &str,
    ) -> AppResult<Arc<RpcClient<ChildStdin>>> {
        // Authoritative stop funnel (PR #47 F1, race-free close). This is the single
        // acquisition path every review start (manual `start_review` AND auto-dispatch)
        // funnels through (`session::start_review`). A user-stopped server must never be
        // revived here, so REFUSE when stopped rather than clear-the-flag-and-spawn.
        //
        // The manual entries (`start_review` / `start_codex` commands) call `resume()`
        // BEFORE reaching here, so a manual review still force-starts (overrides a prior
        // `stop`). Auto-dispatch never calls `resume()`, so even if a `stop` lands after
        // an automatic outbox execution has been enqueued, this refusal blocks the revive.
        // `ensure_started` adds the symmetric cold-start guard (re-check under the install lock). Interrupts
        // use `existing_client()` (no spawn, no flag change; PR #47 F2).
        if self.stopped.load(Ordering::SeqCst) {
            return Err(AppError::new("codex app-server 已停止".to_string()));
        }
        self.ensure_started(codex, repo_root).await?;
        let guard = self.inner.lock().unwrap();
        let proc = guard
            .as_ref()
            .ok_or_else(|| AppError::new("codex app-server 连接不可用".to_string()))?;
        Ok(proc.client())
    }

    /// Whether the user has explicitly stopped codex (`stop`/`stop_codex`). Production auto-dispatch
    /// reads this as a producer-side skip gate, while every review start still enforces it inside
    /// [`Self::connection`] as the race-free authority.
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Clear the user-stop flag (manual force-start intent). Called by the MANUAL
    /// entries (`start_review` / `start_codex` commands) BEFORE acquiring the
    /// connection, so a manual review/start overrides a prior `stop`. Auto-dispatch
    /// never calls this — that is what makes `connection`'s refuse-if-stopped keep a
    /// stopped server from being auto-revived (PR #47 F1 race close).
    pub(crate) fn resume(&self) {
        self.stopped.store(false, Ordering::SeqCst);
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

    /// Return an authoritative resident lifecycle snapshot without resolving or spawning a CLI.
    /// `None` means the manager is neither user-stopped nor currently live, so a caller may proceed
    /// with cold-start configuration resolution. This keeps stale configuration from masking the
    /// stopped/live axes while preserving lazy startup for a genuinely cold manager.
    pub(crate) fn resident_status(&self) -> Option<CodexStatus> {
        if self.stopped.load(Ordering::SeqCst) {
            return Some(CodexStatus {
                available: false,
                desired_running: false,
                message: "codex app-server 已停止".to_string(),
            });
        }
        self.live_info().map(|info| CodexStatus {
            available: true,
            desired_running: true,
            message: if info.user_agent.is_empty() {
                "codex app-server 已就绪".to_string()
            } else {
                info.user_agent
            },
        })
    }

    /// Fingerprint of the currently live resident. A new configured path does not kill it; probe
    /// compares this snapshot with the newly resolved credential to report `pendingRestart`.
    pub fn active_fingerprint(&self) -> Option<String> {
        let guard = self.inner.lock().unwrap();
        let process = guard.as_ref()?;
        process
            .is_connected()
            .then(|| process.fingerprint().to_string())
    }

    /// Probe codex availability for the StatusBar: ensure the resident connection
    /// and report `available` + version (`userAgent`). Honors the user-stop flag —
    /// when `stop` was called this short-circuits to a stopped status WITHOUT
    /// calling `ensure_started`, so a passive probe never revives a stopped server.
    /// Never errors — every failure maps to `available: false` with a Chinese
    /// message (mirrors the pr slice's `gh_auth_status`).
    pub async fn status(&self, codex: &ResolvedCli, repo_root: &str) -> CodexStatus {
        if let Some(status) = self.resident_status() {
            return status;
        }
        self.status_inner(codex, repo_root).await
    }

    /// The probe body shared by `status` (passive) and `start` (explicit): ensure
    /// the resident connection and map the outcome to a `desired_running: true`
    /// status (success or spawn failure are both an intent-to-run state — only an
    /// explicit `stop` clears the intent). Calls `ensure_started`, so it CAN spawn.
    async fn status_inner(&self, codex: &ResolvedCli, repo_root: &str) -> CodexStatus {
        match self.ensure_started(codex, repo_root).await {
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
    pub async fn start(&self, codex: &ResolvedCli, repo_root: &str) -> CodexStatus {
        self.resume();
        self.status_inner(codex, repo_root).await
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
    use std::{fs, path::Path};

    use super::*;

    fn missing_codex() -> ResolvedCli {
        ResolvedCli::for_test(std::env::temp_dir().join("prmonitor-no-such-codex-bin"))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn status_starts_custom_codex_resolved_from_config() {
        use std::os::unix::fs::PermissionsExt;

        use crate::{
            config::service::{resolve_cli_from, CliResolver, CliToolsConfig},
            model::CliTool,
        };

        let root = std::env::temp_dir().join(format!(
            "prmonitor-codex-status-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("codex");
        fs::write(
            &path,
            br#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{"userAgent":"fake-codex"}}'
      ;;
  esac
done
"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let tools: CliToolsConfig = serde_json::from_value(serde_json::json!({
            "ghPath": "",
            "azPath": "",
            "codexPath": path,
            "claudePath": "",
            "cloudflaredPath": ""
        }))
        .unwrap();
        let codex =
            resolve_cli_from(&CliResolver::default(), &tools, CliTool::Codex, false).unwrap();
        let manager = CodexManager::default();

        let status = manager.status(&codex, root.to_str().unwrap()).await;
        assert!(status.available, "{}", status.message);
        assert_eq!(status.message, "fake-codex");
        let resident = manager.resident_status().expect("live resident snapshot");
        assert!(resident.available);
        assert!(resident.desired_running);
        assert_eq!(resident.message, "fake-codex");
        assert_eq!(
            manager.active_fingerprint().as_deref(),
            Some(codex.fingerprint())
        );

        manager.shutdown();
        let _ = fs::remove_dir_all(root);
    }

    // CI-safe: the user-stop short-circuit returns before any spawn, and the
    // not-stopped paths use a bin name that doesn't exist so `spawn` errors
    // immediately (no real binary, no handshake-timeout wait).

    #[tokio::test]
    async fn status_reports_stopped_without_spawning() {
        let m = CodexManager::default();
        m.stop();
        let resident = m.resident_status().expect("stopped resident snapshot");
        assert!(!resident.available);
        assert!(!resident.desired_running);
        assert_eq!(resident.message, "codex app-server 已停止");
        let s = m.status(&missing_codex(), "").await;
        assert!(!s.available);
        assert!(!s.desired_running);
        assert_eq!(s.message, "codex app-server 已停止");
    }

    #[tokio::test]
    async fn status_attempts_start_when_not_stopped() {
        let m = CodexManager::default();
        let s = m.status(&missing_codex(), "").await;
        assert!(!s.available);
        assert!(s.desired_running);
    }

    #[tokio::test]
    async fn start_clears_stopped_then_attempts() {
        let m = CodexManager::default();
        m.stop();
        let s = m.start(&missing_codex(), "").await;
        assert!(s.desired_running);
        assert!(!s.available);
    }

    #[tokio::test]
    async fn connection_refuses_when_stopped_and_does_not_clear() {
        // Race-free F1 close (reproduction test for the TOCTOU): every review start —
        // including auto-dispatch — funnels through `connection`. When stopped it must
        // REFUSE (Err) and leave `stopped` set, NOT clear-the-flag-and-spawn. (The old
        // behavior cleared `stopped` here, which let auto-dispatch revive a stopped
        // server if a stop landed after the upstream `is_stopped()` gate.)
        let m = CodexManager::default();
        m.stop();
        assert!(m.is_stopped());
        let r = m.connection(&missing_codex(), "").await;
        assert!(r.is_err(), "stopped → connection refuses");
        assert!(
            m.is_stopped(),
            "connection must NOT clear the user-stop flag (auto-dispatch revive guard)"
        );
    }

    #[tokio::test]
    async fn resume_clears_stopped() {
        // The manual entries (`start_review` / `start_codex` commands) call `resume()`
        // before acquiring the connection, so a manual review/start overrides a prior
        // stop and `connection` no longer refuses.
        let m = CodexManager::default();
        m.stop();
        assert!(m.is_stopped());
        m.resume();
        assert!(!m.is_stopped());
    }

    // NOTE: the cold-start install guard in `ensure_started` (re-check `stopped` under
    // the inner lock, discard the freshly-spawned process if a stop landed during the
    // spawn/handshake await) is RUNTIME_ONLY — reaching the post-handshake install path
    // requires a real `codex app-server` binary, so it can't be exercised CI-safe with
    // a bogus bin name (which fails at spawn, before install). It is defended by
    // construction (atomic re-check inside the std-Mutex critical section) and is the
    // companion of the `#[ignore]`d real-binary test in `tests/codex_handshake.rs`.

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

    #[cfg(unix)]
    #[tokio::test]
    async fn changed_fingerprint_reuses_live_process_until_stop() {
        use std::os::unix::fs::PermissionsExt;

        fn fake_codex(path: &Path, user_agent: &str) {
            let script = format!(
                "#!/bin/sh\nwhile IFS= read -r line; do\ncase \"$line\" in\n*\\\"method\\\":\\\"initialize\\\"*) printf '%s\\n' '{{\"id\":1,\"result\":{{\"userAgent\":\"{user_agent}\",\"codexHome\":\"/tmp\",\"platformFamily\":\"unix\",\"platformOs\":\"test\"}}}}' ;;\nesac\ndone\n"
            );
            fs::write(path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let root = std::env::temp_dir().join(format!(
            "prmonitor-codex-manager-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first_dir = root.join("first");
        let second_dir = root.join("second");
        fs::create_dir_all(&first_dir).unwrap();
        fs::create_dir_all(&second_dir).unwrap();
        let first_path = first_dir.join("codex");
        let second_path = second_dir.join("codex");
        fake_codex(&first_path, "first");
        fake_codex(&second_path, "second");
        let first = ResolvedCli::for_test(first_path);
        let second = ResolvedCli::for_test(second_path);
        let manager = CodexManager::default();

        assert_eq!(
            manager.ensure_started(&first, "").await.unwrap().user_agent,
            "first"
        );
        assert_eq!(
            manager.active_fingerprint().as_deref(),
            Some(first.fingerprint())
        );
        assert_eq!(
            manager
                .ensure_started(&second, "")
                .await
                .unwrap()
                .user_agent,
            "first",
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
