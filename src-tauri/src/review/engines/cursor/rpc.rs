//! JSON-RPC 2.0 plumbing over the Cursor ACP stdio transport: a dedicated reader
//! task demuxes responses (`id` → oneshot) from notifications (broadcast to
//! subscribers), plus a serial writer. Auto-answers reverse requests so an
//! unattended review never stalls on permission / Cursor extension prompts.

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

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cursor ACP 错误 {}: {}", self.code, self.message)
    }
}

#[derive(Default)]
struct Pending {
    map: HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>,
    closed: bool,
}

type PendingMap = Arc<Mutex<Pending>>;
type SharedWriter<W> = Arc<tokio::sync::Mutex<W>>;

/// JSON-RPC client over the Cursor ACP transport. Drop aborts the reader task.
pub struct RpcClient<W> {
    writer: SharedWriter<W>,
    pending: PendingMap,
    next_id: AtomicI64,
    notifications: broadcast::Sender<Arc<ServerNotification>>,
    reader: tauri::async_runtime::JoinHandle<()>,
}

impl<W: AsyncWrite + Unpin + Send + 'static> RpcClient<W> {
    pub fn connect<R>(write_half: W, read_half: R, notif_capacity: usize) -> Self
    where
        R: AsyncBufRead + Unpin + Send + 'static,
    {
        Self::connect_with_max_frame(write_half, read_half, notif_capacity, MAX_FRAME_BYTES)
    }

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

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<ServerNotification>> {
        self.notifications.subscribe()
    }

    pub fn is_connected(&self) -> bool {
        !self.pending.lock().unwrap().closed
    }

    pub async fn request(&self, method: &str, params: Value) -> AppResult<Value> {
        self.request_with_timeout(method, params, REQUEST_TIMEOUT)
            .await
    }

    /// Like [`Self::request`] but with an explicit budget — `session/prompt` can
    /// run for an entire review turn (many minutes), so the engine uses a longer
    /// timeout there while handshake RPCs stay on the default.
    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> AppResult<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.pending.lock().unwrap();
            if pending.closed {
                return Err(AppError::new("cursor ACP 连接已关闭".to_string()));
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

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(rpc_err))) => Err(AppError::new(rpc_err.to_string())),
            Ok(Err(_)) => Err(AppError::new(
                "cursor ACP 连接已关闭（请求未完成）".to_string(),
            )),
            Err(_) => {
                self.pending.lock().unwrap().map.remove(&id);
                Err(AppError::new(format!("cursor ACP 请求超时: {method}")))
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> AppResult<()> {
        let line = codec::encode_notification(method, &params)?;
        self.write_line(&line).await
    }

    async fn write_line(&self, line: &str) -> AppResult<()> {
        write_line(&self.writer, line).await
    }
}

async fn write_line<W>(writer: &SharedWriter<W>, line: &str) -> AppResult<()>
where
    W: AsyncWrite + Unpin,
{
    let mut w = writer.lock().await;
    w.write_all(line.as_bytes())
        .await
        .map_err(|e| AppError::new(format!("写入 cursor ACP 失败: {e}")))?;
    w.flush()
        .await
        .map_err(|e| AppError::new(format!("刷新 cursor ACP 失败: {e}")))?;
    Ok(())
}

impl<W> Drop for RpcClient<W> {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

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
        let read = (&mut reader)
            .take(max_frame as u64)
            .read_until(b'\n', &mut buf)
            .await;
        match read {
            Ok(0) => break,
            Ok(_) => {
                if !buf.ends_with(b"\n") && buf.len() >= max_frame {
                    eprintln!("cursor ACP 帧超过 {max_frame} 字节上限，断开连接");
                    break;
                }
            }
            Err(e) => {
                eprintln!("cursor ACP 读取错误: {e}");
                break;
            }
        }

