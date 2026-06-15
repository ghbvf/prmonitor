//! Newline-delimited JSON (NDJSON) framing for the app-server stream — one JSON
//! value per line, with no `jsonrpc` field (the app-server omits it, MCP-style).
//!
//! Pure + stateless: encode an outbound frame to a line, and classify one
//! already-read inbound line into a response, a server→client *request*, or a
//! notification. Line *splitting* is delegated to the reader task's
//! `AsyncBufReadExt::read_line` (which reassembles partial lines), so this module
//! stays free of buffering state and trivially unit-testable.
//!
//! Classification rule (order matters — a server request carries BOTH `id` and
//! `method`): `method` present + `id` present → [`Inbound::ServerRequest`];
//! `method` present, no `id` → [`Inbound::Notification`]; `id` present, no
//! `method` → [`Inbound::Response`]. The server DOES send us requests (reverse
//! approval prompts); a naive "`id` ⇒ response" check would misroute them onto a
//! non-existent pending entry and leave codex blocked on a reply that never
//! comes — the reader auto-answers them via [`encode_response`] instead.

use serde::Serialize;
use serde_json::Value;

use super::rpc::RpcError;
use crate::error::{AppError, AppResult};

/// A parsed inbound frame. The server sends responses to our requests,
/// notifications (no `id`), and its own requests (reverse approval prompts —
/// `id` + `method`) that we must answer.
#[derive(Debug)]
pub(crate) enum Inbound {
    /// `{"id": N, "result": …}` or `{"id": N, "error": {…}}` — no `method`.
    Response { id: i64, payload: ResponsePayload },
    /// `{"id": N, "method": "…", "params": …}` — a server→client request we must
    /// answer with a matching `{"id": N, "result": …}` (or error).
    ServerRequest {
        id: i64,
        method: String,
        params: Value,
    },
    /// `{"method": "…", "params": …}` — no `id`.
    Notification { method: String, params: Value },
}

#[derive(Debug)]
pub(crate) enum ResponsePayload {
    Ok(Value),
    Err(RpcError),
}

/// Encode an outbound request frame to a single NDJSON line (trailing `\n`).
/// `jsonrpc` is intentionally omitted — the app-server does not expect it.
pub(crate) fn encode_request(id: i64, method: &str, params: &Value) -> AppResult<String> {
    to_line(&OutboundRequest { id, method, params })
}

/// Encode an outbound notification frame (no `id`). `params` is omitted when null
/// (e.g. the `initialized` handshake notification sends no params).
pub(crate) fn encode_notification(method: &str, params: &Value) -> AppResult<String> {
    to_line(&OutboundNotification { method, params })
}

/// Encode a response to a server→client request (`{"id": N, "result": …}`). Used
/// by the reader to auto-answer reverse approval prompts so codex never blocks.
pub(crate) fn encode_response(id: i64, result: &Value) -> AppResult<String> {
    to_line(&OutboundResponse { id, result })
}

/// Encode an error response to a server→client request
/// (`{"id": N, "error": {code, message}}`) — sent when we have no auto-answer for
/// a server request, so codex gets a reply rather than hanging.
pub(crate) fn encode_error_response(id: i64, code: i64, message: &str) -> AppResult<String> {
    to_line(&OutboundErrorResponse {
        id,
        error: OutboundError { code, message },
    })
}

fn to_line<T: Serialize>(frame: &T) -> AppResult<String> {
    let mut s = serde_json::to_string(frame)
        .map_err(|e| AppError::new(format!("编码 app-server 请求失败: {e}")))?;
    s.push('\n');
    Ok(s)
}

/// Classify one raw line (trailing newline allowed) into an [`Inbound`].
/// Whitespace-only lines return `Ok(None)` (skip). Malformed JSON or a frame
/// with neither `id` nor `method` returns `Err` — the reader logs and skips it,
/// never tearing down the connection over one bad line.
///
/// `method` is inspected BEFORE `id`: a server→client request carries both, so
/// checking `id` first would misclassify it as a [`Inbound::Response`].
pub(crate) fn decode_line(line: &str) -> AppResult<Option<Inbound>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(trimmed)
        .map_err(|e| AppError::new(format!("解析 app-server 帧失败: {e}")))?;

    let id = v.get("id").and_then(Value::as_i64);
    let method = v.get("method").and_then(Value::as_str);

    match (id, method) {
        // Server→client request: both `id` and `method`. Must precede the
        // response check (a response has `id` but no `method`).
        (Some(id), Some(method)) => Ok(Some(Inbound::ServerRequest {
            id,
            method: method.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        })),
        // Response to one of our requests: `id`, no `method`.
        (Some(id), None) => {
            let payload = if let Some(err) = v.get("error") {
                ResponsePayload::Err(
                    serde_json::from_value(err.clone())
                        .map_err(|e| AppError::new(format!("解析 app-server 错误体失败: {e}")))?,
                )
            } else {
                ResponsePayload::Ok(v.get("result").cloned().unwrap_or(Value::Null))
            };
            Ok(Some(Inbound::Response { id, payload }))
        }
        // Notification: `method`, no `id`.
        (None, Some(method)) => Ok(Some(Inbound::Notification {
            method: method.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        })),
        (None, None) => Err(AppError::new(
            "app-server 帧既无 id 也无 method".to_string(),
        )),
    }
}

