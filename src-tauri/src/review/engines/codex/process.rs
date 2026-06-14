//! Spawn and manage a `codex app-server --stdio` child process (cwd =
//! `repo_root`), owning its stdin/stdout pipes (wired into an [`RpcClient`]) and
//! draining stderr. Provides the `initialize` → `initialized` handshake and a
//! `thread/start` helper. The process is kept *resident* by
//! [`super::manager::CodexManager`]; this module is just the per-connection
//! lifecycle.

use std::process::Stdio;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, Command};

use super::protocol::{
    rpc_methods, ClientInfo, InitializeParams, InitializeResult, ThreadStartParams,
    ThreadStartResult,
};
use super::rpc::RpcClient;
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
    /// `userAgent` on success, or a human-readable (Chinese) failure message.
    pub message: String,
}

/// A live, handshaken connection to a `codex app-server --stdio` child.
pub struct CodexProcess {
    child: Child, // kill_on_drop(true) — killed on drop; manager also kills explicitly.
    client: RpcClient<ChildStdin>,
    /// `initialize` result captured at handshake (carries `userAgent`).
    pub info: InitializeResult,
}

impl CodexProcess {
    /// Spawn `codex app-server --stdio` in `repo_root` and complete the
    /// `initialize` + `initialized` handshake. Does NOT start a thread. An empty
    /// `repo_root` leaves the child's cwd at the process default (the handshake is
    /// cwd-independent), so an unconfigured app can still probe availability.
    pub async fn spawn(codex_bin: &str, repo_root: &str) -> AppResult<Self> {
        let mut cmd = Command::new(codex_bin);
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
            client,
            info,
        })
    }

    /// `initialize` request → response, then the `initialized` notification. The
    /// notification MUST precede any `thread/start` (the server rejects
    /// pre-initialized requests with `-32016`).
    async fn handshake(client: &RpcClient<ChildStdin>) -> AppResult<InitializeResult> {
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

    /// Open a thread, returning its id. Callable only on an already-handshaken
    /// process, so `initialized` has necessarily been sent.
    pub async fn start_thread(&self, params: ThreadStartParams) -> AppResult<String> {
        let raw = self
            .client
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

    /// Whether the underlying connection is still live (reader task running).
    pub fn is_connected(&self) -> bool {
        self.client.is_connected()
    }

    /// Send SIGKILL to the child (used by the manager on app shutdown).
    /// `start_kill` only signals — it does not await reaping — so it is synchronous
    /// and safe to call from the sync `RunEvent` handler with no `block_on`.
    /// `kill_on_drop(true)` is the backstop.
    pub fn start_kill(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn spawn_stderr_drain(stderr: ChildStderr) {
    tauri::async_runtime::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[codex app-server stderr] {line}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_status_wire_shape_is_camel_case() {
        let v = serde_json::to_value(CodexStatus {
            available: true,
            message: "codex/0.139.0".to_string(),
        })
        .expect("CodexStatus serializes");
        // Keys present — locks the contract with `src/review/types.ts`.
        assert!(v.get("available").is_some());
        assert!(v.get("message").is_some());
        assert_eq!(v["available"], true);
    }
}
