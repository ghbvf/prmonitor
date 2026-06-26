//! iTerm daemon backend — the MVP [`super::backend::TerminalBackend`] impl (#1383).
//!
//! [`ITermBackend`] is a per-request handle that borrows the composition root's pieces
//! (`app`, the resident `daemon`, the `python_bin`, the resolved `script_path`). Each trait
//! method ENSURES the daemon (lazily spawning + handshaking on first use) then issues one
//! typed JSON-RPC request. The request bodies are free helpers generic over the writer half
//! (`W`) so they are CI-testable over `tokio::io::duplex()` against an in-process fake daemon
//! — exactly like the codex transport — WITHOUT spawning real python.
//!
//! The pure [`map_notification`] lives here (the manager's pump calls it): it maps each
//! daemon [`ServerNotification`] to a [`TerminalEvent`], unit-tested below.

use std::sync::Arc;

use tauri::{AppHandle, Runtime};
use tokio::io::AsyncWrite;
use tokio::process::ChildStdin;

use super::backend::TerminalBackend;
use super::manager::ITermDaemonManager;
use super::protocol::{
    rpc_methods, ResizeParams, SendTextParams, ServerNotification, SubscribeParams,
    SubscribeResult, UnsubscribeParams, ERR_DAEMON_CLOSED,
};
use super::rpc::RpcClient;
use crate::error::{AppError, AppResult};
use crate::events::{StreamEvent, TerminalEvent};
use crate::model::{CreateSessionOpts, TerminalSession};

/// A per-request iTerm backend handle. Cheap to build per command (it only borrows); the
/// expensive resident state (the daemon process) lives in [`ITermDaemonManager`].
pub struct ITermBackend<'a, R: Runtime> {
    app: &'a AppHandle<R>,
    daemon: &'a ITermDaemonManager,
    python_bin: &'a str,
    script_path: &'a str,
}

impl<'a, R: Runtime> ITermBackend<'a, R> {
    /// Build a handle borrowing the composition root's pieces (the command layer supplies
    /// them: `&app`, `&state.terminal`, the `PYTHON_BIN` const, the resolved script path).
    pub fn new(
        app: &'a AppHandle<R>,
        daemon: &'a ITermDaemonManager,
        python_bin: &'a str,
        script_path: &'a str,
    ) -> Self {
        Self {
            app,
            daemon,
            python_bin,
            script_path,
        }
    }

    /// Ensure the resident daemon and get a callable client (lazy spawn on first use).
    async fn ensure(&self) -> AppResult<Arc<RpcClient<ChildStdin>>> {
        self.daemon
            .ensure_started(self.app, self.python_bin, self.script_path)
            .await
    }
}

impl<R: Runtime> TerminalBackend for ITermBackend<'_, R> {
    async fn list_sessions(&self) -> AppResult<Vec<TerminalSession>> {
        let client = self.ensure().await?;
        request_list_sessions(&client).await
    }

    async fn create_session(&self, opts: CreateSessionOpts) -> AppResult<TerminalSession> {
        let client = self.ensure().await?;
        request_create_session(&client, &opts).await
    }

    async fn send_text(&self, session_id: &str, text: &str) -> AppResult<()> {
        let client = self.ensure().await?;
        request_send_text(&client, session_id, text).await
    }

    async fn subscribe(&self, session_id: &str) -> AppResult<()> {
        let client = self.ensure().await?;
        let res = request_subscribe(&client, session_id).await?;
        // The daemon sends NO `attached` notification (its stream is screenUpdate/sessionEnded/
        // error only); synthesize the one-shot `Attached` from the subscribe ack so the panel
        // flips to a connected state and seeds the xterm grid BEFORE the first `screenUpdate`.
        //
        // NOTE: this `Attached` emit side-effect is covered ONLY by the `#[ignore]`d real-daemon
        // integration test (`tests/iterm_daemon.rs`); no CI test asserts its DELIVERY because the
        // Tauri `mock_app` `emit` is fire-and-forget with no observable sink. The RPC half this
        // depends on — `request_subscribe`'s camelCase wire shape + `SubscribeResult` parse — IS
        // locked by the duplex test below; only this emit hop is the uncovered coverage boundary.
        crate::stream::emit(
            self.app,
            StreamEvent::Terminal(TerminalEvent::Attached {
                session_id: session_id.to_string(),
                cols: res.cols,
                rows: res.rows,
            }),
        );
        Ok(())
    }

    async fn unsubscribe(&self, session_id: &str) -> AppResult<()> {
        let client = self.ensure().await?;
        request_unsubscribe(&client, session_id).await
    }

    async fn resize(&self, session_id: &str, cols: u16, rows: u16) -> AppResult<()> {
        let client = self.ensure().await?;
        request_resize(&client, session_id, cols, rows).await
    }
}

