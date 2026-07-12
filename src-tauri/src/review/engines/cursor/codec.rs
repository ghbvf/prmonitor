//! Newline-delimited JSON (NDJSON) framing for the Cursor ACP stream — one JSON
//! value per line, **with** `"jsonrpc":"2.0"` on every outbound frame (and required
//! on inbound). Codex's app-server omits `jsonrpc`; Cursor ACP must not.
//!
//! Classification rule (order matters — a server request carries BOTH `id` and
//! `method`): `method` present + `id` present → [`Inbound::ServerRequest`];
//! `method` present, no `id` → [`Inbound::Notification`]; `id` present, no
//! `method` → [`Inbound::Response`].

use serde::Serialize;
use serde_json::Value;

use super::rpc::RpcError;
use crate::error::{AppError, AppResult};

const JSONRPC_VERSION: &str = "2.0";

/// A parsed inbound frame.
#[derive(Debug)]
pub(crate) enum Inbound {
    /// `{"jsonrpc":"2.0","id": N, "result": …}` or `{"id": N, "error": {…}}`.
    Response { id: i64, payload: ResponsePayload },
    /// `{"jsonrpc":"2.0","id": N, "method": "…", "params": …}` — reverse request.
    ServerRequest {
        id: i64,
        method: String,
        params: Value,
    },
    /// `{"jsonrpc":"2.0","method": "…", "params": …}` — no `id`.
    Notification { method: String, params: Value },
}

#[derive(Debug)]
pub(crate) enum ResponsePayload {
    Ok(Value),
    Err(RpcError),
}

/// Encode an outbound request frame to a single NDJSON line (trailing `\n`).
pub(crate) fn encode_request(id: i64, method: &str, params: &Value) -> AppResult<String> {
    to_line(&OutboundRequest {
        jsonrpc: JSONRPC_VERSION,
        id,
        method,
        params,
    })
}

/// Encode an outbound notification frame (no `id`). `params` is omitted when null.
pub(crate) fn encode_notification(method: &str, params: &Value) -> AppResult<String> {
    to_line(&OutboundNotification {
        jsonrpc: JSONRPC_VERSION,
        method,
        params,
    })
}

/// Encode a response to a server→client request.
pub(crate) fn encode_response(id: i64, result: &Value) -> AppResult<String> {
    to_line(&OutboundResponse {
        jsonrpc: JSONRPC_VERSION,
        id,
        result,
    })
}

/// Encode an error response to a server→client request.
pub(crate) fn encode_error_response(id: i64, code: i64, message: &str) -> AppResult<String> {
    to_line(&OutboundErrorResponse {
        jsonrpc: JSONRPC_VERSION,
        id,
        error: OutboundError { code, message },
    })
}

fn to_line<T: Serialize>(frame: &T) -> AppResult<String> {
    let mut s = serde_json::to_string(frame)
        .map_err(|e| AppError::new(format!("编码 cursor ACP 请求失败: {e}")))?;
    s.push('\n');
    Ok(s)
}

/// Classify one raw line into an [`Inbound`]. Blank → `Ok(None)`. Malformed JSON,
/// missing `jsonrpc`, neither `id` nor `method`, or a present-but-unusable `id`
/// (wrong type / non-digit string) → `Err`. JSON-RPC allows `id` as number **or**
/// string; we normalize digit strings to `i64` so they match the pending map.
pub(crate) fn decode_line(line: &str) -> AppResult<Option<Inbound>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(trimmed)
        .map_err(|e| AppError::new(format!("解析 cursor ACP 帧失败: {e}")))?;

    match v.get("jsonrpc").and_then(Value::as_str) {
        Some(JSONRPC_VERSION) => {}
        Some(other) => {
            return Err(AppError::new(format!(
                "cursor ACP 帧 jsonrpc 版本无效: {other}"
            )));
        }
        None => {
            return Err(AppError::new("cursor ACP 帧缺少 jsonrpc 字段".to_string()));
        }
    }

    let method = v.get("method").and_then(Value::as_str);
    let id_field = v.get("id");

    match (id_field, method) {
        (Some(id_val), Some(method)) => {
            let id = parse_rpc_id(id_val)?;
            Ok(Some(Inbound::ServerRequest {
                id,
                method: method.to_string(),
                params: v.get("params").cloned().unwrap_or(Value::Null),
            }))
        }
        (Some(id_val), None) => {
            let id = parse_rpc_id(id_val)?;
            let payload = if let Some(err) = v.get("error") {
                ResponsePayload::Err(
                    serde_json::from_value(err.clone())
                        .map_err(|e| AppError::new(format!("解析 cursor ACP 错误体失败: {e}")))?,
                )
            } else {
                ResponsePayload::Ok(v.get("result").cloned().unwrap_or(Value::Null))
            };
            Ok(Some(Inbound::Response { id, payload }))
        }
        (None, Some(method)) => Ok(Some(Inbound::Notification {
            method: method.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        })),
        (None, None) => Err(AppError::new(
            "cursor ACP 帧既无 id 也无 method".to_string(),
        )),
    }
}

