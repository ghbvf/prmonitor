//! Newline-delimited JSON (NDJSON) framing for the app-server stream — one JSON
//! value per line, with no `jsonrpc` field (the app-server omits it, MCP-style).
//!
//! Pure + stateless: encode an outbound frame to a line, and classify one
//! already-read inbound line into a response (carries `id`) vs a notification
//! (carries `method`, no `id`). Line *splitting* is delegated to the reader
//! task's `AsyncBufReadExt::read_line` (which reassembles partial lines), so
//! this module stays free of buffering state and trivially unit-testable.

use serde::Serialize;
use serde_json::Value;

use super::rpc::RpcError;
use crate::error::{AppError, AppResult};

/// A parsed inbound frame, classified by the presence of `id`. The server never
/// sends us requests, so it is always one of these two.
#[derive(Debug)]
pub(crate) enum Inbound {
    /// `{"id": N, "result": …}` or `{"id": N, "error": {…}}`.
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
/// `jsonrpc` is intentionally omitted — the app-server does not expect it.
pub(crate) fn encode_request(id: i64, method: &str, params: &Value) -> AppResult<String> {
    to_line(&OutboundRequest { id, method, params })
}

/// Encode an outbound notification frame (no `id`). `params` is omitted when null
/// (e.g. the `initialized` handshake notification sends no params).
pub(crate) fn encode_notification(method: &str, params: &Value) -> AppResult<String> {
    to_line(&OutboundNotification { method, params })
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
pub(crate) fn decode_line(line: &str) -> AppResult<Option<Inbound>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(trimmed)
        .map_err(|e| AppError::new(format!("解析 app-server 帧失败: {e}")))?;

    if let Some(id) = v.get("id").and_then(Value::as_i64) {
        let payload = if let Some(err) = v.get("error") {
            ResponsePayload::Err(
                serde_json::from_value(err.clone())
                    .map_err(|e| AppError::new(format!("解析 app-server 错误体失败: {e}")))?,
            )
        } else {
            ResponsePayload::Ok(v.get("result").cloned().unwrap_or(Value::Null))
        };
        Ok(Some(Inbound::Response { id, payload }))
    } else if let Some(method) = v.get("method").and_then(Value::as_str) {
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        Ok(Some(Inbound::Notification {
            method: method.to_string(),
            params,
        }))
    } else {
        Err(AppError::new(
            "app-server 帧既无 id 也无 method".to_string(),
        ))
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
