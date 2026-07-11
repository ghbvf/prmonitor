//! Spawn and manage a `codex app-server --stdio` child process (cwd =
//! `repo_root`), owning its stdin/stdout pipes (wired into an [`RpcClient`]) and
//! draining stderr. Provides the `initialize` → `initialized` handshake plus the
//! per-connection RPC ops the session layer drives over `client()`
//! ([`start_thread`] / [`start_turn`] / [`interrupt_turn`]). The process is kept
//! *resident* by [`super::manager::CodexManager`]; this module is just the
//! per-connection lifecycle.

use std::process::Stdio;
use std::sync::Arc;

use serde::Serialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin};

use super::protocol::{
    rpc_methods, ClientInfo, InitializeParams, InitializeResult, ThreadStartParams,
    ThreadStartResult, TurnInterruptParams, TurnStartParams, TurnStartResult,
};
use super::rpc::RpcClient;
use crate::config::service::ResolvedCli;
use crate::error::{AppError, AppResult};

/// Broadcast ring capacity for streamed notifications (generous for delta streams).
const NOTIF_CAPACITY: usize = 1024;

/// codex availability reported to the StatusBar. Slice-private wire type, mirrored
/// in `src/review/types.ts` (NOT `model.rs` / `src/types.ts`) — same placement as
/// the pr slice's `GhStatus`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexStatus {
    pub available: bool,
    /// 用户意图的运行状态：true=意图运行（available 反映实际连通），false=用户已显式停止。前端据此显示「启动」/「停止」按钮与 idle 态。
    pub desired_running: bool,
    /// `userAgent` on success, or a human-readable (Chinese) failure message.
    pub message: String,
}

/// A live, handshaken connection to a `codex app-server --stdio` child.
pub struct CodexProcess {
    child: Child, // kill_on_drop(true) — killed on drop; manager also kills explicitly.
    /// `Arc` so the session layer can clone a callable handle out from under the
    /// manager's `std::Mutex` without holding the lock across an `.await` (the
    /// lock guard isn't `Send` across awaits; cloning an `Arc` is sync + cheap).
    client: Arc<RpcClient<ChildStdin>>,
    /// `initialize` result captured at handshake (carries `userAgent`).
    pub info: InitializeResult,
    fingerprint: String,
}

