//! Typed subset of the codex app-server protocol we use: the `initialize` /
//! `initialized` handshake (v1), `thread/start` (v2), and the streaming
//! `ServerNotification`s. Shapes verified against `codex app-server
//! generate-json-schema` (codex 0.139.0; `TurnStartParams.model` re-verified against
//! 0.141.0 — v2 documents it as "Override the model for this turn and subsequent
//! turns"); regenerate from the installed binary when bumping codex.
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
    /// v2 request starting a turn within a thread (expects [`super::TurnStartResult`]).
    pub const TURN_START: &str = "turn/start";
    /// v2 request interrupting a running turn (empty result).
    pub const TURN_INTERRUPT: &str = "turn/interrupt";
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

/// `thread/start` request params. All fields are optional in the protocol; we
/// only set `cwd` (the repo the review runs against). The rich turn/sandbox params
/// live on [`TurnStartParams`], set when the review turn starts.
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

// ---- turn/start + turn/interrupt (v2) ----

/// `turn/start` request params — launches the pr-review skill on a thread.
/// Mirrors the proven `router.py` shape (verified against codex 0.139.0): the
/// top level is camelCase, but a [`UserInput::Text`]'s `text_elements` is
/// snake_case on the wire (we omit it — it defaults to `[]`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub thread_id: String,
    pub input: Vec<UserInput>,
    /// `"never"` for unattended reviews (no human approval prompts).
    pub approval_policy: String,
    pub sandbox_policy: SandboxPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Per-turn model override ("Override the model for this turn and subsequent
    /// turns" in the codex app-server v2 schema). The app-server is a single shared
    /// process, so model selection must ride the per-turn RPC, not a spawn flag.
    /// `None` (config left blank) omits the key → codex uses its configured default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// One input item for `turn/start`. The pr-review turn sends a [`Self::Skill`]
/// (attaches the local skill) followed by a [`Self::Text`] (the instruction).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserInput {
    /// `{"type":"skill","name":…,"path":…}` — attach a local project skill.
    Skill { name: String, path: String },
    /// `{"type":"text","text":…}` — a plain instruction. `text_elements` is
    /// omitted (defaults to `[]` server-side).
    Text { text: String },
}

/// `turn/start` sandbox policy. `workspaceWrite` + network so the pr-review skill
/// can run `git`/`gh` and write within the repo. Serializes camelCase
/// (`networkAccess` / `writableRoots`); the `type` tag stays `type`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxPolicy {
    #[serde(rename = "type")]
    pub kind: String,
    pub network_access: bool,
    pub writable_roots: Vec<String>,
}

/// `turn/start` result — we extract only `turn.id`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResult {
    pub turn: TurnRef,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnRef {
    pub id: String,
}

/// `turn/interrupt` request params.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    pub thread_id: String,
    pub turn_id: String,
}

// ---- reverse approval auto-response ----

/// What to reply to a server→client request (a reverse approval prompt).
///
/// With `approvalPolicy: "never"` codex should not ask, but the protocol still
/// permits it; auto-answering keeps a review from stalling on an unanswered
/// prompt. The approve token differs by method family (verified against codex
/// 0.139.0): the v1 exec/applyPatch prompts take `ReviewDecision` (`"approved"`),
/// the v2 `item/*requestApproval` prompts take an accept decision (`"accept"`).
pub enum ApprovalReply {
    /// Reply `{"decision": "approved"}` (v1 `execCommandApproval`/`applyPatchApproval`).
    Approved,
    /// Reply `{"decision": "accept"}` (v2 `item/commandExecution|fileChange/requestApproval`).
    Accept,
    /// No auto-answer known — reply with a JSON-RPC error so codex does not hang.
    Unhandled,
}

impl ApprovalReply {
    /// The `result` body for an approving reply, or `None` for [`Self::Unhandled`].
    pub fn result(&self) -> Option<Value> {
        match self {
            Self::Approved => Some(serde_json::json!({ "decision": "approved" })),
            Self::Accept => Some(serde_json::json!({ "decision": "accept" })),
            Self::Unhandled => None,
        }
    }
}

/// Map a server→client request method to its auto-response. Centralizes the
/// approval policy (a wrong token would silently stall reviews — locked by a unit
/// test below, the **Medium** carrier per `ai-robust.md`).
pub fn auto_response(method: &str) -> ApprovalReply {
    match method {
        "execCommandApproval" | "applyPatchApproval" => ApprovalReply::Approved,
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            ApprovalReply::Accept
        }
        _ => ApprovalReply::Unhandled,
    }
}

