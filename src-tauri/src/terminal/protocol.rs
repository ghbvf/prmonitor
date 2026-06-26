//! Typed subset of the iTerm daemon JSON-RPC protocol (#1383).
//!
//! The daemon is a long-resident `python3 iterm_daemon.py` child that bridges the
//! iTerm2 Python API to NDJSON JSON-RPC over stdio (no `jsonrpc` field — see
//! [`super::codec`], MCP-style). UNLIKE the codex app-server, the daemon NEVER sends
//! server→client requests (no reverse approval), so the inbound classes are only
//! responses + notifications.
//!
//! Wire contract: serde `camelCase` both directions. Outgoing params serialize
//! camelCase; incoming results/notifications deserialize camelCase with
//! `#[serde(default)]` on non-essential fields so a daemon that adds a field never
//! breaks us. The `TerminalSession` / `CreateSessionOpts` payloads are the SHARED
//! `crate::model` contract (also mirrored in `src/types.ts`); everything else here is
//! slice-private transport plumbing.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Connection-level error message for a torn-down daemon transport (reader EOF / oversized
/// frame / shutdown). SINGLE SOURCE so the rpc reader's drain, the manager pump's closed-arm,
/// and `map_notification`'s synthetic-close path all emit one identical user-visible message
/// (a literal copy would silently drift). Slice-private (`pub(crate)`, only referenced inside
/// `crate::terminal`, per the slice-boundary charter).
pub(crate) const ERR_DAEMON_CLOSED: &str = "iTerm daemon 连接已关闭";

/// Status / error message for a user-stopped daemon (the `stop` short-circuit). SINGLE SOURCE
/// shared by the manager's stop refusal + stopped-status struct and its golden wire test.
/// Slice-private (`pub(crate)`, only referenced inside `crate::terminal`).
pub(crate) const ERR_DAEMON_STOPPED: &str = "iTerm daemon 已停止";

/// Request method names this client issues to the daemon (single source for the
/// backend call sites + tests).
pub mod rpc_methods {
    /// Handshake request (expects [`super::InitializeResult`]). The daemon connects to
    /// iTerm INSIDE this handler, so ImportError / iTerm-not-running / API-unauthorized
    /// surface as a structured JSON-RPC error here, never a silent EOF.
    pub const INITIALIZE: &str = "initialize";
    /// List every iTerm session (expects `Vec<crate::model::TerminalSession>`).
    pub const LIST_SESSIONS: &str = "listSessions";
    /// Create a new session (expects one `crate::model::TerminalSession`).
    pub const CREATE_SESSION: &str = "createSession";
    /// Type text into a session (ack result, ignored).
    pub const SEND_TEXT: &str = "sendText";
    /// Start streaming a session's screen (expects [`super::SubscribeResult`]).
    pub const SUBSCRIBE: &str = "subscribe";
    /// Stop streaming a session's screen (ack result, ignored).
    pub const UNSUBSCRIBE: &str = "unsubscribe";
    /// Resize a session's grid (ack result, ignored).
    pub const RESIZE: &str = "resize";
}

/// Server→client notification method strings (single source for
/// [`ServerNotification::from_raw`] + tests).
pub mod notif_methods {
    /// A full visible-screen snapshot for a subscribed session.
    pub const SCREEN_UPDATE: &str = "screenUpdate";
    /// A subscribed session's tab/window closed (its streamer ended).
    pub const SESSION_ENDED: &str = "sessionEnded";
    /// A per-session (or connection-level) streaming error; the daemon survives.
    pub const ERROR: &str = "error";
}

// ---- requests ----

/// `initialize` request params — empty object `{}` (a unit struct would serialize to
/// `null`; an empty struct serializes to `{}`, matching the daemon's expected shape).
#[derive(Debug, Clone, Serialize)]
pub struct InitializeParams {}

/// `sendText` request params.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendTextParams<'a> {
    pub session_id: &'a str,
    pub text: &'a str,
}