        let line = String::from_utf8_lossy(&buf);
        match codec::decode_line(&line) {
            Ok(None) => {}
            Ok(Some(Inbound::Response { id, payload })) => {
                if let Some(tx) = pending.lock().unwrap().map.remove(&id) {
                    let routed = match payload {
                        ResponsePayload::Ok(v) => Ok(v),
                        ResponsePayload::Err(e) => Err(e),
                    };
                    let _ = tx.send(routed);
                } else {
                    eprintln!("cursor ACP 响应 id={id} 无匹配请求（丢弃）");
                }
            }
            Ok(Some(Inbound::Notification { method, params })) => {
                let note = ServerNotification::from_raw(method, params);
                let _ = notif_tx.send(Arc::new(note));
            }
            Ok(Some(Inbound::ServerRequest { id, method, params })) => {
                auto_answer_server_request(&writer, id, &method, params).await;
            }
            Err(e) => {
                eprintln!("cursor ACP 帧解析失败（跳过）: {e}");
            }
        }
    }

    {
        let mut pending = pending.lock().unwrap();
        pending.closed = true;
        for (_id, tx) in pending.map.drain() {
            let _ = tx.send(Err(RpcError {
                code: -1,
                message: "cursor ACP 连接已关闭".to_string(),
                data: None,
            }));
        }
    }

    let _ = notif_tx.send(Arc::new(ServerNotification::ConnectionClosed));
}

const METHOD_NOT_FOUND: i64 = -32601;

