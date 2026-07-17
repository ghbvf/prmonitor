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

/// `session/request_permission` option ids (ACP Permission Options).
pub mod option_ids {
    pub const ALLOW_ALWAYS: &str = "allow-always";
    pub const ALLOW_ONCE: &str = "allow-once";
    pub const REJECT_ONCE: &str = "reject-once";
}

/// ACP / Cursor `toolCall.kind` strings for tests / diagnostics.
///
/// Official ACP kinds: `read` / `edit` / `delete` / `move` / `search` / `execute` /
/// `think` / `fetch` / `switch_mode` / `other`. We also accept `list` / `write` as
/// common variants seen in the wild. Production `permission_reply` ignores kind.
pub mod tool_kinds {
    pub const READ: &str = "read";
    pub const SEARCH: &str = "search";
    pub const LIST: &str = "list";
    pub const THINK: &str = "think";
    pub const FETCH: &str = "fetch";
    pub const SWITCH_MODE: &str = "switch_mode";
    pub const EXECUTE: &str = "execute";
    pub const EDIT: &str = "edit";
    pub const WRITE: &str = "write";
    pub const DELETE: &str = "delete";
    pub const MOVE: &str = "move";
    pub const OTHER: &str = "other";
}

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
    /// `session/request_permission` → selected `allow-always`.
    AllowAlways,
    /// `session/request_permission` → selected `allow-once`.
    AllowOnce,
    /// `session/request_permission` → selected `reject-once` (no allow option offered).
    RejectOnce,
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
            Self::AllowAlways => Some(serde_json::json!({
                "outcome": { "outcome": "selected", "optionId": option_ids::ALLOW_ALWAYS }
            })),
            Self::AllowOnce => Some(serde_json::json!({
                "outcome": { "outcome": "selected", "optionId": option_ids::ALLOW_ONCE }
            })),
            Self::RejectOnce => Some(serde_json::json!({
                "outcome": { "outcome": "selected", "optionId": option_ids::REJECT_ONCE }
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

/// Map a server→client request method (+ params) to its auto-response.
/// **Medium** carrier: wrong tokens stall unattended reviews — locked by unit tests.
///
/// Unrestricted (Claude/Codex-parity): ignore `toolCall.kind`; prefer `allow-always`,
/// else `allow-once`, else `reject-once`, else Unhandled.
pub fn auto_response(method: &str, params: &Value) -> ApprovalReply {
    match method {
        server_methods::SESSION_REQUEST_PERMISSION => permission_reply(params),
        server_methods::CURSOR_ASK_QUESTION => ApprovalReply::Skipped,
        server_methods::CURSOR_CREATE_PLAN => ApprovalReply::Rejected,
        _ => ApprovalReply::Unhandled,
    }
}

/// Option priority for `session/request_permission` (kind ignored).
/// **Medium**: golden in `auto_response_locks_permission_and_cursor_extensions`.
fn permission_reply(params: &Value) -> ApprovalReply {
    let options = params.get("options").and_then(Value::as_array);
    let has_option = |option_id: &str| {
        options
            .map(|opts| {
                opts.iter()
                    .any(|o| o.get("optionId").and_then(Value::as_str) == Some(option_id))
            })
            .unwrap_or(false)
    };

    if has_option(option_ids::ALLOW_ALWAYS) {
        ApprovalReply::AllowAlways
    } else if has_option(option_ids::ALLOW_ONCE) {
        ApprovalReply::AllowOnce
    } else if has_option(option_ids::REJECT_ONCE) {
        ApprovalReply::RejectOnce
    } else {
        // No usable option advertised — fail closed with a JSON-RPC error.
        ApprovalReply::Unhandled
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
    fn initialize_result_deserializes_camel_case_and_ignores_unknown() {
        let r: InitializeResult = serde_json::from_value(serde_json::json!({
            "protocolVersion": 1,
            "agentCapabilities": { "loadSession": true },
            "authMethods": [{ "id": "cursor_login" }],
            "unknownFutureField": { "x": 1 }
        }))
        .expect("InitializeResult parses");
        assert_eq!(r.protocol_version, 1);
        assert_eq!(r.auth_methods.len(), 1);
        assert_eq!(r.agent_capabilities["loadSession"], true);
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
    fn session_cancel_params_serialize_camel_case() {
        let v = serde_json::to_value(SessionCancelParams {
            session_id: "sess_cancel".to_string(),
        })
        .expect("SessionCancelParams serializes");
        assert_eq!(v["sessionId"], "sess_cancel");
        assert!(v.get("session_id").is_none());
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
    fn from_raw_empty_agent_message_chunk_is_other() {
        // Empty sessionId + empty text degrades to Other (no usable delta).
        let n = ServerNotification::from_raw(
            server_methods::SESSION_UPDATE.to_string(),
            serde_json::json!({
                "sessionId": "",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "" }
                }
            }),
        );
        assert!(matches!(n, ServerNotification::Other { .. }));
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
        // Medium golden: unrestricted option priority (kind ignored).
        // allow-always > allow-once > reject-once > Unhandled.
        let once_and_reject = serde_json::json!([
            { "optionId": option_ids::ALLOW_ONCE, "name": "Allow once", "kind": "allow_once" },
            { "optionId": option_ids::REJECT_ONCE, "name": "Reject", "kind": "reject_once" }
        ]);
        let all_three = serde_json::json!([
            { "optionId": option_ids::ALLOW_ALWAYS, "name": "Allow always", "kind": "allow_always" },
            { "optionId": option_ids::ALLOW_ONCE, "name": "Allow once", "kind": "allow_once" },
            { "optionId": option_ids::REJECT_ONCE, "name": "Reject", "kind": "reject_once" }
        ]);

        let read_once = serde_json::json!({
            "sessionId": "s1",
            "toolCall": { "toolCallId": "c1", "kind": tool_kinds::READ },
            "options": once_and_reject.clone()
        });
        assert_eq!(
            auto_response(server_methods::SESSION_REQUEST_PERMISSION, &read_once).result(),
            Some(serde_json::json!({
                "outcome": { "outcome": "selected", "optionId": option_ids::ALLOW_ONCE }
            }))
        );

        let execute_always = serde_json::json!({
            "sessionId": "s1",
            "toolCall": { "toolCallId": "c1c", "kind": tool_kinds::EXECUTE },
            "options": all_three.clone()
        });
        assert_eq!(
            auto_response(server_methods::SESSION_REQUEST_PERMISSION, &execute_always).result(),
            Some(serde_json::json!({
                "outcome": { "outcome": "selected", "optionId": option_ids::ALLOW_ALWAYS }
            }))
        );

        let execute_once = serde_json::json!({
            "sessionId": "s1",
            "toolCall": { "toolCallId": "c1d", "kind": tool_kinds::EXECUTE },
            "options": once_and_reject.clone()
        });
        assert_eq!(
            auto_response(server_methods::SESSION_REQUEST_PERMISSION, &execute_once).result(),
            Some(serde_json::json!({
                "outcome": { "outcome": "selected", "optionId": option_ids::ALLOW_ONCE }
            }))
        );

        let edit_once = serde_json::json!({
            "sessionId": "s1",
            "toolCall": { "toolCallId": "c1e", "kind": tool_kinds::EDIT },
            "options": once_and_reject.clone()
        });
        assert_eq!(
            auto_response(server_methods::SESSION_REQUEST_PERMISSION, &edit_once).result(),
            Some(serde_json::json!({
                "outcome": { "outcome": "selected", "optionId": option_ids::ALLOW_ONCE }
            }))
        );

        let switch_mode = serde_json::json!({
            "sessionId": "s1",
            "toolCall": { "toolCallId": "c2", "kind": tool_kinds::SWITCH_MODE },
            "options": once_and_reject
        });
        assert_eq!(
            auto_response(server_methods::SESSION_REQUEST_PERMISSION, &switch_mode).result(),
            Some(serde_json::json!({
                "outcome": { "outcome": "selected", "optionId": option_ids::ALLOW_ONCE }
            }))
        );

        let only_reject = serde_json::json!({
            "sessionId": "s1",
            "toolCall": { "toolCallId": "c3", "kind": tool_kinds::READ },
            "options": [
                { "optionId": option_ids::REJECT_ONCE, "name": "Reject", "kind": "reject_once" }
            ]
        });
        assert_eq!(
            auto_response(server_methods::SESSION_REQUEST_PERMISSION, &only_reject).result(),
            Some(serde_json::json!({
                "outcome": { "outcome": "selected", "optionId": option_ids::REJECT_ONCE }
            }))
        );

        assert!(auto_response(
            server_methods::SESSION_REQUEST_PERMISSION,
            &serde_json::json!({})
        )
        .result()
        .is_none());

        assert_eq!(
            auto_response(server_methods::CURSOR_ASK_QUESTION, &Value::Null).result(),
            Some(serde_json::json!({
                "outcome": { "outcome": "skipped" }
            }))
        );
        assert_eq!(
            auto_response(server_methods::CURSOR_CREATE_PLAN, &Value::Null).result(),
            Some(serde_json::json!({
                "outcome": { "outcome": "rejected" }
            }))
        );
        assert!(auto_response("mcpServer/elicitation/request", &Value::Null)
            .result()
            .is_none());
    }
}