/// `subscribe` request params.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeParams<'a> {
    pub session_id: &'a str,
}

/// `unsubscribe` request params.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnsubscribeParams<'a> {
    pub session_id: &'a str,
}

/// `resize` request params.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResizeParams<'a> {
    pub session_id: &'a str,
    pub cols: u16,
    pub rows: u16,
}

// ---- results ----

/// `initialize` result. We surface `iterm_version` in the status badge; a daemon that
/// adds fields never breaks us (`default` on every field).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    #[serde(default)]
    pub iterm_version: String,
}

/// `subscribe` result — the session's current grid, used to emit the one-shot
/// [`crate::events::TerminalEvent::Attached`] (the daemon does NOT send an `attached`
/// notification; the backend synthesizes it from this ack so the panel flips connected
/// before the first `screenUpdate`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeResult {
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
}

// ---- streaming server -> client notifications ----

/// A streamed daemon notification (typed subset + catch-all).
///
/// Robustness contract: [`Self::from_raw`] is **total** — an unrecognized method, or a
/// known method whose params fail to parse, becomes [`Self::Other`]. The rpc reader loop
/// relies on this: an unknown/future notification must never abort the connection (a
/// Medium fail-safe runtime guard per `ai-robust.md`, not a new enforcement mechanism).
#[derive(Debug, Clone)]
pub enum ServerNotification {
    /// `screenUpdate` — a full visible-screen snapshot.
    ScreenUpdate(ScreenUpdateNotification),
    /// `sessionEnded` — a subscribed session's tab/window closed.
    SessionEnded(SessionEndedNotification),
    /// `error` — a per-session (or connection-level) streaming error.
    Error(ErrorNotification),
    /// Synthetic, **not** a wire notification: the rpc reader injects this on the
    /// broadcast when its loop exits (daemon EOF / IO error / oversized frame), so the
    /// per-connection pump sees the transport tearing down and emits a terminal error
    /// instead of hanging on `recv()` forever (the `RpcClient` keeps the broadcast
    /// `Sender` alive across a dead reader, so `RecvError::Closed` would otherwise never
    /// fire). [`Self::from_raw`] never produces it.
    ConnectionClosed,
    /// Any other (unknown/future) notification — method + raw params preserved.
    Other { method: String, params: Value },
}

impl ServerNotification {
    /// Total classifier — never panics, never errors (degrades to [`Self::Other`]).
    pub fn from_raw(method: String, params: Value) -> Self {
        match method.as_str() {
            notif_methods::SCREEN_UPDATE => match serde_json::from_value(params.clone()) {
                Ok(d) => Self::ScreenUpdate(d),
                Err(_) => Self::Other { method, params },
            },
            notif_methods::SESSION_ENDED => match serde_json::from_value(params.clone()) {
                Ok(d) => Self::SessionEnded(d),
                Err(_) => Self::Other { method, params },
            },
            notif_methods::ERROR => match serde_json::from_value(params.clone()) {
                Ok(d) => Self::Error(d),
                Err(_) => Self::Other { method, params },
            },
            _ => Self::Other { method, params },
        }
    }
}

/// `screenUpdate` payload — a full visible-screen snapshot (`contents` is the rendered
/// grid). `cursor_row` / `cursor_col` are 0-based; omitted when the daemon can't resolve
/// them.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenUpdateNotification {
    pub session_id: String,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub contents: String,
    #[serde(default)]
    pub cursor_row: Option<u16>,
    #[serde(default)]
    pub cursor_col: Option<u16>,
}

/// `sessionEnded` payload — a subscribed session's tab/window closed.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEndedNotification {
    pub session_id: String,
    #[serde(default)]
    pub reason: String,
}

/// `error` payload — `session_id` omitted for a connection-level error not tied to one
/// session.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorNotification {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub message: String,
}

