//! JSON-RPC plumbing over the app-server stdio transport: a dedicated reader
//! task demuxes responses (`id` → oneshot) from notifications (broadcast to
//! subscribers), plus a serial writer. This is what the prior `router.py`
//! lacked — it discarded notifications; we forward them so the UI can stream.
//!
//! [`RpcClient`] is generic over the write half (`W`) so tests drive it over
//! `tokio::io::duplex()` against an in-process fake server (CI-safe, no real
//! binary); the read half is owned by the spawned reader task. `process` wires
//! the real `ChildStdin` / `BufReader<ChildStdout>` into the same client.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, oneshot};

use super::codec::{self, Inbound, ResponsePayload};
use super::protocol::ServerNotification;
use crate::error::{AppError, AppResult};

/// Per-request budget. A response that never arrives (server stall) fails the
/// awaiting `request` here rather than hanging forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Max bytes per inbound NDJSON frame. The reader caps each line at this via
/// `take`, so a never-terminating line from the (external) child can't grow the
/// read buffer without bound; a frame that fills the cap with no closing newline
/// is a protocol violation that tears the connection down. 16 MiB is generous for
/// review payloads.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// JSON-RPC 2.0 error object (the `jsonrpc` field is omitted on the wire, but the
/// error shape is standard: `{code, message, data?}`).
#[derive(Debug, Clone, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "app-server 错误 {}: {}", self.code, self.message)
    }
}

/// `id` → response-sender registry plus a `closed` flag, shared between `request`
/// and the reader task under one `std::sync::Mutex` (every critical section
/// completes without an `.await`). The shared lock makes the "register a request
/// vs. the reader draining on disconnect" race impossible: the reader sets
/// `closed` while draining, and `request` refuses to register once `closed`, so a
/// request can never be left dangling to wait out its timeout.
#[derive(Default)]
struct Pending {
    map: HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>,
    /// Set once the reader task exits (EOF / error). Drives [`RpcClient::is_connected`].
    closed: bool,
}

type PendingMap = Arc<Mutex<Pending>>;

/// Shared serial writer. `tokio::sync::Mutex` because its guard is held across the
/// `write_all`/`flush` awaits; it is NEVER held across a response await. `Arc`
/// because two tasks write: `RpcClient::write_line` (our requests/notifications)
/// and the reader task (auto-answers to server→client approval requests). The
/// mutex serializes them; neither holds it across a response await, so they can't
/// deadlock.
type SharedWriter<W> = Arc<tokio::sync::Mutex<W>>;

/// JSON-RPC client over the app-server transport. Drop aborts the reader task.
pub struct RpcClient<W> {
    writer: SharedWriter<W>,
    pending: PendingMap,
    next_id: AtomicI64,
    /// Notification fan-out; subscribers (PR6 session manager) call [`Self::subscribe`].
    notifications: broadcast::Sender<Arc<ServerNotification>>,
    reader: tauri::async_runtime::JoinHandle<()>,
}

