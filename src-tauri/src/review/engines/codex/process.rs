//! Spawn and manage a `codex app-server --stdio` child process (cwd =
//! `repo_root`), owning its stdin/stdout pipes (wired into an [`RpcClient`]) and
//! draining stderr. Provides the `initialize` → `initialized` handshake and a
//! `thread/start` helper. The process is kept *resident* by
//! [`super::manager::CodexManager`]; this module is just the per-connection
//! lifecycle.

use std::process::Stdio;

use serde::Serialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, BufReader};
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