/// Serde wire-shape locks (the **Medium carrier** for the daemon protocol wire contract
/// per `ai-robust.md`): assert camelCase keys are present and snake_case absent on what we
/// serialize, that what we deserialize lands on the fields the handshake/stream depend on,
/// and that `from_raw` classifies known/unknown methods totally (the fail-safe guard).
#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled iTerm daemon source, embedded at compile time (no Python runtime needed at
    /// test time). Relative to THIS file (`src-tauri/src/terminal/protocol.rs`): `../../`
    /// climbs to `src-tauri/`, then into `resources/iterm-daemon/`.
    const DAEMON_SRC: &str = include_str!("../../resources/iterm-daemon/iterm_daemon.py");

    /// Extract the method-name KEYS of the daemon's `HANDLERS` dict (the request methods it
    /// dispatches). The dict is a flat literal with no nested braces, so the first `}` after
    /// `HANDLERS = {` closes it; each entry line is `"method": "handle_xxx",` and we take the
    /// first quoted literal (the key). Returns sorted for set comparison.
    fn parse_daemon_handler_methods() -> Vec<String> {
        let start = DAEMON_SRC
            .find("HANDLERS = {")
            .expect("daemon must define a HANDLERS dict");
        let rest = &DAEMON_SRC[start..];
        let close = rest.find('}').expect("HANDLERS dict must close with `}`");
        let mut methods: Vec<String> = rest[..close]
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let after = line.strip_prefix('"')?; // dict KEY starts the entry line.
                let end = after.find('"')?;
                Some(after[..end].to_string())
            })
            .collect();
        methods.sort();
        methods
    }

    /// Extract the notification method names the daemon EMITS, by scanning every
    /// `self.send_notification("name", ...)` call site (the `async def send_notification`
    /// header uses `def send_notification(`, so keying on `self.send_notification(` skips it)
    /// and taking the first quoted literal after the call paren (which may sit on the next
    /// line). Returns sorted + deduped for set comparison.
    fn parse_daemon_notification_methods() -> Vec<String> {
        const CALL: &str = "self.send_notification(";
        let mut names = Vec::new();
        let mut search = DAEMON_SRC;
        while let Some(pos) = search.find(CALL) {
            let after = &search[pos + CALL.len()..];
            if let Some(q) = after.find('"') {
                let lit = &after[q + 1..];
                if let Some(end) = lit.find('"') {
                    names.push(lit[..end].to_string());
                }
            }
            search = after;
        }
        names.sort();
        names.dedup();
        names
    }

    /// Medium machine-checked contract: the Python daemon's `HANDLERS` request methods must
    /// EXACTLY mirror the Rust `rpc_methods` constants. Drift on EITHER side fails CI — a
    /// renamed/added/removed handler in Python, or a changed constant VALUE in Rust. (The Rust
    /// side is hand-listed because module constants can't be enumerated at runtime; a changed
    /// constant value is still caught since the list references the real constants.)
    #[test]
    fn python_handlers_mirror_rust_rpc_methods() {
        let mut rust: Vec<String> = [
            rpc_methods::INITIALIZE,
            rpc_methods::LIST_SESSIONS,
            rpc_methods::CREATE_SESSION,
            rpc_methods::SEND_TEXT,
            rpc_methods::SUBSCRIBE,
            rpc_methods::UNSUBSCRIBE,
            rpc_methods::RESIZE,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        rust.sort();

        assert_eq!(
            parse_daemon_handler_methods(),
            rust,
            "Python HANDLERS must exactly mirror Rust rpc_methods (drift on either side fails CI)"
        );
    }

    /// Medium machine-checked contract: the notification method names the daemon emits must
    /// EXACTLY mirror the Rust `notif_methods` constants. Bidirectional — a new Python
    /// `send_notification` name not mirrored in Rust, or a changed Rust value, fails CI.
    #[test]
    fn python_notifications_mirror_rust_notif_methods() {
        let mut rust: Vec<String> = [
            notif_methods::SCREEN_UPDATE,
            notif_methods::SESSION_ENDED,
            notif_methods::ERROR,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        rust.sort();

        assert_eq!(
            parse_daemon_notification_methods(),
            rust,
            "Python send_notification names must exactly mirror Rust notif_methods (drift on either side fails CI)"
        );
    }

    #[test]
    fn initialize_params_serialize_to_empty_object() {
        let v = serde_json::to_value(InitializeParams {}).expect("InitializeParams serializes");
        assert!(v.is_object());
        assert_eq!(v.as_object().expect("object").len(), 0);
    }

    #[test]
    fn send_text_params_serialize_camel_case() {
        let v = serde_json::to_value(SendTextParams {
            session_id: "w0t0p0",
            text: "ls\n",
        })
        .expect("SendTextParams serializes");
        assert_eq!(v["sessionId"], "w0t0p0");
        assert_eq!(v["text"], "ls\n");
        // snake_case absent — a rename would surface here.
        assert!(v.get("session_id").is_none());
    }

    #[test]
    fn subscribe_and_unsubscribe_params_serialize_camel_case() {
        let s = serde_json::to_value(SubscribeParams { session_id: "p0" })
            .expect("SubscribeParams serializes");
        assert_eq!(s["sessionId"], "p0");
        assert!(s.get("session_id").is_none());

        let u = serde_json::to_value(UnsubscribeParams { session_id: "p0" })
            .expect("UnsubscribeParams serializes");
        assert_eq!(u["sessionId"], "p0");
    }

    #[test]
    fn resize_params_serialize_camel_case() {
        let v = serde_json::to_value(ResizeParams {
            session_id: "p0",
            cols: 120,
            rows: 40,
        })
        .expect("ResizeParams serializes");
        assert_eq!(v["sessionId"], "p0");
        assert_eq!(v["cols"], 120);
        assert_eq!(v["rows"], 40);
        assert!(v.get("session_id").is_none());
    }

    #[test]
    fn initialize_result_parses_version_ignoring_unknown() {
        let r: InitializeResult = serde_json::from_value(serde_json::json!({
            "itermVersion": "3.5.0",
            "unknownFutureField": 7
        }))
        .expect("InitializeResult parses");
        assert_eq!(r.iterm_version, "3.5.0");
    }

    #[test]
    fn subscribe_result_parses_grid() {
        let r: SubscribeResult = serde_json::from_value(serde_json::json!({
            "cols": 80, "rows": 24, "extra": true
        }))
        .expect("SubscribeResult parses");
        assert_eq!(r.cols, 80);
        assert_eq!(r.rows, 24);
    }

    #[test]
    fn subscribe_result_defaults_cols_rows_to_zero_when_absent() {
        // `#[serde(default)]` on both fields: an empty object still parses, defaulting cols/rows
        // to 0 — the path that yields `TerminalEvent::Attached { cols: 0, rows: 0 }` when the
        // daemon's subscribe ack omits the grid. Documents the default→0 behavior.
        let r: SubscribeResult =
            serde_json::from_value(serde_json::json!({})).expect("SubscribeResult parses empty");
        assert_eq!(r.cols, 0);
        assert_eq!(r.rows, 0);
    }

    #[test]
    fn from_raw_screen_update_is_typed() {
        let n = ServerNotification::from_raw(
            notif_methods::SCREEN_UPDATE.to_string(),
            serde_json::json!({
                "sessionId": "p0", "cols": 80, "rows": 24, "contents": "$ ",
                "cursorRow": 0, "cursorCol": 2
            }),
        );
        match n {
            ServerNotification::ScreenUpdate(d) => {
                assert_eq!(d.session_id, "p0");
                assert_eq!(d.cols, 80);
                assert_eq!(d.contents, "$ ");
                assert_eq!(d.cursor_row, Some(0));
                assert_eq!(d.cursor_col, Some(2));
            }
            _ => panic!("expected ScreenUpdate"),
        }
    }

    #[test]
    fn from_raw_screen_update_without_cursor_defaults_none() {
        let n = ServerNotification::from_raw(
            notif_methods::SCREEN_UPDATE.to_string(),
            serde_json::json!({ "sessionId": "p0", "cols": 80, "rows": 24, "contents": "" }),
        );
        match n {
            ServerNotification::ScreenUpdate(d) => {
                assert_eq!(d.cursor_row, None);
                assert_eq!(d.cursor_col, None);
            }
            _ => panic!("expected ScreenUpdate"),
        }
    }

    #[test]
    fn from_raw_session_ended_is_typed() {
        let n = ServerNotification::from_raw(
            notif_methods::SESSION_ENDED.to_string(),
            serde_json::json!({ "sessionId": "p0", "reason": "closed" }),
        );
        match n {
            ServerNotification::SessionEnded(d) => {
                assert_eq!(d.session_id, "p0");
                assert_eq!(d.reason, "closed");
            }
            _ => panic!("expected SessionEnded"),
        }
    }

    #[test]
    fn from_raw_error_with_and_without_session() {
        let with = ServerNotification::from_raw(
            notif_methods::ERROR.to_string(),
            serde_json::json!({ "sessionId": "p0", "message": "boom" }),
        );
        match with {
            ServerNotification::Error(d) => {
                assert_eq!(d.session_id.as_deref(), Some("p0"));
                assert_eq!(d.message, "boom");
            }
            _ => panic!("expected Error"),
        }

        let without = ServerNotification::from_raw(
            notif_methods::ERROR.to_string(),
            serde_json::json!({ "message": "daemon down" }),
        );
        match without {
            ServerNotification::Error(d) => {
                assert_eq!(d.session_id, None);
                assert_eq!(d.message, "daemon down");
            }
            _ => panic!("expected Error"),
        }
    }

    #[test]
    fn from_raw_unknown_method_is_other_not_panic() {
        let n =
            ServerNotification::from_raw("futureThing".to_string(), serde_json::json!({ "x": 1 }));
        match n {
            ServerNotification::Other { method, params } => {
                assert_eq!(method, "futureThing");
                assert_eq!(params["x"], 1);
            }
            _ => panic!("unknown method must degrade to Other"),
        }
    }

    #[test]
    fn from_raw_known_method_bad_params_degrades_to_other() {
        // `sessionId` missing → typed parse fails → Other (reader survives).
        let n = ServerNotification::from_raw(
            notif_methods::SCREEN_UPDATE.to_string(),
            serde_json::json!({}),
        );
        assert!(matches!(n, ServerNotification::Other { .. }));
    }

    #[test]
    fn from_raw_session_ended_missing_session_id_degrades_to_other() {
        // `session_id` is REQUIRED (no `#[serde(default)]`), so empty params fail the typed
        // parse → degrade to Other (the reader survives an unknown/future shape). Contrast with
        // `error` below, whose every field defaults.
        let n = ServerNotification::from_raw(
            notif_methods::SESSION_ENDED.to_string(),
            serde_json::json!({}),
        );
        assert!(matches!(n, ServerNotification::Other { .. }));
    }

    #[test]
    fn from_raw_error_empty_params_is_typed_error_not_other() {
        // EVERY `error` field defaults (`session_id: None`, `message: ""`), so empty params
        // still parse to a typed `Error` (NOT Other) — the required-vs-optional divergence from
        // `sessionEnded`.
        let n =
            ServerNotification::from_raw(notif_methods::ERROR.to_string(), serde_json::json!({}));
        match n {
            ServerNotification::Error(d) => {
                assert_eq!(d.session_id, None);
                assert_eq!(d.message, "");
            }
            _ => panic!("error with all-defaulting fields must be a typed Error, not Other"),
        }
    }
}
