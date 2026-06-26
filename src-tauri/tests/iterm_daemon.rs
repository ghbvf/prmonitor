//! Integration tests for the iTerm daemon transport (#1383), mirroring
//! `tests/codex_handshake.rs`.
//!
//! Two layers:
//! - `handshake_over_duplex` — CI-safe and deterministic: drives the real `RpcClient` +
//!   `ITermDaemonProcess::handshake` over `tokio::io::duplex()` against an in-process fake
//!   daemon, with NO real Python / iTerm (CI has neither).
//! - `real_daemon_*` — `#[ignore]`d: spawns the real `python3 iterm_daemon.py` for local
//!   verification. Requires iTerm running with the Python API enabled + authorized, and
//!   `pip install iterm2`. Run with `cargo test --test iterm_daemon -- --ignored`.

use prmonitor_lib::terminal::process::ITermDaemonProcess;
use prmonitor_lib::terminal::rpc::RpcClient;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The acceptance test, runnable in CI: the production `initialize` handshake completes over
/// an in-process fake daemon and yields the reported iTerm version, then `listSessions`
/// round-trips an array. A regression in `ITermDaemonProcess::handshake` (a reshaped
/// `initialize`) fails here.
#[tokio::test]
async fn handshake_over_duplex() {
    let (client_w, server_r) = tokio::io::duplex(8192);
    let (server_w, client_r) = tokio::io::duplex(8192);

    // Fake daemon: answers `initialize` with an itermVersion and `listSessions` with [].
    let server = tokio::spawn(async move {
        let mut reader = BufReader::new(server_r).lines();
        let mut out = server_w;
        while let Ok(Some(line)) = reader.next_line().await {
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let id = v["id"].as_i64().unwrap_or(0);
            match v["method"].as_str().unwrap_or("") {
                "initialize" => {
                    let resp = format!(r#"{{"id":{id},"result":{{"itermVersion":"3.5.0"}}}}"#);
                    out.write_all(resp.as_bytes()).await.unwrap();
                    out.write_all(b"\n").await.unwrap();
                }
                "listSessions" => {
                    let resp = format!(r#"{{"id":{id},"result":[]}}"#);
                    out.write_all(resp.as_bytes()).await.unwrap();
                    out.write_all(b"\n").await.unwrap();
                }
                _ => {}
            }
        }
    });

    let client = RpcClient::connect(client_w, BufReader::new(client_r), 64);

    // Drive the PRODUCTION handshake orchestration, not a hand-rolled copy.
    let info = ITermDaemonProcess::handshake(&client)
        .await
        .expect("production handshake");
    assert_eq!(info.iterm_version, "3.5.0");

    let raw = client
        .request("listSessions", serde_json::json!({}))
        .await
        .expect("listSessions");
    assert!(raw.is_array());

    drop(client); // aborts the reader task.
    let _ = server.await;
}

/// Local verification against the real daemon. `#[ignore]` keeps it out of CI (which has no
/// iTerm / `iterm2`); run where iTerm is set up:
/// `cargo test --test iterm_daemon -- --ignored`.
#[tokio::test]
#[ignore = "requires iTerm + the iterm2 pip + API authorization; run with: cargo test --test iterm_daemon -- --ignored"]
async fn real_daemon_handshake_and_list_sessions() {
    let script = format!(
        "{}/resources/iterm-daemon/iterm_daemon.py",
        env!("CARGO_MANIFEST_DIR")
    );

    let proc = ITermDaemonProcess::spawn("python3", &script)
        .await
        .expect("spawn + handshake the real daemon");
    // A handshake that reached iTerm reports a version (may be empty if the lib hides it,
    // but the connection itself must have succeeded — `spawn` would have errored otherwise).
    let _ = proc.info.iterm_version.clone();

    let raw = proc
        .client()
        .request("listSessions", serde_json::json!({}))
        .await
        .expect("listSessions against the real iTerm");
    assert!(raw.is_array(), "listSessions returns a JSON array");

    proc.kill_and_reap(); // synchronous kill + detached reap — exercises teardown.
}
