//! Bidirectional messaging/bot integrations (#1559).
//!
//! This slice owns provider verification/parsing, messaging-event persistence, command parsing,
//! and reply production. It is separate from outbound notifications: `NotificationKind::Feishu`
//! remains a one-way webhook channel, while this slice handles Feishu bot ingress + replies.

pub mod commands;
pub mod feishu;
pub mod provider;
pub mod service;
pub mod store;

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
