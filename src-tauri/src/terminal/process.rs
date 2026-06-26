//! Spawn and manage a `python3 <iterm_daemon.py>` child process, owning its
//! stdin/stdout pipes (wired into an [`RpcClient`]) and draining stderr. Provides the
//! `initialize` handshake (which connects to iTerm INSIDE the daemon, so a missing
//! `iterm2` pip / iTerm-not-running / API-unauthorized surfaces as a structured error,
//! never a silent EOF). The process is kept *resident* by
//! [`super::manager::ITermDaemonManager`]; this module is just the per-connection
//! lifecycle (mirrors codex `process.rs`, minus the per-turn RPC ops which live on the
//! backend).

use std::process::Stdio;
use std::sync::Arc;

use serde::Serialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, Command};

use super::protocol::{rpc_methods, InitializeParams, InitializeResult};
use super::rpc::RpcClient;
use crate::error::{AppError, AppResult};

/// Broadcast ring capacity for streamed notifications (generous for screen-update streams).
const NOTIF_CAPACITY: usize = 1024;

/// iTerm daemon availability reported to the StatusBar. Slice-private wire type, mirrored
/// in `src/terminal/types.ts` (NOT `model.rs` / `src/types.ts`) — same placement as the
/// review slice's `CodexStatus`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalDaemonStatus {
    pub available: bool,
    /// 用户意图的运行状态：true=意图运行（available 反映实际连通），false=用户已显式停止。
    /// 前端据此显示「启动」/「停止」按钮与 idle 态。
    pub desired_running: bool,
    /// iTerm version on success, or a human-readable (Chinese) failure message.
    pub message: String,
}

/// A live, handshaken connection to a `python3 iterm_daemon.py` child.
pub struct ITermDaemonProcess {
    child: Child, // kill_on_drop(true) — killed on drop; manager also kills explicitly.
    /// `Arc` so the backend can clone a callable handle out from under the manager's
    /// `std::Mutex` without holding the lock across an `.await`.
    client: Arc<RpcClient<ChildStdin>>,
    /// `initialize` result captured at handshake (carries `itermVersion`).
    pub info: InitializeResult,
}

impl ITermDaemonProcess {
    /// Spawn `python3 <script>` and complete the `initialize` handshake (which connects to
    /// iTerm inside the daemon). No iTerm work happens before `initialize`, so a precondition
    /// failure (missing `iterm2` pip, iTerm not running, API unauthorized) arrives as a
    /// structured JSON-RPC error from the handshake, not a silent EOF.
    pub async fn spawn(python_bin: &str, script: &str) -> AppResult<Self> {
        let mut cmd = Command::new(python_bin);
        cmd.arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            AppError::new(format!(
                "无法启动 iTerm daemon（python3 未安装或不在 PATH？）: {e}"
            ))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::new("iTerm daemon stdin 不可用".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::new("iTerm daemon stdout 不可用".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::new("iTerm daemon stderr 不可用".to_string()))?;

        // Drain stderr so a full pipe can't block the child.
        spawn_stderr_drain(stderr);

        // `connect` spawns the reader task before we send `initialize`.
        let client = RpcClient::connect(stdin, BufReader::new(stdout), NOTIF_CAPACITY);
        let info = Self::handshake(&client).await?;
        Ok(Self {
            child,
            client: Arc::new(client),
            info,
        })
    }

    /// `initialize` request → response. The daemon connects to iTerm in this handler, so a
    /// precondition failure surfaces as the request's `Err`.
    ///
    /// Generic over the write half so CI-safe tests drive this *production* orchestration
    /// over `tokio::io::duplex()` (no real daemon). `spawn` calls it with `W = ChildStdin`.
    pub async fn handshake<W>(client: &RpcClient<W>) -> AppResult<InitializeResult>
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let raw = client
            .request(
                rpc_methods::INITIALIZE,
                serde_json::to_value(InitializeParams {})
                    .map_err(|e| AppError::new(format!("编码 initialize 失败: {e}")))?,
            )
            .await?;
        serde_json::from_value(raw)
            .map_err(|e| AppError::new(format!("解析 InitializeResult 失败: {e}")))
    }

    /// A cloned, callable handle to the JSON-RPC client (the backend issues `listSessions`
    /// etc. and subscribes to notifications through this, without holding the manager's lock).
    pub fn client(&self) -> Arc<RpcClient<ChildStdin>> {
        self.client.clone()
    }

    /// Whether the underlying connection is still live (reader task running).
    pub fn is_connected(&self) -> bool {
        self.client.is_connected()
    }

    /// Kill the child and reap it. Sends SIGKILL synchronously (immediate, safe from any
    /// context including the sync `RunEvent` shutdown handler with no `block_on`), then reaps
    /// the child on a detached task so it can't linger as a zombie. `kill_on_drop` and the OS
    /// reaping on app exit are the backstops.
    pub fn kill_and_reap(mut self) {
        let _ = self.child.start_kill(); // immediate SIGKILL — synchronous.
        tauri::async_runtime::spawn(async move {
            // Move `self` in so the child (and its pipes) live until reaped.
            let _ = self.child.wait().await;
        });
    }
}

/// Max bytes logged per stderr line. Bounds memory against an unterminated flood and caps
/// how much child diagnostic context reaches the app log — the daemon is a trusted local
/// child, so truncation (not redaction) suffices.
const STDERR_MAX_LINE: usize = 512;

fn spawn_stderr_drain(stderr: ChildStderr) {
    tauri::async_runtime::spawn(drain_stderr(BufReader::new(stderr)));
}

/// Drain and log the child's stderr, capping each line so a never-terminating line can't
/// grow the read buffer without bound. Generic over the reader so it is unit-testable
/// without spawning a real child.
async fn drain_stderr<R: AsyncBufRead + Unpin>(mut reader: R) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
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
            "[iTerm daemon stderr] {}{}",
            String::from_utf8_lossy(&buf).trim_end(),
            if overflowed { " …(已截断)" } else { "" }
        );
        if overflowed {
            discard_to_newline(&mut reader).await;
        }
    }
}

/// Drop the rest of an over-long stderr line (bounded reads) so it is logged once, not as a
/// flood of fixed-size chunks.
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

    /// Golden lock for the `TerminalDaemonStatus` wire shape (Medium carrier per
    /// ai-robust.md): mirrors `CodexStatus`'s lock. camelCase keys present, snake_case absent
    /// — locks the contract with `src/terminal/types.ts`.
    #[test]
    fn terminal_daemon_status_wire_shape_is_camel_case() {
        let v = serde_json::to_value(TerminalDaemonStatus {
            available: true,
            desired_running: true,
            message: "iTerm 3.5.0".to_string(),
        })
        .expect("TerminalDaemonStatus serializes");
        assert!(v.get("available").is_some());
        assert!(v.get("desiredRunning").is_some());
        assert!(v.get("desired_running").is_none());
        assert!(v.get("message").is_some());
        assert_eq!(v["available"], true);

        // 停止态：desiredRunning=false 必须如实出现（StatusBar 据此切「启动/停止」按钮）。
        let stopped = serde_json::to_value(TerminalDaemonStatus {
            available: false,
            desired_running: false,
            message: crate::terminal::protocol::ERR_DAEMON_STOPPED.to_string(),
        })
        .expect("TerminalDaemonStatus serializes");
        assert_eq!(stopped["available"], false);
        assert_eq!(stopped["desiredRunning"], false);
        assert!(stopped.get("desired_running").is_none());
    }

    /// An unterminated stderr line (no newline) must not hang or buffer without bound: the
    /// capped reader terminates at EOF.
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