/// Accept JSON-RPC `id` as `i64` or a digit string; reject other shapes fail-closed
/// so a mistyped id + method never silently degrades to a Notification.
fn parse_rpc_id(id: &Value) -> AppResult<i64> {
    match id {
        Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| AppError::new(format!("cursor ACP 帧 id 无法表示为 i64: {id}"))),
        Value::String(s) => s
            .parse::<i64>()
            .map_err(|_| AppError::new(format!("cursor ACP 帧 id 字符串无法解析为 i64: {s:?}"))),
        other => Err(AppError::new(format!(
            "cursor ACP 帧 id 类型无效（期望 number 或 string）: {other}"
        ))),
    }
}

#[derive(Serialize)]
struct OutboundRequest<'a> {
    jsonrpc: &'a str,
    id: i64,
    method: &'a str,
    params: &'a Value,
}

#[derive(Serialize)]
struct OutboundNotification<'a> {
    jsonrpc: &'a str,
    method: &'a str,
    #[serde(skip_serializing_if = "Value::is_null")]
    params: &'a Value,
}

#[derive(Serialize)]
struct OutboundResponse<'a> {
    jsonrpc: &'a str,
    id: i64,
    result: &'a Value,
}

#[derive(Serialize)]
struct OutboundErrorResponse<'a> {
    jsonrpc: &'a str,
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
    fn encode_request_includes_jsonrpc_and_appends_newline() {
        let line = encode_request(7, "initialize", &serde_json::json!({"a": 1})).unwrap();
        assert!(line.ends_with('\n'));
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "initialize");
        assert_eq!(v["params"]["a"], 1);
    }

    #[test]
    fn encode_notification_omits_null_params() {
        let line = encode_notification("session/cancel", &Value::Null).unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "session/cancel");
        assert!(v.get("params").is_none());
    }

    #[test]
    fn encode_response_includes_jsonrpc() {
        let line = encode_response(
            5,
            &serde_json::json!({"outcome":{"outcome":"selected","optionId":"allow-once"}}),
        )
        .unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 5);
        assert_eq!(v["result"]["outcome"]["optionId"], "allow-once");
    }

    #[test]
    fn decode_classifies_response_ok() {
        match decode_line(r#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#)
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
        match decode_line(
            r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32601,"message":"not found"}}"#,
        )
        .unwrap()
        .unwrap()
        {
            Inbound::Response {
                id,
                payload: ResponsePayload::Err(e),
            } => {
                assert_eq!(id, 4);
                assert_eq!(e.code, -32601);
                assert_eq!(e.message, "not found");
            }
            other => panic!("expected error response, got {other:?}"),
        }
    }

    #[test]
    fn encode_error_includes_jsonrpc() {
        let line = encode_error_response(9, -32601, "method not handled by client").unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 9);
        assert_eq!(v["error"]["code"], -32601);
        assert!(v.get("result").is_none());
    }

    #[test]
    fn decode_wrong_jsonrpc_version_is_err() {
        let err = decode_line(r#"{"jsonrpc":"1.0","id":1,"result":{}}"#).expect_err("version");
        assert!(
            err.message.contains("jsonrpc"),
            "unexpected error: {}",
            err.message
        );
    }

    #[test]
    fn decode_classifies_server_request() {
        match decode_line(
            r#"{"jsonrpc":"2.0","id":5,"method":"session/request_permission","params":{}}"#,
        )
        .unwrap()
        .unwrap()
        {
            Inbound::ServerRequest { id, method, .. } => {
                assert_eq!(id, 5);
                assert_eq!(method, "session/request_permission");
            }
            other => panic!("expected server request, got {other:?}"),
        }
    }

    #[test]
    fn decode_classifies_notification() {
        match decode_line(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s"}}"#,
        )
        .unwrap()
        .unwrap()
        {
            Inbound::Notification { method, params } => {
                assert_eq!(method, "session/update");
                assert_eq!(params["sessionId"], "s");
            }
            other => panic!("expected notification, got {other:?}"),
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
    fn decode_missing_jsonrpc_is_err() {
        assert!(decode_line(r#"{"id":1,"result":{}}"#).is_err());
    }

    #[test]
    fn decode_neither_id_nor_method_is_err() {
        assert!(decode_line(r#"{"jsonrpc":"2.0","foo":1}"#).is_err());
    }

    #[test]
    fn decode_string_id_server_request() {
        match decode_line(
            r#"{"jsonrpc":"2.0","id":"42","method":"session/request_permission","params":{}}"#,
        )
        .unwrap()
        .unwrap()
        {
            Inbound::ServerRequest { id, method, .. } => {
                assert_eq!(id, 42);
                assert_eq!(method, "session/request_permission");
            }
            other => panic!("expected server request, got {other:?}"),
        }
    }

    #[test]
    fn decode_string_id_response() {
        match decode_line(r#"{"jsonrpc":"2.0","id":"3","result":{"ok":true}}"#)
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
    fn decode_invalid_id_shape_with_method_is_err_not_notification() {
        // Object id + method must fail closed — never silently become Notification.
        let err = decode_line(
            r#"{"jsonrpc":"2.0","id":{"n":1},"method":"session/request_permission","params":{}}"#,
        )
        .expect_err("invalid id shape");
        assert!(
            err.message.contains("id"),
            "unexpected error: {}",
            err.message
        );

        let err_str = decode_line(
            r#"{"jsonrpc":"2.0","id":"not-a-number","method":"session/update","params":{}}"#,
        )
        .expect_err("non-digit string id");
        assert!(
            err_str.message.contains("id"),
            "unexpected error: {}",
            err_str.message
        );
    }
}