// ---- typed request bodies (generic over the write half so they're duplex-testable) ----

async fn request_list_sessions<W>(client: &RpcClient<W>) -> AppResult<Vec<TerminalSession>>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let raw = client
        .request(rpc_methods::LIST_SESSIONS, serde_json::json!({}))
        .await?;
    serde_json::from_value(raw).map_err(|e| AppError::new(format!("解析 listSessions 失败: {e}")))
}

async fn request_create_session<W>(
    client: &RpcClient<W>,
    opts: &CreateSessionOpts,
) -> AppResult<TerminalSession>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let params = serde_json::to_value(opts)
        .map_err(|e| AppError::new(format!("编码 createSession 失败: {e}")))?;
    let raw = client.request(rpc_methods::CREATE_SESSION, params).await?;
    serde_json::from_value(raw).map_err(|e| AppError::new(format!("解析 createSession 失败: {e}")))
}

async fn request_send_text<W>(client: &RpcClient<W>, session_id: &str, text: &str) -> AppResult<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let params = serde_json::to_value(SendTextParams { session_id, text })
        .map_err(|e| AppError::new(format!("编码 sendText 失败: {e}")))?;
    client.request(rpc_methods::SEND_TEXT, params).await?;
    Ok(())
}

async fn request_subscribe<W>(client: &RpcClient<W>, session_id: &str) -> AppResult<SubscribeResult>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let params = serde_json::to_value(SubscribeParams { session_id })
        .map_err(|e| AppError::new(format!("编码 subscribe 失败: {e}")))?;
    let raw = client.request(rpc_methods::SUBSCRIBE, params).await?;
    serde_json::from_value(raw).map_err(|e| AppError::new(format!("解析 subscribe 失败: {e}")))
}

async fn request_unsubscribe<W>(client: &RpcClient<W>, session_id: &str) -> AppResult<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let params = serde_json::to_value(UnsubscribeParams { session_id })
        .map_err(|e| AppError::new(format!("编码 unsubscribe 失败: {e}")))?;
    client.request(rpc_methods::UNSUBSCRIBE, params).await?;
    Ok(())
}

async fn request_resize<W>(
    client: &RpcClient<W>,
    session_id: &str,
    cols: u16,
    rows: u16,
) -> AppResult<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let params = serde_json::to_value(ResizeParams {
        session_id,
        cols,
        rows,
    })
    .map_err(|e| AppError::new(format!("编码 resize 失败: {e}")))?;
    client.request(rpc_methods::RESIZE, params).await?;
    Ok(())
}

