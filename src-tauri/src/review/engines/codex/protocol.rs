//! Typed subset of the codex app-server protocol we use: the `initialize` /
//! `initialized` handshake (v1), `thread/start` (v2), and the streaming
//! `ServerNotification`s. Shapes verified against `codex app-server
//! generate-json-schema` (codex 0.139.0); regenerate from the installed binary
//! when bumping codex.
//!
//! Wire contract: serde `camelCase` (the app-server speaks camelCase, `jsonrpc`
//! field omitted — see `codec`). Outgoing params serialize camelCase; incoming
//! results/notifications deserialize camelCase with `#[serde(default)]` on
//! non-essential fields so a newer server adding fields never breaks us.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Request/notification method names this client issues.
pub mod rpc_methods {
    /// v1 handshake request (expects [`super::InitializeResult`]).
    pub const INITIALIZE: &str = "initialize";
    /// v1 handshake notification sent after `initialize` (no response).
    pub const INITIALIZED: &str = "initialized";
    /// v2 request opening a thread (expects [`super::ThreadStartResult`]).
    pub const THREAD_START: &str = "thread/start";
}

// ---- initialize (v1) ----

/// `initialize` request params. `capabilities` is optional and omitted (PR5
/// declares none); the server fills defaults.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub client_info: ClientInfo,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
}

/// `initialize` result. We surface `user_agent` as the codex version in the
/// status badge; the rest is captured for completeness and forward-compat.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    #[serde(default)]
    pub user_agent: String,
    #[serde(default)]
    pub codex_home: String,
    #[serde(default)]
    pub platform_family: String,
    #[serde(default)]
    pub platform_os: String,
}

// ---- thread/start (v2) ----

/// `thread/start` request params. All fields are optional in the protocol; PR5
/// only sets `cwd` (the repo the review runs against). The rich turn/sandbox
/// params belong to PR6's actual review invocation.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// `thread/start` result. The full response carries many fields (model, sandbox,
/// …); we deserialize only `thread`, ignoring the rest.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartResult {
    pub thread: ThreadRef,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRef {
    pub id: String,
}

// ---- streaming server -> client notifications (v2 subset) ----

/// Notification method strings (single source for [`ServerNotification::from_raw`]
/// + tests).
pub mod methods {
    pub const AGENT_MESSAGE_DELTA: &str = "item/agentMessage/delta";
    pub const COMMAND_EXEC_OUTPUT_DELTA: &str = "command/exec/outputDelta";
    pub const PROCESS_OUTPUT_DELTA: &str = "process/outputDelta";
}

/// Streaming server→client notification (v2 subset + catch-all).
///
/// Robustness contract: [`Self::from_raw`] is **total** — an unrecognized method,
/// or a known method whose params fail to parse, becomes [`Self::Other`]. The
/// rpc reader loop relies on this: an unknown/future notification must never
/// abort the connection (a Medium fail-safe runtime guard per `ai-robust.md`,
/// not a new enforcement mechanism).
#[derive(Debug, Clone)]
pub enum ServerNotification {
    /// `item/agentMessage/delta` — incremental assistant text (`delta: String`).
    AgentMessageDelta(AgentMessageDelta),
    /// `command/exec/outputDelta` / `process/outputDelta` — base64 output chunk
    /// (`deltaBase64`, kept opaque in PR5; PR6 decodes to bytes).
    OutputDelta(OutputDelta),
    /// Any other (unknown/future) notification — method + raw params preserved.
    Other { method: String, params: Value },
}

impl ServerNotification {
    /// Total classifier — never panics, never errors (degrades to [`Self::Other`]).
    pub fn from_raw(method: String, params: Value) -> Self {
        match method.as_str() {
            methods::AGENT_MESSAGE_DELTA => match serde_json::from_value(params.clone()) {
                Ok(d) => Self::AgentMessageDelta(d),
                Err(_) => Self::Other { method, params },
            },
            methods::COMMAND_EXEC_OUTPUT_DELTA | methods::PROCESS_OUTPUT_DELTA => {
                match serde_json::from_value(params.clone()) {
                    Ok(d) => Self::OutputDelta(d),
                    Err(_) => Self::Other { method, params },
                }
            }
            _ => Self::Other { method, params },
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageDelta {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
}

/// Common shape of the two base64 output-delta notifications. They differ only
/// in the process identifier key (`processId` for `command/exec`, `processHandle`
/// for `process/spawn`); both are captured optionally so one struct serves both.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputDelta {
    #[serde(default)]
    pub process_id: Option<String>,
    #[serde(default)]
    pub process_handle: Option<String>,
    #[serde(default)]
    pub stream: String,
    /// Opaque in PR5 — preserved verbatim; PR6 decodes to bytes.
    pub delta_base64: String,
    #[serde(default)]
    pub cap_reached: bool,
}

/// Serde wire-shape locks (the **Medium carrier** for the codex protocol wire
/// contract per `ai-robust.md`): assert camelCase keys are present and snake_case
/// absent on what we serialize, and that what we deserialize lands on the fields
/// the handshake depends on. A field rename surfaces here.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_params_serialize_camel_case() {
        let v = serde_json::to_value(InitializeParams {
            client_info: ClientInfo {
                name: "prmonitor".to_string(),
                title: Some("PR Monitor".to_string()),
                version: "0.1.0".to_string(),
            },
        })
        .expect("InitializeParams serializes");

        assert!(v.get("clientInfo").is_some());
        // snake_case form absent — a rename would surface here.
        assert!(v.get("client_info").is_none());
        assert_eq!(v["clientInfo"]["name"], "prmonitor");
        assert_eq!(v["clientInfo"]["title"], "PR Monitor");
        assert_eq!(v["clientInfo"]["version"], "0.1.0");
    }