impl<W: AsyncWrite + Unpin + Send + 'static> RpcClient<W> {
    /// Wire a write half + read half into a live client, spawning the reader task.
    ///
    /// The reader is spawned **before** this returns, so by the time a caller can
    /// issue the first `request` (e.g. `initialize`) the reader is already
    /// looping — closing the "response arrives before the reader starts" race.
    /// `notif_capacity` bounds the broadcast ring; a slow subscriber that falls
    /// behind gets `RecvError::Lagged` rather than blocking the reader.
    pub fn connect<R>(write_half: W, read_half: R, notif_capacity: usize) -> Self
    where
        R: AsyncBufRead + Unpin + Send + 'static,
    {
        Self::connect_with_max_frame(write_half, read_half, notif_capacity, MAX_FRAME_BYTES)
    }

    /// As [`Self::connect`] but with an explicit inbound-frame cap (tests inject a
    /// small cap to exercise the oversized-frame teardown without writing 16 MiB).
    fn connect_with_max_frame<R>(
        write_half: W,
        read_half: R,
        notif_capacity: usize,
        max_frame: usize,
    ) -> Self
    where
        R: AsyncBufRead + Unpin + Send + 'static,
    {
        let pending: PendingMap = Arc::new(Mutex::new(Pending::default()));
        let (notif_tx, _rx) = broadcast::channel(notif_capacity);
        let writer: SharedWriter<W> = Arc::new(tokio::sync::Mutex::new(write_half));

        let reader = tauri::async_runtime::spawn(reader_loop(
            read_half,
            pending.clone(),
            notif_tx.clone(),
            writer.clone(),
            max_frame,
        ));

        Self {
            writer,
            pending,
            next_id: AtomicI64::new(1),
            notifications: notif_tx,
            reader,
        }
    }

    /// Subscribe to streamed server notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<ServerNotification>> {
        self.notifications.subscribe()
    }

    /// Whether the connection is still live (the reader task has not exited).
    pub fn is_connected(&self) -> bool {
        !self.pending.lock().unwrap().closed
    }

    /// Send a request and await the matching response, bounded by [`REQUEST_TIMEOUT`].
    pub async fn request(&self, method: &str, params: Value) -> AppResult<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();

        // Register BEFORE writing so a fast response can't race ahead of the
        // insert. Refuse to register if the reader has already closed the
        // connection — the shared lock with the reader's drain makes this
        // race-free (no request can be left dangling past the drain).
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.closed {
                return Err(AppError::new("app-server 连接已关闭".to_string()));
            }
            pending.map.insert(id, tx);
        }

        let line = match codec::encode_request(id, method, &params) {
            Ok(line) => line,
            Err(e) => {
                self.pending.lock().unwrap().map.remove(&id);
                return Err(e);
            }
        };
        if let Err(e) = self.write_line(&line).await {
            self.pending.lock().unwrap().map.remove(&id);
            return Err(e);
        }

        // Await the response with no locks held.
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(rpc_err))) => Err(AppError::new(rpc_err.to_string())),
            // Sender dropped without sending => reader fell over (EOF / child died).
            Ok(Err(_)) => Err(AppError::new(
                "app-server 连接已关闭（请求未完成）".to_string(),
            )),
            Err(_) => {
                self.pending.lock().unwrap().map.remove(&id);
                Err(AppError::new(format!("app-server 请求超时: {method}")))
            }
        }
    }

    /// Fire-and-forget notification (no `id`, no response) — e.g. `initialized`.
    pub async fn notify(&self, method: &str, params: Value) -> AppResult<()> {
        let line = codec::encode_notification(method, &params)?;
        self.write_line(&line).await
    }

    /// Acquire the writer lock and write one line. The guard spans write+flush and
    /// is released at scope end — never across the response await in
    /// [`Self::request`] (the structural deadlock guard). The reader task is the
    /// only other writer (auto-answers to server requests, via [`write_line`]); the
    /// shared mutex serializes the two and neither holds it across a response await.
    async fn write_line(&self, line: &str) -> AppResult<()> {
        write_line(&self.writer, line).await
    }
}

/// Write one already-encoded line to the shared writer (used by both
/// `RpcClient::write_line` and the reader task's auto-answer path).
async fn write_line<W>(writer: &SharedWriter<W>, line: &str) -> AppResult<()>
where
    W: AsyncWrite + Unpin,
{
    let mut w = writer.lock().await;
    w.write_all(line.as_bytes())
        .await
        .map_err(|e| AppError::new(format!("写入 app-server 失败: {e}")))?;
    w.flush()
        .await
        .map_err(|e| AppError::new(format!("刷新 app-server 失败: {e}")))?;
    Ok(())
}

impl<W> Drop for RpcClient<W> {
    fn drop(&mut self) {
        // The reader task must not outlive the client. Aborting it drops the
        // pending map's `oneshot::Sender`s, so any in-flight `request().await`
        // resolves with `RecvError` (the `Ok(Err(_))` "连接已关闭" arm) rather
        // than hanging.
        self.reader.abort();
    }
}

