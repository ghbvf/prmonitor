//! Integration tests for the codex app-server transport (issue #6 acceptance:
//! "start app-server, complete handshake, obtain a thread id").
//!
//! Two layers:
//! - `handshake_then_thread_start_over_duplex` — CI-safe and deterministic: drives
//!   the real `RpcClient` over `tokio::io::duplex()` against an in-process fake
//!   server, exercising initialize → initialized → thread/start with NO real
//!   binary (CI has no `codex`).
//! - `real_app_server_*` — `#[ignore]`d: spawns the real `codex app-server` for
//!   local verification (run with `cargo test -- --ignored`).

use prmonitor_lib::review::engines::codex::{CodexProcess, RpcClient};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The acceptance test, runnable in CI: handshake + thread id over an in-process
/// fake server, no real codex binary.
#[tokio::test]
async fn handshake_then_thread_start_over_duplex() {
    // Two pipes: client writes -> server reads; server writes -> client reads.
    let (client_w, server_r) = tokio::io::duplex(8192);
    let (server_w, client_r) = tokio::io::duplex(8192);

    // Fake app-server: scripts responses by echoing the request id, and enforces
    // the protocol precondition — `thread/start` before `initialized` is rejected
    // with `-32016` (as the real server does). So a regression in the production
    // handshake that drops the `initialized` notification fails this test.
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(server_r).lines();
        let mut out = server_w;
        let mut initialized = false;
        while let Ok(Some(line)) = reader.next_line().await {
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match v["method"].as_str().unwrap_or("") {
                "initialize" => {
                    let id = v["id"].as_i64().unwrap();
                    let resp = format!(
                        r#"{{"id":{id},"result":{{"userAgent":"codex/0.139.0","codexHome":"/h","platformFamily":"unix","platformOs":"macos"}}}}"#
                    );
                    out.write_all(resp.as_bytes()).await.unwrap();
                    out.write_all(b"\n").await.unwrap();
                }
                "initialized" => initialized = true, // notification: no response
                "thread/start" => {
                    let id = v["id"].as_i64().unwrap();
                    let resp = if initialized {
                        format!(r#"{{"id":{id},"result":{{"thread":{{"id":"th_abc"}}}}}}"#)
                    } else {
                        format!(
                            r#"{{"id":{id},"error":{{"code":-32016,"message":"not initialized"}}}}"#
                        )
                    };
                    out.write_all(resp.as_bytes()).await.unwrap();
                    out.write_all(b"\n").await.unwrap();
                }
                _ => {}
            }
        }
    });

    // The real client over the in-process transport.
    let client = RpcClient::connect(client_w, BufReader::new(client_r), 64);

    // Drive the PRODUCTION handshake orchestration (initialize → initialized), not
    // a hand-rolled copy — a regression in `CodexProcess::handshake` surfaces here.
    let info = CodexProcess::handshake(&client)
        .await
        .expect("production handshake");
    assert_eq!(info.user_agent, "codex/0.139.0");

    // thread/start -> thread id (the acceptance assertion). The fake server only
    // answers it once `initialized` has arrived, so the handshake above is required.
    let raw = client
        .request("thread/start", serde_json::json!({"cwd": "/repo"}))
        .await
        .expect("thread/start");
    assert_eq!(raw["thread"]["id"], "th_abc");

    drop(client); // aborts the reader task.
    let _ = server.await;
}

/// Unknown / future notifications must not break the demux: the client keeps
/// working after the server emits one.
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
                // Emit an unknown notification BEFORE the response.
                out.write_all(br#"{"method":"some/futureThing","params":{"x":1}}"#)
                    .await
                    .unwrap();
                out.write_all(b"\n").await.unwrap();
                let resp = format!(r#"{{"id":{id},"result":{{"userAgent":"codex/x"}}}}"#);
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
    assert_eq!(raw["userAgent"], "codex/x");

    drop(client);
    let _ = server.await;
}

/// A request must fail promptly (not hang to its 30s timeout) when the server
/// disconnects, and `is_connected()` must flip to false — the reader's EOF drain.
#[tokio::test]
async fn request_fails_fast_when_server_disconnects() {
    let (client_w, server_r) = tokio::io::duplex(8192);
    let (server_w, client_r) = tokio::io::duplex(8192);

    // Server reads the request, then drops its write half (EOF for the client)
    // WITHOUT responding.
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(server_r).lines();
        let _ = reader.next_line().await;
        drop(server_w);
    });

    let client = RpcClient::connect(client_w, BufReader::new(client_r), 64);
    assert!(client.is_connected());

    let res = client.request("initialize", serde_json::json!({})).await;
    assert!(
        res.is_err(),
        "request should fail on disconnect, not hang to timeout"
    );
    assert!(!client.is_connected(), "is_connected flips false after EOF");

    let _ = server.await;
}

/// Local verification against the real binary. `#[ignore]` keeps it out of CI
/// (which has no `codex`); run with `cargo test -- --ignored` where codex is
/// installed. Also exercises the production teardown path: `shutdown()` kills
/// synchronously then reaps (`kill_and_reap`, no `block_on`), so it is safe here.
#[tokio::test]
#[ignore = "requires the real codex binary; run with: cargo test -- --ignored"]
async fn real_app_server_handshake_thread_and_reuse() {
    use prmonitor_lib::review::engines::codex::process::start_thread;
    use prmonitor_lib::review::engines::codex::protocol::ThreadStartParams;
    use prmonitor_lib::review::engines::codex::{CodexManager, CodexProcess};

    let repo_root = env!("CARGO_MANIFEST_DIR");

    // (a) Direct process: spawn + handshake yields a populated userAgent, and
    // thread/start (via the session-layer free helper over `client()`) yields a
    // non-empty thread id.
    let proc = CodexProcess::spawn("codex", repo_root)
        .await
        .expect("spawn + handshake");
    assert!(
        !proc.info.user_agent.is_empty(),
        "userAgent populated by initialize"
    );
    let tid = start_thread(
        &proc.client(),
        ThreadStartParams {
            cwd: Some(repo_root.to_string()),
        },
    )
    .await
    .expect("thread/start");
    assert!(!tid.is_empty(), "got a thread id");
    drop(proc);

    // (b) Manager: ensure_started is idempotent (second call reuses the resident
    // connection, returning consistent handshake info), then drop tears it down.
    let mgr = CodexManager::default();
    let info1 = mgr.ensure_started("codex", repo_root).await.expect("first");
    let info2 = mgr
        .ensure_started("codex", repo_root)
        .await
        .expect("second reuses");
    assert_eq!(info1.user_agent, info2.user_agent);
    mgr.shutdown(); // synchronous kill + detached reap — exercises the teardown.
}
