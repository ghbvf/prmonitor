//! JSON-RPC plumbing over the iTerm daemon stdio transport: a dedicated reader task
//! demuxes responses (`id` → oneshot) from notifications (broadcast to subscribers), plus
//! a serial writer. A minimal copy of the codex `rpc` (the slice boundary forbids
//! `crate::review::`, so it is an independent copy) — dropped: the server→client request /
//! auto-answer path (the daemon never sends requests) and the outbound `notify` (the
//! client only issues request/response calls).
//!
//! [`RpcClient`] is generic over the write half (`W`) so tests drive it over
//! `tokio::io::duplex()` against an in-process fake daemon (CI-safe, no real Python); the
//! read half is owned by the spawned reader task. `process` wires the real `ChildStdin` /
//! `BufReader<ChildStdout>` into the same client.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, oneshot};

use super::codec::{self, Inbound, ResponsePayload};
use super::protocol::{ServerNotification, ERR_DAEMON_CLOSED};
use crate::error::{AppError, AppResult};

/// Per-request budget. A response that never arrives (daemon stall) fails the awaiting
/// `request` here rather than hanging forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Max bytes per inbound NDJSON frame. The reader caps each line at this via `take`, so a
/// never-terminating line from the (external) child can't grow the read buffer without
/// bound; a frame that fills the cap with no closing newline is a protocol violation that
/// tears the connection down. 16 MiB is generous for a screen snapshot.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// JSON-RPC 2.0 error object (`jsonrpc` omitted on the wire; shape is `{code, message, data?}`).
#[derive(Debug, Clone, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "iTerm daemon 错误 {}: {}", self.code, self.message)
    }
}

/// `id` → response-sender registry plus a `closed` flag, shared between `request` and the
/// reader task under one `std::sync::Mutex` (every critical section completes without an
/// `.await`). The shared lock makes the "register a request vs. the reader draining on
/// disconnect" race impossible: the reader sets `closed` while draining, and `request`
/// refuses to register once `closed`, so a request can never be left dangling.
#[derive(Default)]
struct Pending {
    map: HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>,
    /// Set once the reader task exits (EOF / error). Drives [`RpcClient::is_connected`].
    closed: bool,
}

type PendingMap = Arc<Mutex<Pending>>;

/// Shared serial writer. `tokio::sync::Mutex` because its guard is held across the
/// `write_all`/`flush` awaits; it is NEVER held across a response await.
type SharedWriter<W> = Arc<tokio::sync::Mutex<W>>;

/// JSON-RPC client over the daemon transport. Drop aborts the reader task.
pub struct RpcClient<W> {
    writer: SharedWriter<W>,
    pending: PendingMap,
    next_id: AtomicI64,
    /// Notification fan-out; the manager's per-connection pump calls [`Self::subscribe`].
    notifications: broadcast::Sender<Arc<ServerNotification>>,
    reader: tauri::async_runtime::JoinHandle<()>,
}

