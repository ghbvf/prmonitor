//! Newline-delimited JSON (NDJSON) framing for the iTerm daemon stream — one JSON
//! value per line, with no `jsonrpc` field (the daemon omits it, MCP-style).
//!
//! A minimal copy of the codex `codec` (the slice boundary forbids `crate::review::`, so
//! the transport is an independent copy — same precedent as `claude` having its own
//! `process.rs`). Pure + stateless: encode an outbound request to a line, and classify
//! one already-read inbound line into a response or a notification. Line *splitting* is
//! the reader task's job ([`AsyncBufReadExt::read_until`]), so this module stays
//! buffering-free and trivially unit-testable.
//!
//! Classification rule: UNLIKE codex, the daemon NEVER sends server→client requests, so
//! `method` present → [`Inbound::Notification`]; otherwise `id` present →
//! [`Inbound::Response`]. (A frame carrying BOTH is impossible from our own daemon; if it
//! ever appeared it would be treated as a notification — harmless, since no pending entry
//! keys on it.)

use serde::Serialize;
use serde_json::Value;

use super::rpc::RpcError;
use crate::error::{AppError, AppResult};

/// A parsed inbound frame: a response to one of our requests, or a daemon notification.
#[derive(Debug)]
pub(crate) enum Inbound {
    /// `{"id": N, "result": …}` or `{"id": N, "error": {…}}` — no `method`.
    Response { id: i64, payload: ResponsePayload },
    /// `{"method": "…", "params": …}` — no `id`.
    Notification { method: String, params: Value },
}

#[derive(Debug)]
pub(crate) enum ResponsePayload {
    Ok(Value),
    Err(RpcError),
}

/// Encode an outbound request frame to a single NDJSON line (trailing `\n`).
/// `jsonrpc` is intentionally omitted — the daemon does not expect it.
pub(crate) fn encode_request(id: i64, method: &str, params: &Value) -> AppResult<String> {
    let mut s = serde_json::to_string(&OutboundRequest { id, method, params })
        .map_err(|e| AppError::new(format!("编码 iTerm daemon 请求失败: {e}")))?;
    s.push('\n');
    Ok(s)
}

/// Classify one raw line (trailing newline allowed) into an [`Inbound`].
/// Whitespace-only lines return `Ok(None)` (skip). Malformed JSON or a frame with neither
/// `id` nor `method` returns `Err` — the reader logs and skips it, never tearing down the
/// connection over one bad line.
pub(crate) fn decode_line(line: &str) -> AppResult<Option<Inbound>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(trimmed)
        .map_err(|e| AppError::new(format!("解析 iTerm daemon 帧失败: {e}")))?;

    let id = v.get("id").and_then(Value::as_i64);
    let method = v.get("method").and_then(Value::as_str);

    // `method` first: the daemon sends no server→client requests, so a `method` frame is
    // always a notification (any stray `id` alongside it is ignored).
    if let Some(method) = method {
        return Ok(Some(Inbound::Notification {
            method: method.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        }));
    }
    if let Some(id) = id {
        let payload = if let Some(err) = v.get("error") {
            ResponsePayload::Err(
                serde_json::from_value(err.clone())
                    .map_err(|e| AppError::new(format!("解析 iTerm daemon 错误体失败: {e}")))?,
            )
        } else {
            ResponsePayload::Ok(v.get("result").cloned().unwrap_or(Value::Null))
        };
        return Ok(Some(Inbound::Response { id, payload }));
    }
    Err(AppError::new(
        "iTerm daemon 帧既无 id 也无 method".to_string(),
    ))
}

#[derive(Serialize)]
struct OutboundRequest<'a> {
    id: i64,
    method: &'a str,
    params: &'a Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_request_omits_jsonrpc_and_appends_newline() {
        let line = encode_request(7, "subscribe", &serde_json::json!({"sessionId": "p0"})).unwrap();
        assert!(line.ends_with('\n'));
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "subscribe");
        assert_eq!(v["params"]["sessionId"], "p0");
        assert!(v.get("jsonrpc").is_none());
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
        match decode_line(r#"{"id":1,"error":{"code":-32000,"message":"iTerm 未授权"}}"#)
            .unwrap()
            .unwrap()
        {
            Inbound::Response {
                payload: ResponsePayload::Err(e),
                ..
            } => {
                assert_eq!(e.code, -32000);
                assert_eq!(e.message, "iTerm 未授权");
            }
            other => panic!("expected error response, got {other:?}"),
        }
    }

    #[test]
    fn decode_classifies_notification() {
        match decode_line(r#"{"method":"screenUpdate","params":{"sessionId":"p0"}}"#)
            .unwrap()
            .unwrap()
        {
            Inbound::Notification { method, params } => {
                assert_eq!(method, "screenUpdate");
                assert_eq!(params["sessionId"], "p0");
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