#[derive(Serialize)]
struct OutboundRequest<'a> {
    id: i64,
    method: &'a str,
    params: &'a Value,
}

#[derive(Serialize)]
struct OutboundNotification<'a> {
    method: &'a str,
    #[serde(skip_serializing_if = "Value::is_null")]
    params: &'a Value,
}

#[derive(Serialize)]
struct OutboundResponse<'a> {
    id: i64,
    result: &'a Value,
}

#[derive(Serialize)]
struct OutboundErrorResponse<'a> {
    id: i64,
    error: OutboundError<'a>,
}

#[derive(Serialize)]
struct OutboundError<'a> {
    code: i64,
    message: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_request_omits_jsonrpc_and_appends_newline() {
        let line = encode_request(7, "initialize", &serde_json::json!({"a": 1})).unwrap();
        assert!(line.ends_with('\n'));
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "initialize");
        assert_eq!(v["params"]["a"], 1);
        // jsonrpc omitted on the wire.
        assert!(v.get("jsonrpc").is_none());
    }

    #[test]
    fn encode_notification_omits_null_params() {
        let line = encode_notification("initialized", &Value::Null).unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["method"], "initialized");
        assert!(v.get("params").is_none());

        let line = encode_notification("x", &serde_json::json!({"k": 1})).unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["params"]["k"], 1);
    }

    #[test]
    fn decode_classifies_response_ok() {
        match decode_line(r#"{"id":3,"result":{"ok":true}}"#)
            .unwrap()
            .unwrap()
        {
            Inbound::Response {
                id,
                payload: ResponsePayload::Ok(v),
            } => {
                assert_eq!(id, 3);
                assert_eq!(v["ok"], true);
            }
            other => panic!("expected ok response, got {other:?}"),
        }
    }

    #[test]
    fn decode_classifies_error_response() {
        match decode_line(r#"{"id":1,"error":{"code":-32016,"message":"not initialized"}}"#)
            .unwrap()
            .unwrap()
        {
            Inbound::Response {
                payload: ResponsePayload::Err(e),
                ..
            } => {
                assert_eq!(e.code, -32016);
                assert_eq!(e.message, "not initialized");
            }
            other => panic!("expected error response, got {other:?}"),
        }
    }

    #[test]
    fn decode_classifies_server_request_not_response() {
        // A frame with BOTH `id` and `method` is a server→client request, not a
        // response — the regression guard for the reverse-approval misroute.
        match decode_line(r#"{"id":5,"method":"execCommandApproval","params":{"command":"ls"}}"#)
            .unwrap()
            .unwrap()
        {
            Inbound::ServerRequest { id, method, params } => {
                assert_eq!(id, 5);
                assert_eq!(method, "execCommandApproval");
                assert_eq!(params["command"], "ls");
            }
            other => panic!("expected server request, got {other:?}"),
        }
    }

    #[test]
    fn encode_response_and_error_shapes() {
        let line = encode_response(5, &serde_json::json!({"decision": "approved"})).unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 5);
        assert_eq!(v["result"]["decision"], "approved");
        assert!(v.get("jsonrpc").is_none());

        let line = encode_error_response(6, -32601, "method not found").unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 6);
        assert_eq!(v["error"]["code"], -32601);
        assert_eq!(v["error"]["message"], "method not found");
        assert!(v.get("result").is_none());
    }

    #[test]
    fn decode_classifies_notification() {
        match decode_line(r#"{"method":"item/agentMessage/delta","params":{"delta":"x"}}"#)
            .unwrap()
            .unwrap()
        {
            Inbound::Notification { method, params } => {
                assert_eq!(method, "item/agentMessage/delta");
                assert_eq!(params["delta"], "x");
            }
            other => panic!("expected notification, got {other:?}"),
        }
    }

    #[test]
    fn decode_result_absent_defaults_to_null() {
        match decode_line(r#"{"id":9}"#).unwrap().unwrap() {
            Inbound::Response {
                payload: ResponsePayload::Ok(v),
                ..
            } => assert!(v.is_null()),
            other => panic!("expected ok response, got {other:?}"),
        }
    }

    #[test]
    fn decode_blank_line_is_none() {
        assert!(decode_line("   \n").unwrap().is_none());
    }

    #[test]
    fn decode_malformed_is_err_not_panic() {
        assert!(decode_line("{not json").is_err());
    }

    #[test]
    fn decode_neither_id_nor_method_is_err() {
        assert!(decode_line(r#"{"foo":1}"#).is_err());
    }
}