impl<W: AsyncWrite + Unpin + Send + 'static> RpcClient<W> {
    /// Wire a write half + read half into a live client, spawning the reader task BEFORE
    /// returning — so by the time a caller can issue the first `request` the reader is
    /// already looping (closing the "response arrives before the reader starts" race).
    /// `notif_capacity` bounds the broadcast ring; a slow subscriber that falls behind gets
    /// `RecvError::Lagged` rather than blocking the reader.
    pub fn connect<R>(write_half: W, read_half: R, notif_capacity: usize) -> Self
    where
        R: AsyncBufRead + Unpin + Send + 'static,
    {
        Self::connect_with_max_frame(write_half, read_half, notif_capacity, MAX_FRAME_BYTES)
    }

    /// As [`Self::connect`] but with an explicit inbound-frame cap (tests inject a small cap
    /// to exercise the oversized-frame teardown without writing 16 MiB).
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

    /// Subscribe to streamed daemon notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<ServerNotification>> {
        self.notifications.subscribe()
    }

    /// Whether the connection is still live (the reader task has not exited).
    pub fn is_connected(&self) -> bool {
        !self.pending.lock().unwrap().closed
    }

    /// Send a request and await the matching response, bounded by [`REQUEST_TIMEOUT`].
    pub async fn request(&self, method: &str, params: Value) -> AppResult<Value> {
        self.request_with_timeout(method, params, REQUEST_TIMEOUT)
            .await
    }

    /// As [`Self::request`] but with an explicit per-request budget. The budget covers the
    /// WHOLE request path — both the writer-mutex'd `write_all`/`flush` AND the response
    /// await — in a SINGLE [`tokio::time::timeout`], so a stalled write (writer-lock
    /// contention / a blocked pipe) is bounded too, not just a never-arriving response.
    /// Tests inject a tiny budget to exercise the timeout path without sleeping out the real
    /// 30s [`REQUEST_TIMEOUT`].
    async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        budget: Duration,
    ) -> AppResult<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();

        // Register BEFORE writing so a fast response can't race ahead of the insert. Refuse
        // to register if the reader has already closed the connection — the shared lock with
        // the reader's drain makes this race-free.
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.closed {
                return Err(AppError::new(ERR_DAEMON_CLOSED.to_string()));
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

        // One budget for write + response await. The writer guard is released inside
        // `write_line` (never held across the `rx` await); no lock is held across the await.
        let outcome = tokio::time::timeout(budget, async {
            self.write_line(&line).await?;
            match rx.await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(rpc_err)) => Err(AppError::new(rpc_err.to_string())),
                // Sender dropped without sending => reader fell over (EOF / child died).
                Err(_) => Err(AppError::new(
                    "iTerm daemon 连接已关闭（请求未完成）".to_string(),
                )),
            }
        })
        .await;

        match outcome {
            Ok(result) => {
                // On any error (write failure / rpc error / closed) drop the pending entry.
                // The reader already removes it when it routes a response, so this is an
                // idempotent no-op in the rpc-error / closed arms and a real cleanup for a
                // write failure (whose entry the reader never touched).
                if result.is_err() {
                    self.pending.lock().unwrap().map.remove(&id);
                }
                result
            }
            // Timeout spanning write + await: remove the pending entry so a late response
            // can't leak (route to a dead `id`), then surface an actionable timeout error.
            Err(_) => {
                self.pending.lock().unwrap().map.remove(&id);
                Err(AppError::new(format!("iTerm daemon 请求超时: {method}")))
            }
        }
    }

    /// Acquire the writer lock and write one line; the guard spans write+flush and is
    /// released at scope end — never across the response await in [`Self::request`].
    async fn write_line(&self, line: &str) -> AppResult<()> {
        let mut w = self.writer.lock().await;
        w.write_all(line.as_bytes())
            .await
            .map_err(|e| AppError::new(format!("写入 iTerm daemon 失败: {e}")))?;
        w.flush()
            .await
            .map_err(|e| AppError::new(format!("刷新 iTerm daemon 失败: {e}")))?;
        Ok(())
    }
}

impl<W> Drop for RpcClient<W> {
    fn drop(&mut self) {
        // The reader task must not outlive the client. Aborting it drops the pending map's
        // `oneshot::Sender`s, so any in-flight `request().await` resolves with `RecvError`
        // (the "连接已关闭" arm) rather than hanging.
        self.reader.abort();
    }
}