async fn auto_answer_server_request<W>(
    writer: &SharedWriter<W>,
    id: i64,
    method: &str,
    params: Value,
) where
    W: AsyncWrite + Unpin,
{
    let line = match super::protocol::auto_response(method, &params).result() {
        Some(result) => codec::encode_response(id, &result),
        None => {
            eprintln!("cursor ACP 反向请求 {method}（id={id}）无自动应答，回 error");
            codec::encode_error_response(id, METHOD_NOT_FOUND, "method not handled by client")
        }
    };
    match line {
        Ok(line) => {
            if let Err(e) = write_line(writer, &line).await {
                eprintln!("回复 cursor ACP 反向请求 {method}（id={id}）失败: {e}");
            }
        }
        Err(e) => eprintln!("编码 cursor ACP 反向请求应答失败: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;

    #[tokio::test]
    async fn auto_answers_session_request_permission_allow_once() {
        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let _client = RpcClient::connect(client_w, tokio::io::BufReader::new(client_r), 16);

        let mut sw = server_w;
        sw.write_all(
            br#"{"jsonrpc":"2.0","id":99,"method":"session/request_permission","params":{"sessionId":"s","toolCall":{"toolCallId":"c1","kind":"execute"},"options":[{"optionId":"allow-once","name":"Allow once","kind":"allow_once"},{"optionId":"reject-once","name":"Reject","kind":"reject_once"}]}}
"#,
        )
        .await
        .unwrap();
        sw.flush().await.unwrap();

        let mut sr = tokio::io::BufReader::new(server_r);
        let mut line = String::new();
        sr.read_line(&mut line).await.unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 99);
        assert_eq!(v["result"]["outcome"]["outcome"], "selected");
        assert_eq!(v["result"]["outcome"]["optionId"], "allow-once");
    }

    #[tokio::test]
    async fn auto_rejects_switch_mode_permission() {
        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let _client = RpcClient::connect(client_w, tokio::io::BufReader::new(client_r), 16);

        let mut sw = server_w;
        sw.write_all(
            br#"{"jsonrpc":"2.0","id":100,"method":"session/request_permission","params":{"toolCall":{"kind":"switch_mode"},"options":[{"optionId":"allow-once","kind":"allow_once"},{"optionId":"reject-once","kind":"reject_once"}]}}
"#,
        )
        .await
        .unwrap();
        sw.flush().await.unwrap();

        let mut sr = tokio::io::BufReader::new(server_r);
        let mut line = String::new();
        sr.read_line(&mut line).await.unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 100);
        assert_eq!(v["result"]["outcome"]["optionId"], "reject-once");
    }

    #[tokio::test]
    async fn permission_without_allow_once_errors() {
        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let _client = RpcClient::connect(client_w, tokio::io::BufReader::new(client_r), 16);

        let mut sw = server_w;
        sw.write_all(
            br#"{"jsonrpc":"2.0","id":101,"method":"session/request_permission","params":{}}
"#,
        )
        .await
        .unwrap();
        sw.flush().await.unwrap();

        let mut sr = tokio::io::BufReader::new(server_r);
        let mut line = String::new();
        sr.read_line(&mut line).await.unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 101);
        assert!(v.get("error").is_some());
        assert!(v.get("result").is_none());
    }

    #[tokio::test]
    async fn auto_answers_cursor_ask_question_skipped() {
        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let _client = RpcClient::connect(client_w, tokio::io::BufReader::new(client_r), 16);

        let mut sw = server_w;
        sw.write_all(
            br#"{"jsonrpc":"2.0","id":7,"method":"cursor/ask_question","params":{}}
"#,
        )
        .await
        .unwrap();
        sw.flush().await.unwrap();

        let mut sr = tokio::io::BufReader::new(server_r);
        let mut line = String::new();
        sr.read_line(&mut line).await.unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["result"]["outcome"]["outcome"], "skipped");
    }

    #[tokio::test]
    async fn auto_answers_cursor_create_plan_rejected() {
        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let _client = RpcClient::connect(client_w, tokio::io::BufReader::new(client_r), 16);

        let mut sw = server_w;
        sw.write_all(
            br#"{"jsonrpc":"2.0","id":8,"method":"cursor/create_plan","params":{}}
"#,
        )
        .await
        .unwrap();
        sw.flush().await.unwrap();

        let mut sr = tokio::io::BufReader::new(server_r);
        let mut line = String::new();
        sr.read_line(&mut line).await.unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 8);
        assert_eq!(v["result"]["outcome"]["outcome"], "rejected");
    }

    #[tokio::test]
    async fn unhandled_server_request_gets_error_reply() {
        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let _client = RpcClient::connect(client_w, tokio::io::BufReader::new(client_r), 16);

        let mut sw = server_w;
        sw.write_all(
            br#"{"jsonrpc":"2.0","id":9,"method":"mcpServer/elicitation/request","params":{}}
"#,
        )
        .await
        .unwrap();
        sw.flush().await.unwrap();

        let mut sr = tokio::io::BufReader::new(server_r);
        let mut line = String::new();
        sr.read_line(&mut line).await.unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 9);
        assert!(v.get("error").is_some());
        assert!(v.get("result").is_none());
    }

    #[tokio::test]
    async fn reader_exit_broadcasts_connection_closed() {
        let (client_w, _server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let client = RpcClient::connect(client_w, tokio::io::BufReader::new(client_r), 16);

        let mut sub = client.subscribe();
        drop(server_w);

        let note = sub
            .recv()
            .await
            .expect("subscriber must receive the synthetic close");
        assert!(matches!(
            note.as_ref(),
            ServerNotification::ConnectionClosed
        ));
        assert!(!client.is_connected());
    }

    #[tokio::test]
    async fn outbound_request_includes_jsonrpc() {
        use tokio::io::AsyncBufReadExt;

        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (_server_w, client_r) = tokio::io::duplex(64 * 1024);
        let client = RpcClient::connect(client_w, tokio::io::BufReader::new(client_r), 16);

        let req = tauri::async_runtime::spawn(async move {
            let _ = client
                .request("initialize", serde_json::json!({"protocolVersion": 1}))
                .await;
        });

        let mut sr = tokio::io::BufReader::new(server_r);
        let mut line = String::new();
        sr.read_line(&mut line).await.unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "initialize");
        req.abort();
    }

    /// A frame that fills the cap with no closing newline tears the connection
    /// down (parity with Codex): reader stops, pending drains, request fails fast.
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
}
