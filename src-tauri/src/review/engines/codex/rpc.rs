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
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, oneshot};

use super::codec::{self, Inbound, ResponsePayload};
use super::protocol::ServerNotification;
use crate::error::{AppError, AppResult};

/// Per-request budget. A response that never arrives (server stall) fails the
/// awaiting `request` here rather than hanging forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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

/// `id` → response-sender registry, shared between `request` and the reader task.
/// `std::sync::Mutex` (not tokio's): every critical section completes without an
/// `.await`, so an async mutex would only add overhead and a clippy hazard.
type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// JSON-RPC client over the app-server transport. Drop aborts the reader task.
pub struct RpcClient<W> {
    /// Serial writer. `tokio::sync::Mutex` because its guard is held across the
    /// `write_all`/`flush` awaits; it is NEVER held across a response await.
    writer: tokio::sync::Mutex<W>,
    pending: PendingMap,
    next_id: AtomicI64,
    /// Notification fan-out; subscribers (PR6 session manager) call [`Self::subscribe`].
    notifications: broadcast::Sender<Arc<ServerNotification>>,
    /// Liveness: `true` until the reader task exits (EOF / error). The manager
    /// reads this to decide whether the resident connection needs respawning.
    connected: Arc<AtomicBool>,
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
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (notif_tx, _rx) = broadcast::channel(notif_capacity);
        let connected = Arc::new(AtomicBool::new(true));

        let reader = tauri::async_runtime::spawn(reader_loop(
            read_half,
            pending.clone(),
            notif_tx.clone(),
            connected.clone(),
        ));

        Self {
            writer: tokio::sync::Mutex::new(write_half),
            pending,
            next_id: AtomicI64::new(1),
            notifications: notif_tx,
            connected,
            reader,
        }
    }

    /// Subscribe to streamed server notifications.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<ServerNotification>> {
        self.notifications.subscribe()
    }

    /// Whether the reader task is still running (the connection is live).
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Send a request and await the matching response, bounded by [`REQUEST_TIMEOUT`].
    pub async fn request(&self, method: &str, params: Value) -> AppResult<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();

        // Register BEFORE writing so a fast response can't race ahead of the insert.
        self.pending.lock().unwrap().insert(id, tx);

        let line = match codec::encode_request(id, method, &params) {
            Ok(line) => line,
            Err(e) => {
                self.pending.lock().unwrap().remove(&id);
                return Err(e);
            }
        };
        if let Err(e) = self.write_line(&line).await {
            self.pending.lock().unwrap().remove(&id);
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
                self.pending.lock().unwrap().remove(&id);
                Err(AppError::new(format!("app-server 请求超时: {method}")))
            }
        }
    }

    /// Fire-and-forget notification (no `id`, no response) — e.g. `initialized`.
    pub async fn notify(&self, method: &str, params: Value) -> AppResult<()> {
        let line = codec::encode_notification(method, &params)?;
        self.write_line(&line).await
    }

    /// The ONLY acquirer of the writer lock. The guard spans write+flush and is
    /// released at scope end — never across the response await in [`Self::request`]
    /// (the structural deadlock guard).
    async fn write_line(&self, line: &str) -> AppResult<()> {
        let mut w = self.writer.lock().await;
        w.write_all(line.as_bytes())
            .await
            .map_err(|e| AppError::new(format!("写入 app-server 失败: {e}")))?;
        w.flush()
            .await
            .map_err(|e| AppError::new(format!("刷新 app-server 失败: {e}")))?;
        Ok(())
    }
}

impl<W> Drop for RpcClient<W> {
    fn drop(&mut self) {
        // The reader task must not outlive the client.
        self.reader.abort();
    }
}

/// Reader task body. Owns the read half exclusively; loops reading NDJSON lines
/// and routing responses to their oneshot, notifications to the broadcast. One
/// malformed line is logged and skipped (never tears down the connection). On
/// EOF / IO error it clears `connected` and drain-fails every pending request so
/// no `request().await` hangs out its timeout.
async fn reader_loop<R>(
    read_half: R,
    pending: PendingMap,
    notif_tx: broadcast::Sender<Arc<ServerNotification>>,
    connected: Arc<AtomicBool>,
) where
    R: AsyncBufRead + Unpin + Send + 'static,
{
    let mut reader = read_half;
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF: child closed stdout.
            Ok(_) => {}
            Err(e) => {
                eprintln!("app-server 读取错误: {e}");
                break;
            }
        }

        match codec::decode_line(&line) {
            Ok(None) => {} // blank line.
            Ok(Some(Inbound::Response { id, payload })) => {
                if let Some(tx) = pending.lock().unwrap().remove(&id) {
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
                // `send` errs only with zero receivers — fine to ignore (PR5 has
                // no consumer yet; PR6 subscribes).
                let _ = notif_tx.send(Arc::new(note));
            }
            Err(e) => {
                eprintln!("app-server 帧解析失败（跳过）: {e}");
            }
        }
    }

    connected.store(false, Ordering::Relaxed);
    // Wake every awaiting request immediately instead of stalling to its timeout.
    let mut map = pending.lock().unwrap();
    for (_id, tx) in map.drain() {
        let _ = tx.send(Err(RpcError {
            code: -1,
            message: "app-server 连接已关闭".to_string(),
            data: None,
        }));
    }
}