// ---- streaming server -> client notifications (v2 subset) ----

/// Server→client notification method strings (single source for
/// [`ServerNotification::from_raw`] + tests; named distinctly from the
/// client→server [`rpc_methods`]).
pub mod notif_methods {
    pub const AGENT_MESSAGE_DELTA: &str = "item/agentMessage/delta";
    pub const REASONING_TEXT_DELTA: &str = "item/reasoning/textDelta";
    pub const TURN_COMPLETED: &str = "turn/completed";
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
    /// `item/reasoning/textDelta` — incremental reasoning text (`delta: String`).
    ReasoningTextDelta(ReasoningTextDelta),
    /// `turn/completed` — the turn ended; status is nested in `turn.status`.
    TurnCompleted(TurnCompletedNotification),
    /// `command/exec/outputDelta` / `process/outputDelta` — base64 output chunk
    /// (`deltaBase64`, kept opaque; the review stream does not forward it yet).
    OutputDelta(OutputDelta),
    /// Synthetic, **not** a wire notification: the rpc reader injects this on the
    /// broadcast when its loop exits (codex EOF / IO error / oversized frame), so
    /// every subscribed session pump sees the transport tearing down and ends with
    /// a terminal `Failed` instead of hanging on `recv()` forever (the `RpcClient`
    /// keeps the broadcast `Sender` alive across a dead reader, so `RecvError::Closed`
    /// would otherwise never fire). [`Self::from_raw`] never produces it.
    ConnectionClosed,
    /// Any other (unknown/future) notification — method + raw params preserved.
    Other { method: String, params: Value },
}