    #[test]
    fn client_info_omits_none_title() {
        let v = serde_json::to_value(ClientInfo {
            name: "prmonitor".to_string(),
            title: None,
            version: "0.1.0".to_string(),
        })
        .expect("ClientInfo serializes");
        assert!(v.get("title").is_none());
    }

    #[test]
    fn thread_start_params_omit_none_cwd() {
        let v = serde_json::to_value(ThreadStartParams { cwd: None })
            .expect("ThreadStartParams serializes");
        assert!(v.get("cwd").is_none());

        let v = serde_json::to_value(ThreadStartParams {
            cwd: Some("/repo".to_string()),
        })
        .expect("serializes");
        assert_eq!(v["cwd"], "/repo");
    }

    #[test]
    fn initialize_result_parses_user_agent_ignoring_unknown() {
        let r: InitializeResult = serde_json::from_value(serde_json::json!({
            "userAgent": "codex/0.139.0 (macos)",
            "codexHome": "/h",
            "platformFamily": "unix",
            "platformOs": "macos",
            "unknownFutureField": 7
        }))
        .expect("InitializeResult parses");
        assert_eq!(r.user_agent, "codex/0.139.0 (macos)");
        assert_eq!(r.platform_os, "macos");
    }

    #[test]
    fn thread_start_result_extracts_thread_id() {
        let r: ThreadStartResult = serde_json::from_value(serde_json::json!({
            "thread": { "id": "th_abc", "ephemeral": false },
            "model": "gpt-5.1-codex"
        }))
        .expect("ThreadStartResult parses (extra fields ignored)");
        assert_eq!(r.thread.id, "th_abc");
    }

    #[test]
    fn from_raw_known_agent_message_delta_is_typed() {
        let n = ServerNotification::from_raw(
            methods::AGENT_MESSAGE_DELTA.to_string(),
            serde_json::json!({
                "threadId": "t", "turnId": "u", "itemId": "i", "delta": "hi"
            }),
        );
        match n {
            ServerNotification::AgentMessageDelta(d) => {
                assert_eq!(d.delta, "hi");
                assert_eq!(d.thread_id, "t");
            }
            _ => panic!("expected AgentMessageDelta"),
        }
    }

    #[test]
    fn from_raw_output_delta_handles_both_id_keys() {
        let exec = ServerNotification::from_raw(
            methods::COMMAND_EXEC_OUTPUT_DELTA.to_string(),
            serde_json::json!({
                "processId": "ls-1", "stream": "stdout",
                "deltaBase64": "dGhl", "capReached": false
            }),
        );
        match exec {
            ServerNotification::OutputDelta(d) => {
                assert_eq!(d.delta_base64, "dGhl");
                assert_eq!(d.process_id.as_deref(), Some("ls-1"));
                assert_eq!(d.process_handle, None);
            }
            _ => panic!("expected OutputDelta"),
        }

        let proc = ServerNotification::from_raw(
            methods::PROCESS_OUTPUT_DELTA.to_string(),
            serde_json::json!({
                "processHandle": "cargo-1", "stream": "stderr",
                "deltaBase64": "Zm9v", "capReached": true
            }),
        );
        match proc {
            ServerNotification::OutputDelta(d) => {
                assert_eq!(d.process_handle.as_deref(), Some("cargo-1"));
                assert!(d.cap_reached);
            }
            _ => panic!("expected OutputDelta"),
        }
    }

    #[test]
    fn from_raw_unknown_method_is_other_not_panic() {
        let n = ServerNotification::from_raw(
            "thread/futureThing".to_string(),
            serde_json::json!({ "x": 1 }),
        );
        match n {
            ServerNotification::Other { method, params } => {
                assert_eq!(method, "thread/futureThing");
                assert_eq!(params["x"], 1);
            }
            _ => panic!("unknown method must degrade to Other"),
        }
    }

    #[test]
    fn from_raw_known_method_bad_params_degrades_to_other() {
        // `delta`/`itemId`/… missing → typed parse fails → Other (reader survives).
        let n = ServerNotification::from_raw(
            methods::AGENT_MESSAGE_DELTA.to_string(),
            serde_json::json!({}),
        );
        assert!(matches!(n, ServerNotification::Other { .. }));
    }
}