/// Reader task body. Owns the read half exclusively; loops reading NDJSON lines and routing
/// responses to their oneshot, notifications to the broadcast. One malformed line is logged
/// and skipped (never tears down the connection). On EOF / IO error / oversized frame it
/// marks the connection closed, drain-fails every pending request, and broadcasts a
/// synthetic [`ServerNotification::ConnectionClosed`] so no pump hangs.
async fn reader_loop<R>(
    read_half: R,
    pending: PendingMap,
    notif_tx: broadcast::Sender<Arc<ServerNotification>>,
    max_frame: usize,
) where
    R: AsyncBufRead + Unpin + Send + 'static,
{
    let mut reader = read_half;
    let mut buf: Vec<u8> = Vec::new();

    loop {
        buf.clear();
        // Cap each frame: `take(max_frame)` yields EOF once the cap is hit, so a
        // never-terminating line can't grow `buf` without bound. A frame that fills the cap
        // with no closing newline is a protocol violation → tear the connection down.
        let read = (&mut reader)
            .take(max_frame as u64)
            .read_until(b'\n', &mut buf)
            .await;
        match read {
            Ok(0) => break, // EOF: child closed stdout.
            Ok(_) => {
                if !buf.ends_with(b"\n") && buf.len() >= max_frame {
                    eprintln!("iTerm daemon 帧超过 {max_frame} 字节上限，断开连接");
                    break;
                }
            }
            Err(e) => {
                eprintln!("iTerm daemon 读取错误: {e}");
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
                    eprintln!("iTerm daemon 响应 id={id} 无匹配请求（丢弃）");
                }
            }
            Ok(Some(Inbound::Notification { method, params })) => {
                let note = ServerNotification::from_raw(method, params);
                // `send` errs only with zero receivers — fine when nothing is subscribed.
                let _ = notif_tx.send(Arc::new(note));
            }
            Err(e) => {
                eprintln!("iTerm daemon 帧解析失败（跳过）: {e}");
            }
        }
    }

    // Mark closed + wake every awaiting request, under the same lock, so a request either
    // registered before this drain (and is failed here) or sees `closed` and never registers
    // — no request can be stranded waiting out its timeout.
    {
        let mut pending = pending.lock().unwrap();
        pending.closed = true;
        for (_id, tx) in pending.map.drain() {
            let _ = tx.send(Err(RpcError {
                code: -1,
                message: ERR_DAEMON_CLOSED.to_string(),
                data: None,
            }));
        }
    }

    // Wake the subscribed pump too: the `RpcClient` still holds a broadcast `Sender`, so a
    // dead reader never lets `recv()` return `RecvError::Closed`. Broadcasting a synthetic
    // `ConnectionClosed` lets the pump emit a terminal error instead of hanging. `send` errs
    // only with zero subscribers — harmless.
    let _ = notif_tx.send(Arc::new(ServerNotification::ConnectionClosed));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    /// Request/response demux over an in-process fake daemon (CI-safe, no real Python): the
    /// fake echoes the request id with a typed result, and the client routes it to the
    /// awaiting `request`.
    #[tokio::test]
    async fn request_response_demux_over_duplex() {
        use tokio::io::AsyncBufReadExt;

        let (client_w, server_r) = tokio::io::duplex(8192);
        let (server_w, client_r) = tokio::io::duplex(8192);

        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(server_r).lines();
            let mut out = server_w;
            while let Ok(Some(line)) = reader.next_line().await {
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v["method"] == "listSessions" {
                    let id = v["id"].as_i64().unwrap();
                    let resp = format!(r#"{{"id":{id},"result":[]}}"#);
                    out.write_all(resp.as_bytes()).await.unwrap();
                    out.write_all(b"\n").await.unwrap();
                }
            }
        });

        let client = RpcClient::connect(client_w, BufReader::new(client_r), 16);
        let raw = client
            .request("listSessions", serde_json::json!({}))
            .await
            .expect("listSessions resolves");
        assert!(raw.is_array());

        drop(client);
        let _ = server.await;
    }

    /// A notification (not keyed by an id) is broadcast to subscribers, not routed as a
    /// response.
    #[tokio::test]
    async fn notification_is_broadcast_to_subscribers() {
        let (client_w, _server_r) = tokio::io::duplex(8192);
        let (server_w, client_r) = tokio::io::duplex(8192);

        let client = RpcClient::connect(client_w, BufReader::new(client_r), 16);
        let mut sub = client.subscribe();

        let mut sw = server_w;
        sw.write_all(br#"{"method":"screenUpdate","params":{"sessionId":"p0","cols":80,"rows":24,"contents":"$ "}}"#)
            .await
            .unwrap();
        sw.write_all(b"\n").await.unwrap();
        sw.flush().await.unwrap();

        let note = sub
            .recv()
            .await
            .expect("subscriber receives the notification");
        assert!(matches!(note.as_ref(), ServerNotification::ScreenUpdate(_)));
    }

    /// Reader exit (the server end drops → client read half hits EOF) must broadcast a
    /// synthetic `ConnectionClosed` to every subscriber, and flip `is_connected` false.
    #[tokio::test]
    async fn reader_exit_broadcasts_connection_closed() {
        let (client_w, _server_r) = tokio::io::duplex(8192);
        let (server_w, client_r) = tokio::io::duplex(8192);
        let client = RpcClient::connect(client_w, BufReader::new(client_r), 16);

        let mut sub = client.subscribe();
        drop(server_w); // client read half hits EOF → reader exits.

        let note = sub
            .recv()
            .await
            .expect("subscriber must receive the synthetic close, not a RecvError");
        assert!(
            matches!(note.as_ref(), ServerNotification::ConnectionClosed),
            "reader exit must broadcast ConnectionClosed"
        );
        assert!(!client.is_connected(), "is_connected flips false after EOF");
    }

    /// A frame that fills the cap with no closing newline tears the connection down: the
    /// reader stops, marks closed, drains pending, and a concurrent request fails fast (not a
    /// 30s timeout).
    #[tokio::test]
    async fn oversized_frame_tears_down_connection() {
        let (client_w, _server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let max = 1024usize;
        let client = RpcClient::connect_with_max_frame(client_w, BufReader::new(client_r), 16, max);

        let mut sw = server_w;
        sw.write_all(&vec![b'x'; max * 2]).await.unwrap();
        sw.flush().await.unwrap();

        let res = client.request("listSessions", serde_json::json!({})).await;
        assert!(
            res.is_err(),
            "oversized frame must close the reader and fail the request"
        );
        assert!(
            !client.is_connected(),
            "is_connected flips false once the cap is hit"
        );
    }

    /// A request whose daemon reads the line but never responds (and keeps the connection
    /// open) must time out within its budget AND drain its pending entry — so a late response
    /// can't route to a dead id. Uses an injected tiny budget (not the real 30s
    /// `REQUEST_TIMEOUT`) so CI never sleeps.
    #[tokio::test]
    async fn request_times_out_and_drains_pending() {
        use tokio::io::AsyncBufReadExt;

        let (client_w, server_r) = tokio::io::duplex(8192);
        let (server_w, client_r) = tokio::io::duplex(8192);

        // Fake daemon: read the request line, then NEVER respond. Hold both halves open so the
        // client reader does NOT hit EOF — this isolates the timeout path from the disconnect
        // path (otherwise a closed reader would drain pending with the "connection closed"
        // error, not a timeout).
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(server_r).lines();
            let _ = reader.next_line().await; // consume the request, send nothing back.
            let _hold_writer = &server_w;
            // Park until the client drops its write half (EOF here) at end of test.
            let _ = reader.next_line().await;
        });

        let client = RpcClient::connect(client_w, BufReader::new(client_r), 16);
        let res = client
            .request_with_timeout(
                "listSessions",
                serde_json::json!({}),
                Duration::from_millis(50),
            )
            .await;
        let err = res.expect_err("a non-responding daemon must time out");
        assert!(
            err.message.contains("超时"),
            "timeout error should be actionable, got: {}",
            err.message
        );
        // The pending entry for the timed-out request must be gone (no leak).
        assert!(
            client.pending.lock().unwrap().map.is_empty(),
            "pending map must be drained on timeout"
        );
        // Connection is still live (the reader never saw EOF) — this was a timeout, not a close.
        assert!(
            client.is_connected(),
            "timeout must not mark the connection closed"
        );

        drop(client);
        let _ = server.await;
    }

    /// A request must fail promptly (not hang to its 30s timeout) when the daemon
    /// disconnects, and `is_connected()` must flip false — the reader's EOF drain.
    #[tokio::test]
    async fn request_fails_fast_when_daemon_disconnects() {
        use tokio::io::AsyncBufReadExt;

        let (client_w, server_r) = tokio::io::duplex(8192);
        let (server_w, client_r) = tokio::io::duplex(8192);

        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(server_r).lines();
            let _ = reader.next_line().await;
            drop(server_w); // EOF for the client, no response.
        });

        let client = RpcClient::connect(client_w, BufReader::new(client_r), 16);
        assert!(client.is_connected());

        let res = client.request("initialize", serde_json::json!({})).await;
        assert!(res.is_err(), "request should fail on disconnect, not hang");
        assert!(!client.is_connected());

        let _ = server.await;
    }
}