/// Map one daemon notification to a [`TerminalEvent`] for the frontend, or `None` for a
/// notification class we don't forward (an unknown/future `Other`). Total over every
/// [`ServerNotification`] variant — the synthetic [`ServerNotification::ConnectionClosed`]
/// maps to a connection-level [`TerminalEvent::Error`] (no `session_id`) so the pump can
/// emit a terminal error when the transport dies. Pure — unit-tested below.
pub(super) fn map_notification(note: &ServerNotification) -> Option<TerminalEvent> {
    match note {
        ServerNotification::ScreenUpdate(d) => Some(TerminalEvent::ScreenUpdate {
            session_id: d.session_id.clone(),
            cols: d.cols,
            rows: d.rows,
            contents: d.contents.clone(),
            cursor_row: d.cursor_row,
            cursor_col: d.cursor_col,
        }),
        ServerNotification::SessionEnded(d) => Some(TerminalEvent::SessionEnded {
            session_id: d.session_id.clone(),
            reason: d.reason.clone(),
        }),
        ServerNotification::Error(d) => Some(TerminalEvent::Error {
            session_id: d.session_id.clone(),
            message: d.message.clone(),
        }),
        ServerNotification::ConnectionClosed => Some(TerminalEvent::Error {
            session_id: None,
            message: ERR_DAEMON_CLOSED.to_string(),
        }),
        ServerNotification::Other { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::protocol::{
        ErrorNotification, ScreenUpdateNotification, SessionEndedNotification,
    };
    use serde_json::Value;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn map_notification_screen_update() {
        let ev = map_notification(&ServerNotification::ScreenUpdate(
            ScreenUpdateNotification {
                session_id: "p0".to_string(),
                cols: 80,
                rows: 24,
                contents: "$ ".to_string(),
                cursor_row: Some(0),
                cursor_col: Some(2),
            },
        ))
        .expect("maps");
        match ev {
            TerminalEvent::ScreenUpdate {
                session_id,
                cols,
                rows,
                contents,
                cursor_row,
                cursor_col,
            } => {
                assert_eq!(session_id, "p0");
                assert_eq!(cols, 80);
                assert_eq!(rows, 24);
                assert_eq!(contents, "$ ");
                assert_eq!(cursor_row, Some(0));
                assert_eq!(cursor_col, Some(2));
            }
            other => panic!("expected ScreenUpdate, got {other:?}"),
        }
    }

    #[test]
    fn map_notification_session_ended() {
        let ev = map_notification(&ServerNotification::SessionEnded(
            SessionEndedNotification {
                session_id: "p0".to_string(),
                reason: "closed".to_string(),
            },
        ))
        .expect("maps");
        assert!(matches!(
            ev,
            TerminalEvent::SessionEnded { session_id, reason }
                if session_id == "p0" && reason == "closed"
        ));
    }

    #[test]
    fn map_notification_error_preserves_optional_session() {
        let with = map_notification(&ServerNotification::Error(ErrorNotification {
            session_id: Some("p0".to_string()),
            message: "boom".to_string(),
        }))
        .expect("maps");
        assert!(matches!(
            with,
            TerminalEvent::Error { session_id: Some(ref s), ref message }
                if s == "p0" && message == "boom"
        ));

        let without = map_notification(&ServerNotification::Error(ErrorNotification {
            session_id: None,
            message: "daemon down".to_string(),
        }))
        .expect("maps");
        assert!(matches!(
            without,
            TerminalEvent::Error {
                session_id: None,
                ..
            }
        ));
    }

    #[test]
    fn map_notification_connection_closed_is_connection_level_error() {
        let ev = map_notification(&ServerNotification::ConnectionClosed).expect("maps");
        assert!(matches!(
            ev,
            TerminalEvent::Error {
                session_id: None,
                ..
            }
        ));
    }

    #[test]
    fn map_notification_other_is_ignored() {
        let ev = map_notification(&ServerNotification::Other {
            method: "futureThing".to_string(),
            params: serde_json::json!({}),
        });
        assert!(
            ev.is_none(),
            "unknown/future notifications are not forwarded"
        );
    }

    /// Drive ALL SIX typed request bodies (listSessions / createSession / sendText / subscribe /
    /// unsubscribe / resize) over `tokio::io::duplex()` against an in-process fake daemon (no
    /// real python), asserting each request's WIRE SHAPE is camelCase and the typed results
    /// parse. Exercises the same request path the `ITermBackend` methods use (they only add
    /// `ensure()` in front), so a method-name or params drift surfaces here, not only at runtime.
    #[tokio::test]
    async fn request_bodies_have_camel_case_wire_shape_over_duplex() {
        let (client_w, server_r) = tokio::io::duplex(8192);
        let (server_w, client_r) = tokio::io::duplex(8192);
        let (req_tx, mut req_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();

        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(server_r).lines();
            let mut out = server_w;
            while let Ok(Some(line)) = reader.next_line().await {
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let id = v["id"].as_i64().unwrap_or(0);
                let method = v["method"].as_str().unwrap_or("").to_string();
                let _ = req_tx.send(v.clone());
                let result = match method.as_str() {
                    "listSessions" => serde_json::json!([{
                        "sessionId": "p0", "windowId": "w0", "tabId": "t0",
                        "title": "zsh", "isActive": true, "rows": 24, "cols": 80
                    }]),
                    // Echo the params back into the session so the test can assert the
                    // camelCase params reached the daemon (proves the request shape).
                    "createSession" => {
                        let window_id = v["params"]
                            .get("windowId")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let profile = v["params"]
                            .get("profile")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        serde_json::json!({
                            "sessionId": "new0", "windowId": window_id, "tabId": "t1",
                            "title": profile, "isActive": true, "rows": 24, "cols": 80
                        })
                    }
                    // `subscribe` returns a `SubscribeResult` grid the backend uses for the
                    // one-shot `Attached`; `unsubscribe`/`resize` return a plain ack.
                    "subscribe" => serde_json::json!({ "cols": 100, "rows": 30 }),
                    _ => serde_json::json!({}), // ack for sendText / unsubscribe / resize
                };
                let resp = serde_json::json!({ "id": id, "result": result });
                out.write_all(resp.to_string().as_bytes()).await.unwrap();
                out.write_all(b"\n").await.unwrap();
            }
        });

        let client = RpcClient::connect(client_w, BufReader::new(client_r), 16);

        let sessions = request_list_sessions(&client).await.expect("list_sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "p0");
        assert_eq!(sessions[0].cols, 80);

        let created = request_create_session(
            &client,
            &CreateSessionOpts {
                window_id: Some("w9".to_string()),
                profile: Some("Solarized".to_string()),
            },
        )
        .await
        .expect("create_session");
        // The echoed fields prove the camelCase params round-tripped through the daemon.
        assert_eq!(created.window_id, "w9");
        assert_eq!(created.title, "Solarized");

        request_send_text(&client, "p0", "ls\n")
            .await
            .expect("send_text");

        // The 3 stream-control RPCs: subscribe returns a grid, unsubscribe/resize ack.
        let sub = request_subscribe(&client, "p0").await.expect("subscribe");
        assert_eq!(sub.cols, 100);
        assert_eq!(sub.rows, 30);
        request_unsubscribe(&client, "p0")
            .await
            .expect("unsubscribe");
        request_resize(&client, "p0", 120, 40)
            .await
            .expect("resize");

        // Assert the captured request shapes (camelCase keys present, snake_case absent).
        let r1 = req_rx.recv().await.unwrap();
        assert_eq!(r1["method"], "listSessions");

        let r2 = req_rx.recv().await.unwrap();
        assert_eq!(r2["method"], "createSession");
        assert_eq!(r2["params"]["windowId"], "w9");
        assert_eq!(r2["params"]["profile"], "Solarized");
        assert!(r2["params"].get("window_id").is_none());

        let r3 = req_rx.recv().await.unwrap();
        assert_eq!(r3["method"], "sendText");
        assert_eq!(r3["params"]["sessionId"], "p0");
        assert_eq!(r3["params"]["text"], "ls\n");
        assert!(r3["params"].get("session_id").is_none());

        // Lock the 3 stream-control method names + camelCase param shapes.
        let r4 = req_rx.recv().await.unwrap();
        assert_eq!(r4["method"], "subscribe");
        assert_eq!(r4["params"]["sessionId"], "p0");
        assert!(r4["params"].get("session_id").is_none());

        let r5 = req_rx.recv().await.unwrap();
        assert_eq!(r5["method"], "unsubscribe");
        assert_eq!(r5["params"]["sessionId"], "p0");
        assert!(r5["params"].get("session_id").is_none());

        let r6 = req_rx.recv().await.unwrap();
        assert_eq!(r6["method"], "resize");
        assert_eq!(r6["params"]["sessionId"], "p0");
        assert_eq!(r6["params"]["cols"], 120);
        assert_eq!(r6["params"]["rows"], 40);
        assert!(r6["params"].get("session_id").is_none());

        drop(client);
        let _ = server.await;
    }
}
