//! Bidirectional messaging/bot integrations (#1559).
//!
//! This slice owns provider verification/parsing, messaging-event persistence, command parsing,
//! and reply production. It is separate from outbound notifications: `NotificationKind::Feishu`
//! remains a one-way webhook channel, while this slice handles Feishu bot ingress + replies.

pub mod commands;
pub mod dingtalk;
pub mod feishu;
pub(crate) mod local_api;
pub mod provider;
pub mod service;
pub mod store;
pub mod wechat_work;

pub(crate) const MAX_RAW_SUMMARY: usize = 4096;

pub(crate) fn truncate_utf8_boundary(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

pub(crate) fn redact_raw_summary(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut value =
        serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({ "raw": text }));
    redact_value(&mut value);
    let out = serde_json::to_string(&value).unwrap_or_default();
    truncate_utf8_boundary(&out, MAX_RAW_SUMMARY)
}

fn redact_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if is_secret_like_key(key) {
                    *value = serde_json::Value::String("[redacted]".to_string());
                } else {
                    redact_value(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_value(value);
            }
        }
        _ => {}
    }
}

fn is_secret_like_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.contains("secret")
        || lower.contains("token")
        || lower.contains("authorization")
        || lower.contains("encrypt_key")
        || lower.contains("encryptkey")
        || lower.contains("encodingaeskey")
        || lower.contains("access_token")
        || lower.contains("accesstoken")
        || lower.contains("msg_signature")
        || lower.contains("sessionwebhook")
        || lower.contains("session_webhook")
        || lower == "sign"
}