/// Reader task body. Owns the read half exclusively; loops reading NDJSON lines
/// and routing responses to their oneshot, notifications to the broadcast, and
/// server→client requests to the auto-answer path (so a reverse approval prompt
/// never leaves codex blocked). One malformed line is logged and skipped (never
/// tears down the connection). On EOF / IO error it marks the connection closed
/// and drain-fails every pending request so no `request().await` hangs out its
/// timeout.
async fn reader_loop<R, W>(
    read_half: R,
    pending: PendingMap,
    notif_tx: broadcast::Sender<Arc<ServerNotification>>,
    writer: SharedWriter<W>,
    max_frame: usize,
) where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = read_half;
    let mut buf: Vec<u8> = Vec::new();

    loop {
        buf.clear();
        // Cap each frame: `take(max_frame)` yields EOF once the cap is hit, so a
        // never-terminating line can't grow `buf` without bound. A frame that
        // fills the cap with no closing newline is a protocol violation → tear the
        // connection down (the drain below fails every pending request).
        let read = (&mut reader)
            .take(max_frame as u64)
            .read_until(b'\n', &mut buf)
            .await;
        match read {
            Ok(0) => break, // EOF: child closed stdout.
            Ok(_) => {
                if !buf.ends_with(b"\n") && buf.len() >= max_frame {
                    eprintln!("app-server 帧超过 {max_frame} 字节上限，断开连接");
                    break;
                }
            }
            Err(e) => {
                eprintln!("app-server 读取错误: {e}");
                break;
            }
        }

        let line = String::from_utf8_lossy(&buf);
        match codec::decode_line(&line) {
            Ok(None) => {} // blank line.
            Ok(Some(Inbound::Response { id, payload })) => {
                if let Some(tx) = pending.lock().unwrap().map.remove(&id) {
                    let routed = match payload {
                        ResponsePayload::Ok(v) => Ok(v),
                        ResponsePayload::Err(e) => Err(e),
                    };
                    let _ = tx.send(routed); // Err => requester already gone; harmless.
                } else {
                    eprintln!("app-server 响应 id={id} 无匹配请求（丢弃）");
                }
            }
            Ok(Some(Inbound::Notification { method, params })) => {
                let note = ServerNotification::from_raw(method, params);
                // `send` errs only with zero receivers — fine to ignore when no
                // review session is currently subscribed.
                let _ = notif_tx.send(Arc::new(note));
            }
            Ok(Some(Inbound::ServerRequest { id, method, params })) => {
                auto_answer_server_request(&writer, id, &method, params).await;
            }
            Err(e) => {
                eprintln!("app-server 帧解析失败（跳过）: {e}");
            }
        }
    }

    // Mark closed + wake every awaiting request, under the same lock, so a request
    // either registered before this drain (and is failed here) or sees `closed`
    // and never registers — no request can be stranded waiting out its timeout.
    let mut pending = pending.lock().unwrap();
    pending.closed = true;
    for (_id, tx) in pending.map.drain() {
        let _ = tx.send(Err(RpcError {
            code: -1,
            message: "app-server 连接已关闭".to_string(),
            data: None,
        }));
    }
}

/// JSON-RPC "method not found" — the error code returned for a server→client
/// request we have no auto-answer for, so codex gets a reply rather than hanging.
const METHOD_NOT_FOUND: i64 = -32601;

