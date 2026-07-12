//! Integration tests for the Cursor ACP transport (parity with `codex_handshake.rs`):
//! duplex fake agent speaking ACP JSON-RPC 2.0 — client initialize → authenticate →
//! session/new. Asserts authenticate `methodId` is `cursor_login` and session/new
//! returns a `sessionId`. CI-safe (no real `agent` binary).

use prmonitor_lib::review::engines::cursor::process::CursorProcess;
use prmonitor_lib::review::engines::cursor::protocol::{
    rpc_methods, SessionNewParams, AUTH_METHOD_CURSOR_LOGIN,
};
use prmonitor_lib::review::engines::cursor::rpc::RpcClient;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// CI-safe acceptance: production handshake + session/new over an in-process fake agent.
#[tokio::test]
async fn handshake_then_session_new_over_duplex() {
    let (client_w, server_r) = tokio::io::duplex(8192);
    let (server_w, client_r) = tokio::io::duplex(8192);

    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(server_r).lines();
        let mut out = server_w;
        let mut authenticated = false;
        while let Ok(Some(line)) = reader.next_line().await {
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Every ACP frame must carry jsonrpc 2.0 (unlike codex app-server).
            assert_eq!(
                v["jsonrpc"], "2.0",
                "outbound frame missing jsonrpc: {line}"
            );
            match v["method"].as_str().unwrap_or("") {
                "initialize" => {
                    let id = v["id"].as_i64().unwrap();
                    let resp = format!(
                        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":1,"authMethods":[{{"id":"cursor_login"}}]}}}}"#
                    );
                    out.write_all(resp.as_bytes()).await.unwrap();
                    out.write_all(b"\n").await.unwrap();
                }
                "authenticate" => {
                    let id = v["id"].as_i64().unwrap();
                    assert_eq!(
                        v["params"]["methodId"].as_str(),
                        Some(AUTH_METHOD_CURSOR_LOGIN),
                        "authenticate must use cursor_login"
                    );
                    authenticated = true;
                    let resp = format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#);
                    out.write_all(resp.as_bytes()).await.unwrap();
                    out.write_all(b"\n").await.unwrap();
                }
                "session/new" => {
                    let id = v["id"].as_i64().unwrap();
                    let resp = if authenticated {
                        format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"sessionId":"sess_abc"}}}}"#
                        )
                    } else {
                        format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32000,"message":"not authenticated"}}}}"#
                        )
                    };
                    out.write_all(resp.as_bytes()).await.unwrap();
                    out.write_all(b"\n").await.unwrap();
                }
                _ => {}
            }
        }
    });

    let client = RpcClient::connect(client_w, BufReader::new(client_r), 64);

    let info = CursorProcess::handshake(&client)
        .await
        .expect("production handshake");
    assert_eq!(info.protocol_version, 1);

    let session_id = prmonitor_lib::review::engines::cursor::process::session_new(
        &client,
        SessionNewParams {
            cwd: "/repo".to_string(),
            mcp_servers: vec![],
        },
    )
    .await
    .expect("session/new");
    assert_eq!(session_id, "sess_abc");

    // Sanity: method name constants match what the fake saw.
    assert_eq!(rpc_methods::AUTHENTICATE, "authenticate");
    assert_eq!(rpc_methods::SESSION_NEW, "session/new");

    drop(client);
    let _ = server.await;
}

/// Unknown / future notifications must not break the demux.
#[tokio::test]
async fn unknown_notification_does_not_break_demux() {
    let (client_w, server_r) = tokio::io::duplex(8192);
    let (server_w, client_r) = tokio::io::duplex(8192);

    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(server_r).lines();
        let mut out = server_w;
        while let Ok(Some(line)) = reader.next_line().await {
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v["method"].as_str() == Some("initialize") {
                let id = v["id"].as_i64().unwrap();
                out.write_all(br#"{"jsonrpc":"2.0","method":"some/futureThing","params":{"x":1}}"#)
                    .await
                    .unwrap();
                out.write_all(b"\n").await.unwrap();
                let resp = format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":1,"authMethods":[]}}}}"#
                );
                out.write_all(resp.as_bytes()).await.unwrap();
                out.write_all(b"\n").await.unwrap();
            }
        }
    });

    let client = RpcClient::connect(client_w, BufReader::new(client_r), 64);
    let raw = client
        .request("initialize", serde_json::json!({}))
        .await
        .expect("initialize still resolves after an unknown notification");
    assert_eq!(raw["protocolVersion"], 1);

    drop(client);
    let _ = server.await;
}