impl CodexProcess {
    /// Spawn `codex app-server --stdio` in `repo_root` and complete the
    /// `initialize` + `initialized` handshake. Does NOT start a thread. An empty
    /// `repo_root` leaves the child's cwd at the process default (the handshake is
    /// cwd-independent), so an unconfigured app can still probe availability.
    pub(super) async fn spawn(codex: &ResolvedCli, repo_root: &str) -> AppResult<Self> {
        let mut cmd = codex.command();
        cmd.args(["app-server", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if !repo_root.trim().is_empty() {
            cmd.current_dir(repo_root);
        }

        let mut child = cmd.spawn().map_err(|e| {
            AppError::new(format!(
                "无法启动 codex app-server（未安装或不在 PATH？）: {e}"
            ))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::new("codex app-server stdin 不可用".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::new("codex app-server stdout 不可用".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::new("codex app-server stderr 不可用".to_string()))?;

        // Drain stderr so a full pipe can't block the child.
        spawn_stderr_drain(stderr);

        // `connect` spawns the reader task before we send `initialize`.
        let client = RpcClient::connect(stdin, BufReader::new(stdout), NOTIF_CAPACITY);
        let info = Self::handshake(&client).await?;
        Ok(Self {
            child,
            client: Arc::new(client),
            info,
            fingerprint: codex.fingerprint().to_string(),
        })
    }

    /// `initialize` request → response, then the `initialized` notification. The
    /// notification MUST precede any `thread/start` (the server rejects
    /// pre-initialized requests with `-32016`).
    ///
    /// Generic over the write half so CI-safe tests drive this *production*
    /// orchestration over `tokio::io::duplex()` (no real binary) — a dropped
    /// `initialized` or a reshaped `initialize` regresses in CI, not only in the
    /// `#[ignore]`d real-binary test. `spawn` calls it with `W = ChildStdin`.
    pub async fn handshake<W>(client: &RpcClient<W>) -> AppResult<InitializeResult>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let params = InitializeParams {
            client_info: ClientInfo {
                name: "prmonitor".to_string(),
                title: Some("PR Monitor".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        };
        let raw = client
            .request(
                rpc_methods::INITIALIZE,
                serde_json::to_value(params)
                    .map_err(|e| AppError::new(format!("编码 initialize 失败: {e}")))?,
            )
            .await?;
        let info: InitializeResult = serde_json::from_value(raw)
            .map_err(|e| AppError::new(format!("解析 InitializeResult 失败: {e}")))?;

        client
            .notify(rpc_methods::INITIALIZED, serde_json::Value::Null)
            .await?;
        Ok(info)
    }

    /// A cloned, callable handle to the JSON-RPC client. The session layer issues
    /// `thread/start` / `turn/start` / `turn/interrupt` and subscribes to
    /// notifications through this (see the [`start_thread`] / [`start_turn`] /
    /// [`interrupt_turn`] free helpers), without holding the manager's lock.
    pub fn client(&self) -> Arc<RpcClient<ChildStdin>> {
        self.client.clone()
    }

    /// Whether the underlying connection is still live (reader task running).
    pub fn is_connected(&self) -> bool {
        self.client.is_connected()
    }

    pub(super) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Kill the child and reap it. Sends SIGKILL synchronously (immediate, safe
    /// from any context including the sync `RunEvent` shutdown handler with no
    /// `block_on`), then reaps the child on a detached task so it can't linger as
    /// a zombie during a long-running session — the self-heal path drops a dead
    /// process, and `start_kill` alone never `wait`s (Tokio's docs note the child
    /// stays a zombie until waited on or the orphan reaper runs). `kill_on_drop`
    /// and the OS reaping on app exit are the backstops if the reaper can't run.
    pub fn kill_and_reap(mut self) {
        let _ = self.child.start_kill(); // immediate SIGKILL — synchronous.
        tauri::async_runtime::spawn(async move {
            // Move `self` in so the child (and its pipes) live until reaped.
            let _ = self.child.wait().await;
        });
    }
}

// ---- per-connection RPC ops (driven by the session layer over `client()`) ----
//
// Generic over the write half (`W`) so they're CI-testable over `tokio::io::duplex`
// against an in-process fake server, exactly like the rest of the transport.

/// Open a thread, returning its id. The handshake's `initialized` must precede it
/// — guaranteed because the only `RpcClient` we hand out is from a handshaken
/// [`CodexProcess`].
pub async fn start_thread<W>(client: &RpcClient<W>, params: ThreadStartParams) -> AppResult<String>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let raw = client
        .request(
            rpc_methods::THREAD_START,
            serde_json::to_value(params)
                .map_err(|e| AppError::new(format!("编码 thread/start 失败: {e}")))?,
        )
        .await?;
    let result: ThreadStartResult = serde_json::from_value(raw)
        .map_err(|e| AppError::new(format!("解析 thread/start 失败: {e}")))?;
    Ok(result.thread.id)
}

/// Start a turn (the pr-review skill invocation), returning its id.
pub async fn start_turn<W>(client: &RpcClient<W>, params: TurnStartParams) -> AppResult<String>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let raw = client
        .request(
            rpc_methods::TURN_START,
            serde_json::to_value(params)
                .map_err(|e| AppError::new(format!("编码 turn/start 失败: {e}")))?,
        )
        .await?;
    let result: TurnStartResult = serde_json::from_value(raw)
        .map_err(|e| AppError::new(format!("解析 turn/start 失败: {e}")))?;
    Ok(result.turn.id)
}

/// Interrupt a running turn. The terminal `turn/completed` (status `interrupted`)
/// arrives as a notification, not in this response.
pub async fn interrupt_turn<W>(client: &RpcClient<W>, params: TurnInterruptParams) -> AppResult<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    client
        .request(
            rpc_methods::TURN_INTERRUPT,
            serde_json::to_value(params)
                .map_err(|e| AppError::new(format!("编码 turn/interrupt 失败: {e}")))?,
        )
        .await?;
    Ok(())
}

/// Max bytes logged per stderr line. Bounds memory against an unterminated flood
/// and caps how much child diagnostic context reaches the app log — codex is a
/// trusted local child, so truncation (not redaction) suffices.
const STDERR_MAX_LINE: usize = 512;

fn spawn_stderr_drain(stderr: ChildStderr) {
    tauri::async_runtime::spawn(drain_stderr(BufReader::new(stderr)));
}

/// Drain and log the child's stderr, capping each line so a never-terminating
/// line can't grow the read buffer without bound. Generic over the reader so it
/// is unit-testable without spawning a real child.
async fn drain_stderr<R: AsyncBufRead + Unpin>(mut reader: R) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        // `take(STDERR_MAX_LINE)` yields EOF once the cap is hit, so one read can
        // never buffer more than the cap.
        let n = match (&mut reader)
            .take(STDERR_MAX_LINE as u64)
            .read_until(b'\n', &mut buf)
            .await
        {
            Ok(0) | Err(_) => break, // EOF or read error: child gone.
            Ok(n) => n,
        };
        let overflowed = !buf.ends_with(b"\n") && n >= STDERR_MAX_LINE;
        eprintln!(
            "[codex app-server stderr] {}{}",
            String::from_utf8_lossy(&buf).trim_end(),
            if overflowed { " …(已截断)" } else { "" }
        );
        if overflowed {
            discard_to_newline(&mut reader).await;
        }
    }
}

/// Drop the rest of an over-long stderr line (bounded reads) so it is logged once,
/// not as a flood of fixed-size chunks.
async fn discard_to_newline<R: AsyncBufRead + Unpin>(reader: &mut R) {
    let mut sink: Vec<u8> = Vec::new();
    loop {
        sink.clear();
        match (&mut *reader)
            .take(STDERR_MAX_LINE as u64)
            .read_until(b'\n', &mut sink)
            .await
        {
            Ok(0) => break,                          // EOF
            Ok(_) if sink.ends_with(b"\n") => break, // consumed through the newline
            Ok(_) => continue,                       // more of the long line
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires the real codex binary; run with: cargo test -- --ignored"]
    async fn real_app_server_handshake_and_manager_reuse() {
        use crate::review::engines::codex::manager::CodexManager;
        use crate::{
            config::service::{resolve_cli_from, CliResolver, CliToolsConfig},
            model::CliTool,
        };

        let cli = resolve_cli_from(
            &CliResolver::default(),
            &CliToolsConfig::default(),
            CliTool::Codex,
            true,
        )
        .expect("resolve real codex from configured discovery");
        let repo_root = env!("CARGO_MANIFEST_DIR");
        let process = CodexProcess::spawn(&cli, repo_root)
            .await
            .expect("spawn + handshake");
        assert!(!process.info.user_agent.is_empty());
        process.kill_and_reap();

        let manager = CodexManager::default();
        let first = manager
            .ensure_started(&cli, repo_root)
            .await
            .expect("first");
        let second = manager
            .ensure_started(&cli, repo_root)
            .await
            .expect("reuse");
        assert_eq!(first.user_agent, second.user_agent);
        manager.shutdown();
    }

    #[test]
    fn codex_status_wire_shape_is_camel_case() {
        let v = serde_json::to_value(CodexStatus {
            available: true,
            desired_running: true,
            message: "codex/0.139.0".to_string(),
        })
        .expect("CodexStatus serializes");
        // Keys present — locks the contract with `src/review/types.ts`.
        assert!(v.get("available").is_some());
        assert!(v.get("desiredRunning").is_some());
        assert!(v.get("desired_running").is_none());
        assert!(v.get("message").is_some());
        assert_eq!(v["available"], true);

        // 停止态：desiredRunning=false 必须如实出现（StatusBar 据此切「启动/停止」按钮）。
        let stopped = serde_json::to_value(CodexStatus {
            available: false,
            desired_running: false,
            message: "codex app-server 已停止".to_string(),
        })
        .expect("CodexStatus serializes");
        assert_eq!(stopped["available"], false);
        assert_eq!(stopped["desiredRunning"], false);
        assert!(stopped.get("desired_running").is_none());
    }

    /// An unterminated stderr line (no newline) must not hang or buffer without
    /// bound: the capped reader terminates at EOF.
    #[tokio::test]
    async fn drain_stderr_terminates_on_unterminated_flood() {
        let blob = vec![b'x'; STDERR_MAX_LINE * 4];
        drain_stderr(tokio::io::BufReader::new(&blob[..])).await;
    }

    /// Normal multi-line stderr drains fully and terminates at EOF.
    #[tokio::test]
    async fn drain_stderr_handles_multiple_lines() {
        let data = b"first line\nsecond line\n".to_vec();
        drain_stderr(tokio::io::BufReader::new(&data[..])).await;
    }
}