/// Auto-answer one server→client request. Approval prompts get an approving
/// decision (so an unattended review never stalls); anything else gets a
/// JSON-RPC error (still a reply — codex won't block). The approve token per
/// method lives in [`super::protocol::auto_response`].
async fn auto_answer_server_request<W>(
    writer: &SharedWriter<W>,
    id: i64,
    method: &str,
    _params: Value,
) where
    W: AsyncWrite + Unpin,
{
    let line = match super::protocol::auto_response(method).result() {
        Some(result) => codec::encode_response(id, &result),
        None => {
            eprintln!("app-server 反向请求 {method}（id={id}）无自动应答，回 error");
            codec::encode_error_response(id, METHOD_NOT_FOUND, "method not handled by client")
        }
    };
    match line {
        Ok(line) => {
            if let Err(e) = write_line(writer, &line).await {
                eprintln!("回复 app-server 反向请求 {method}（id={id}）失败: {e}");
            }
        }
        Err(e) => eprintln!("编码 app-server 反向请求应答失败: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame that fills the cap with no closing newline tears the connection
    /// down: the reader stops, marks the connection closed, drains pending, and a
    /// concurrent request fails fast (not a 30s timeout) rather than buffering the
    /// runaway line.
    #[tokio::test]
    async fn oversized_frame_tears_down_connection() {
        let (client_w, _server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let max = 1024usize;
        let client = RpcClient::connect_with_max_frame(
            client_w,
            tokio::io::BufReader::new(client_r),
            16,
            max,
        );

        // Flood > cap with no newline.
        let mut sw = server_w;
        sw.write_all(&vec![b'x'; max * 2]).await.unwrap();
        sw.flush().await.unwrap();

        let res = client.request("initialize", serde_json::json!({})).await;
        assert!(
            res.is_err(),
            "oversized frame must close the reader and fail the request"
        );
        assert!(
            !client.is_connected(),
            "is_connected flips false once the cap is hit"
        );
    }

    /// A server→client approval request (`id` + `method`) must be auto-answered:
    /// the reader replies `{"id": N, "result": {"decision": "approved"}}` so codex
    /// never blocks. Guards the reverse-approval path end to end (decode →
    /// auto_response → encode_response → write).
    #[tokio::test]
    async fn auto_answers_reverse_approval_request() {
        use tokio::io::AsyncBufReadExt;

        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let _client = RpcClient::connect(client_w, tokio::io::BufReader::new(client_r), 16);

        // Server sends an exec approval request.
        let mut sw = server_w;
        sw.write_all(
            b"{\"id\":99,\"method\":\"execCommandApproval\",\"params\":{\"command\":\"ls\"}}\n",
        )
        .await
        .unwrap();
        sw.flush().await.unwrap();

        // Client must auto-reply with the approving response.
        let mut sr = tokio::io::BufReader::new(server_r);
        let mut line = String::new();
        sr.read_line(&mut line).await.unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 99);
        assert_eq!(v["result"]["decision"], "approved");
    }

    /// A v2 `item/*requestApproval` reverse request is auto-answered with the
    /// `accept` decision (distinct from the v1 `approved` token).
    #[tokio::test]
    async fn auto_answers_v2_request_approval_with_accept() {
        use tokio::io::AsyncBufReadExt;

        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let _client = RpcClient::connect(client_w, tokio::io::BufReader::new(client_r), 16);

        let mut sw = server_w;
        sw.write_all(b"{\"id\":7,\"method\":\"item/fileChange/requestApproval\",\"params\":{}}\n")
            .await
            .unwrap();
        sw.flush().await.unwrap();

        let mut sr = tokio::io::BufReader::new(server_r);
        let mut line = String::new();
        sr.read_line(&mut line).await.unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["result"]["decision"], "accept");
    }

    /// An unhandled server→client request is answered with a JSON-RPC error (not
    /// silence), so codex never blocks waiting for a reply.
    #[tokio::test]
    async fn unhandled_server_request_gets_error_reply() {
        use tokio::io::AsyncBufReadExt;

        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let _client = RpcClient::connect(client_w, tokio::io::BufReader::new(client_r), 16);

        let mut sw = server_w;
        sw.write_all(b"{\"id\":8,\"method\":\"mcpServer/elicitation/request\",\"params\":{}}\n")
            .await
            .unwrap();
        sw.flush().await.unwrap();

        let mut sr = tokio::io::BufReader::new(server_r);
        let mut line = String::new();
        sr.read_line(&mut line).await.unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 8);
        assert!(v.get("error").is_some());
        assert!(v.get("result").is_none());
    }
}
