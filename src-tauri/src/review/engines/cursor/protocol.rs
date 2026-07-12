//! Typed subset of the Cursor ACP protocol we use: `initialize` / `authenticate` /
//! `session/new` / `session/prompt` / `session/cancel`, plus `session/update`
//! streaming and reverse requests (`session/request_permission`, Cursor extensions).
//!
//! Wire contract: serde `camelCase` + JSON-RPC 2.0 envelope (see `codec`). Outgoing
//! params serialize camelCase; incoming results/notifications deserialize camelCase
//! with `#[serde(default)]` on non-essential fields so a newer agent adding fields
//! never breaks us.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Request/notification method names this client issues.
pub mod rpc_methods {
    pub const INITIALIZE: &str = "initialize";
    pub const AUTHENTICATE: &str = "authenticate";
    pub const SESSION_NEW: &str = "session/new";
    pub const SESSION_PROMPT: &str = "session/prompt";
    /// Fire-and-forget cancel (notification, no response).
    pub const SESSION_CANCEL: &str = "session/cancel";
}

/// Server→client / Cursor extension method strings.
pub mod server_methods {
    pub const SESSION_UPDATE: &str = "session/update";
    pub const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";
    pub const CURSOR_ASK_QUESTION: &str = "cursor/ask_question";
    pub const CURSOR_CREATE_PLAN: &str = "cursor/create_plan";
}

pub const AUTH_METHOD_CURSOR_LOGIN: &str = "cursor_login";

// ---- initialize ----

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: u32,
    pub client_capabilities: ClientCapabilities,
    pub client_info: ClientInfo,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    pub fs: FsCapabilities,
    pub terminal: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCapabilities {
    pub read_text_file: bool,
    pub write_text_file: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    #[serde(default)]
    pub protocol_version: u32,
    #[serde(default)]
    pub agent_capabilities: Value,
    #[serde(default)]
    pub auth_methods: Vec<Value>,
}

// ---- authenticate ----

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticateParams {
    pub method_id: String,
}