impl ServerNotification {
    /// Total classifier — never panics, never errors (degrades to [`Self::Other`]).
    pub fn from_raw(method: String, params: Value) -> Self {
        match method.as_str() {
            notif_methods::AGENT_MESSAGE_DELTA => match serde_json::from_value(params.clone()) {
                Ok(d) => Self::AgentMessageDelta(d),
                Err(_) => Self::Other { method, params },
            },
            notif_methods::REASONING_TEXT_DELTA => match serde_json::from_value(params.clone()) {
                Ok(d) => Self::ReasoningTextDelta(d),
                Err(_) => Self::Other { method, params },
            },
            notif_methods::TURN_COMPLETED => match serde_json::from_value(params.clone()) {
                Ok(d) => Self::TurnCompleted(d),
                Err(_) => Self::Other { method, params },
            },
            notif_methods::COMMAND_EXEC_OUTPUT_DELTA | notif_methods::PROCESS_OUTPUT_DELTA => {
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningTextDelta {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
}

/// `turn/completed` notification. The terminal status lives in `turn.status`
/// (`completed` / `interrupted` / `failed`); other `turn` fields are ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedNotification {
    pub thread_id: String,
    pub turn: TurnStatusRef,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStatusRef {
    pub status: String,
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
    /// Kept opaque — preserved verbatim; not yet decoded to bytes.
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
            notif_methods::AGENT_MESSAGE_DELTA.to_string(),
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
            notif_methods::COMMAND_EXEC_OUTPUT_DELTA.to_string(),
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
            notif_methods::PROCESS_OUTPUT_DELTA.to_string(),
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
            notif_methods::AGENT_MESSAGE_DELTA.to_string(),
            serde_json::json!({}),
        );
        assert!(matches!(n, ServerNotification::Other { .. }));
    }

    #[test]
    fn turn_start_params_serialize_camel_case_with_skill_and_text_input() {
        let v = serde_json::to_value(TurnStartParams {
            thread_id: "th_1".to_string(),
            input: vec![
                UserInput::Skill {
                    name: "pr-review".to_string(),
                    path: "/repo/.codex/skills/pr-review/SKILL.md".to_string(),
                },
                UserInput::Text {
                    text: "do it".to_string(),
                },
            ],
            approval_policy: "never".to_string(),
            sandbox_policy: SandboxPolicy {
                kind: "workspaceWrite".to_string(),
                network_access: true,
                writable_roots: vec!["/repo".to_string()],
            },
            cwd: Some("/repo".to_string()),
            model: None,
        })
        .expect("TurnStartParams serializes");

        assert_eq!(v["threadId"], "th_1");
        assert_eq!(v["approvalPolicy"], "never");
        assert!(v.get("thread_id").is_none()); // snake_case absent.
        assert!(v.get("model").is_none()); // None → key omitted (codex default).

        // Skill input item.
        assert_eq!(v["input"][0]["type"], "skill");
        assert_eq!(v["input"][0]["name"], "pr-review");
        assert!(v["input"][0]["path"].is_string());
        // Text input item — `text_elements` omitted (server defaults to []).
        assert_eq!(v["input"][1]["type"], "text");
        assert_eq!(v["input"][1]["text"], "do it");

        // Sandbox policy: `type` key (not `kind`) + camelCase networkAccess.
        assert_eq!(v["sandboxPolicy"]["type"], "workspaceWrite");
        assert_eq!(v["sandboxPolicy"]["networkAccess"], true);
        assert_eq!(v["sandboxPolicy"]["writableRoots"][0], "/repo");
    }

    #[test]
    fn turn_start_params_serialize_model_override_when_set() {
        let v = serde_json::to_value(TurnStartParams {
            thread_id: "th_1".to_string(),
            input: vec![],
            approval_policy: "never".to_string(),
            sandbox_policy: SandboxPolicy {
                kind: "workspaceWrite".to_string(),
                network_access: true,
                writable_roots: vec![],
            },
            cwd: None,
            model: Some("gpt-5.1-codex".to_string()),
        })
        .expect("TurnStartParams serializes");
        // Some → key present with the configured model (the per-turn override).
        assert_eq!(v["model"], "gpt-5.1-codex");
    }

    #[test]
    fn turn_interrupt_params_serialize_camel_case() {
        let v = serde_json::to_value(TurnInterruptParams {
            thread_id: "th_1".to_string(),
            turn_id: "tn_1".to_string(),
        })
        .expect("TurnInterruptParams serializes");
        assert_eq!(v["threadId"], "th_1");
        assert_eq!(v["turnId"], "tn_1");
        assert!(v.get("thread_id").is_none());
    }

    #[test]
    fn turn_start_result_extracts_turn_id() {
        let r: TurnStartResult = serde_json::from_value(serde_json::json!({
            "turn": { "id": "tn_abc", "status": "inProgress" }
        }))
        .expect("TurnStartResult parses (extra fields ignored)");
        assert_eq!(r.turn.id, "tn_abc");
    }

    #[test]
    fn from_raw_reasoning_delta_is_typed() {
        let n = ServerNotification::from_raw(
            notif_methods::REASONING_TEXT_DELTA.to_string(),
            serde_json::json!({
                "threadId": "t", "turnId": "u", "itemId": "i", "delta": "why", "contentIndex": 0
            }),
        );
        match n {
            ServerNotification::ReasoningTextDelta(d) => {
                assert_eq!(d.delta, "why");
                assert_eq!(d.item_id, "i");
            }
            _ => panic!("expected ReasoningTextDelta"),
        }
    }

    #[test]
    fn from_raw_turn_completed_extracts_nested_status() {
        let n = ServerNotification::from_raw(
            notif_methods::TURN_COMPLETED.to_string(),
            serde_json::json!({
                "threadId": "t",
                "turn": { "id": "u", "status": "interrupted", "items": [] }
            }),
        );
        match n {
            ServerNotification::TurnCompleted(d) => {
                assert_eq!(d.thread_id, "t");
                assert_eq!(d.turn.status, "interrupted");
            }
            _ => panic!("expected TurnCompleted"),
        }
    }

    #[test]
    fn auto_response_maps_methods_to_approve_tokens() {
        // v1 exec/applyPatch → {"decision":"approved"}.
        assert_eq!(
            auto_response("execCommandApproval").result(),
            Some(serde_json::json!({"decision": "approved"}))
        );
        assert_eq!(
            auto_response("applyPatchApproval").result(),
            Some(serde_json::json!({"decision": "approved"}))
        );
        // v2 item/*requestApproval → {"decision":"accept"}.
        assert_eq!(
            auto_response("item/commandExecution/requestApproval").result(),
            Some(serde_json::json!({"decision": "accept"}))
        );
        assert_eq!(
            auto_response("item/fileChange/requestApproval").result(),
            Some(serde_json::json!({"decision": "accept"}))
        );
        // Unknown server request → no auto-answer (reader replies with an error).
        assert!(auto_response("mcpServer/elicitation/request")
            .result()
            .is_none());
    }
}