// ---- session/new ----

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewParams {
    pub cwd: String,
    pub mcp_servers: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewResult {
    pub session_id: String,
}

// ---- session/prompt ----

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptParams {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

/// One prompt content block. We only send text.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    Text { text: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptResult {
    #[serde(default)]
    pub stop_reason: String,
}

// ---- session/cancel ----

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCancelParams {
    pub session_id: String,
}

// ---- reverse request auto-response ----

/// What to reply to a server→client request so an unattended review never stalls.
pub enum ApprovalReply {
    /// `session/request_permission` → selected `allow-once` (Cursor docs).
    AllowOnce,
    /// `cursor/ask_question` → skipped.
    Skipped,
    /// `cursor/create_plan` → rejected.
    Rejected,
    /// No auto-answer known — reply with a JSON-RPC error.
    Unhandled,
}

impl ApprovalReply {
    pub fn result(&self) -> Option<Value> {
        match self {
            Self::AllowOnce => Some(serde_json::json!({
                "outcome": { "outcome": "selected", "optionId": "allow-once" }
            })),
            Self::Skipped => Some(serde_json::json!({
                "outcome": { "outcome": "skipped" }
            })),
            Self::Rejected => Some(serde_json::json!({
                "outcome": { "outcome": "rejected" }
            })),
            Self::Unhandled => None,
        }
    }
}

/// Map a server→client request method to its auto-response.
/// **Medium** carrier: wrong tokens stall unattended reviews — locked by unit tests.
pub fn auto_response(method: &str) -> ApprovalReply {
    match method {
        server_methods::SESSION_REQUEST_PERMISSION => ApprovalReply::AllowOnce,
        server_methods::CURSOR_ASK_QUESTION => ApprovalReply::Skipped,
        server_methods::CURSOR_CREATE_PLAN => ApprovalReply::Rejected,
        _ => ApprovalReply::Unhandled,
    }
}

// ---- session/update streaming ----

/// Streaming server→client notification (ACP subset + catch-all).
///
/// [`Self::from_raw`] is **total** — unrecognized / bad params → [`Self::Other`].
#[derive(Debug, Clone)]
pub enum ServerNotification {
    /// `session/update` with `agent_message_chunk` → assistant text.
    AgentMessageChunk { session_id: String, text: String },
    /// Synthetic: reader exit (EOF / IO error / oversized frame).
    ConnectionClosed,
    /// Any other notification — method + raw params preserved.
    Other { method: String, params: Value },
}

impl ServerNotification {
    /// Total classifier — never panics, never errors (degrades to [`Self::Other`]).
    pub fn from_raw(method: String, params: Value) -> Self {
        if method != server_methods::SESSION_UPDATE {
            return Self::Other { method, params };
        }
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let update = match params.get("update") {
            Some(u) => u,
            None => return Self::Other { method, params },
        };
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or("");
        if kind == "agent_message_chunk" {
            let text = update
                .pointer("/content/text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if text.is_empty() && session_id.is_empty() {
                return Self::Other { method, params };
            }
            return Self::AgentMessageChunk { session_id, text };
        }
        Self::Other { method, params }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_params_serialize_camel_case() {
        let v = serde_json::to_value(InitializeParams {
            protocol_version: 1,
            client_capabilities: ClientCapabilities {
                fs: FsCapabilities {
                    read_text_file: false,
                    write_text_file: false,
                },
                terminal: false,
            },
            client_info: ClientInfo {
                name: "prmonitor".to_string(),
                title: Some("PR Monitor".to_string()),
                version: "0.1.0".to_string(),
            },
        })
        .expect("InitializeParams serializes");

        assert_eq!(v["protocolVersion"], 1);
        assert!(v.get("protocol_version").is_none());
        assert_eq!(v["clientInfo"]["name"], "prmonitor");
        assert_eq!(v["clientCapabilities"]["fs"]["readTextFile"], false);
        assert_eq!(v["clientCapabilities"]["terminal"], false);
    }

    #[test]
    fn authenticate_params_serialize_method_id() {
        let v = serde_json::to_value(AuthenticateParams {
            method_id: AUTH_METHOD_CURSOR_LOGIN.to_string(),
        })
        .expect("AuthenticateParams serializes");
        assert_eq!(v["methodId"], "cursor_login");
        assert!(v.get("method_id").is_none());
    }

    #[test]
    fn session_new_params_serialize_cwd_and_empty_mcp() {
        let v = serde_json::to_value(SessionNewParams {
            cwd: "/repo".to_string(),
            mcp_servers: vec![],
        })
        .expect("SessionNewParams serializes");
        assert_eq!(v["cwd"], "/repo");
        assert_eq!(v["mcpServers"], serde_json::json!([]));
        assert!(v.get("mcp_servers").is_none());
    }

    #[test]
    fn session_new_result_extracts_session_id() {
        let r: SessionNewResult = serde_json::from_value(serde_json::json!({
            "sessionId": "sess_abc",
            "unknownFutureField": true
        }))
        .expect("SessionNewResult parses");
        assert_eq!(r.session_id, "sess_abc");
    }

    #[test]
    fn session_prompt_params_serialize_text_block() {
        let v = serde_json::to_value(SessionPromptParams {
            session_id: "sess_1".to_string(),
            prompt: vec![ContentBlock::Text {
                text: "/pr-review 42".to_string(),
            }],
        })
        .expect("SessionPromptParams serializes");
        assert_eq!(v["sessionId"], "sess_1");
        assert_eq!(v["prompt"][0]["type"], "text");
        assert_eq!(v["prompt"][0]["text"], "/pr-review 42");
        assert!(v.get("session_id").is_none());
    }

    #[test]
    fn session_prompt_result_parses_stop_reason() {
        let r: SessionPromptResult = serde_json::from_value(serde_json::json!({
            "stopReason": "end_turn"
        }))
        .expect("SessionPromptResult parses");
        assert_eq!(r.stop_reason, "end_turn");
    }

    #[test]
    fn from_raw_agent_message_chunk_maps_text() {
        let n = ServerNotification::from_raw(
            server_methods::SESSION_UPDATE.to_string(),
            serde_json::json!({
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "hello" }
                }
            }),
        );
        match n {
            ServerNotification::AgentMessageChunk { session_id, text } => {
                assert_eq!(session_id, "s1");
                assert_eq!(text, "hello");
            }
            other => panic!("expected AgentMessageChunk, got {other:?}"),
        }
    }

    #[test]
    fn from_raw_unknown_method_is_other() {
        let n = ServerNotification::from_raw(
            "cursor/update_todos".to_string(),
            serde_json::json!({ "x": 1 }),
        );
        assert!(matches!(n, ServerNotification::Other { .. }));
    }

    #[test]
    fn auto_response_locks_permission_and_cursor_extensions() {
        // Medium golden: unattended reviews depend on these exact outcome shapes.
        assert_eq!(
            auto_response(server_methods::SESSION_REQUEST_PERMISSION).result(),
            Some(serde_json::json!({
                "outcome": { "outcome": "selected", "optionId": "allow-once" }
            }))
        );
        assert_eq!(
            auto_response(server_methods::CURSOR_ASK_QUESTION).result(),
            Some(serde_json::json!({
                "outcome": { "outcome": "skipped" }
            }))
        );
        assert_eq!(
            auto_response(server_methods::CURSOR_CREATE_PLAN).result(),
            Some(serde_json::json!({
                "outcome": { "outcome": "rejected" }
            }))
        );
        assert!(auto_response("mcpServer/elicitation/request")
            .result()
            .is_none());
    }
}
