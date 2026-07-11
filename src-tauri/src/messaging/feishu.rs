//! Feishu messaging provider (#1559).

use axum::http::HeaderMap;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::service::MessagingIntegration;
use crate::error::{AppError, AppResult};
use crate::messaging::provider::{MessagingProvider, ProviderFuture, Verification};
use crate::messaging::redact_raw_summary;
use crate::model::{
    ActionExecutionResult, MessagingEvent, MessagingProviderCapability, MessagingProviderKind,
    MessagingReplyTarget,
};

pub struct FeishuProvider;

const MAX_SIGNATURE_AGE_SECS: i64 = 5 * 60;

impl FeishuProvider {
    fn verify_url_challenge(
        raw: &[u8],
        integration: &MessagingIntegration,
    ) -> AppResult<Option<String>> {
        let Ok(value) = serde_json::from_slice::<Value>(raw) else {
            return Ok(None);
        };
        if value.get("type").and_then(Value::as_str) != Some("url_verification") {
            return Ok(None);
        }
        let token = value
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !constant_time_eq(token, integration.verification_token.trim()) {
            return Err(AppError::new("飞书 URL 校验 token 不匹配"));
        }
        let challenge = value
            .get("challenge")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::new("飞书 URL 校验缺少 challenge"))?;
        Ok(Some(challenge.to_string()))
    }

    fn verify_signature(
        headers: &HeaderMap,
        raw: &[u8],
        integration: &MessagingIntegration,
    ) -> AppResult<()> {
        let timestamp = header(headers, "x-lark-request-timestamp")?;
        verify_timestamp_fresh(&timestamp)?;
        let nonce = header(headers, "x-lark-request-nonce")?;
        let signature = header(headers, "x-lark-signature")?;
        let mut hasher = Sha256::new();
        hasher.update(timestamp.as_bytes());
        hasher.update(nonce.as_bytes());
        hasher.update(integration.encrypt_key.trim().as_bytes());
        hasher.update(raw);
        let expected = hex::encode(hasher.finalize());
        if !constant_time_eq(&expected, &signature) {
            return Err(AppError::new("飞书事件签名校验失败"));
        }
        Ok(())
    }
}

impl MessagingProvider for FeishuProvider {
    fn kind(&self) -> MessagingProviderKind {
        MessagingProviderKind::Feishu
    }

    fn capability(&self) -> MessagingProviderCapability {
        MessagingProviderCapability {
            provider: MessagingProviderKind::Feishu,
            supports_reply: true,
            supports_send: true,
            requires_allowed_conversations: true,
        }
    }

    fn verify(
        &self,
        headers: &HeaderMap,
        raw: &[u8],
        integration: &MessagingIntegration,
    ) -> AppResult<Verification> {
        if let Some(challenge) = Self::verify_url_challenge(raw, integration)? {
            return Ok(Verification::UrlVerification { challenge });
        }
        Self::verify_signature(headers, raw, integration)?;
        Ok(Verification::Event)
    }

    fn parse_event(
        &self,
        raw: &[u8],
        integration: &MessagingIntegration,
        now: u64,
    ) -> AppResult<MessagingEvent> {
        let envelope: FeishuEnvelope = serde_json::from_slice(raw)
            .map_err(|e| AppError::new(format!("飞书事件 JSON 解析失败: {e}")))?;
        if !constant_time_eq(
            envelope.header.token.trim(),
            integration.verification_token.trim(),
        ) {
            return Err(AppError::new("飞书事件 token 校验失败"));
        }
        if envelope.header.event_type != "im.message.receive_v1" {
            return Err(AppError::new(format!(
                "飞书事件类型不支持: {}",
                envelope.header.event_type
            )));
        }
        if envelope.header.event_id.trim().is_empty() {
            return Err(AppError::new("飞书事件缺少 event_id"));
        }
        let message = envelope.event.message;
        if message.message_type.as_deref().unwrap_or("text") != "text" {
            return Err(AppError::new("飞书事件不是文本消息，已忽略"));
        }
        let content: FeishuTextContent = serde_json::from_str(&message.content)
            .map_err(|e| AppError::new(format!("飞书文本内容解析失败: {e}")))?;
        let mentioned_bot = mentions_bot(
            message.mentions.as_deref().unwrap_or(&[]),
            integration.bot_open_id.trim(),
        );
        Ok(MessagingEvent {
            provider: MessagingProviderKind::Feishu,
            integration_id: integration.id.clone(),
            event_id: envelope.header.event_id,
            conversation_id: message.chat_id,
            thread_id: message.message_id.clone(),
            sender_id: envelope
                .event
                .sender
                .sender_id
                .and_then(|id| id.user_id.or(id.open_id).or(id.union_id))
                .unwrap_or_default(),
            text: content.text.trim().to_string(),
            mentioned_bot,
            raw_payload: redact_raw_summary(raw),
            received_at_epoch: now,
        })
    }

    fn reply<'a>(
        &'a self,
        integration: &'a MessagingIntegration,
        target: &'a MessagingReplyTarget,
        text: &'a str,
    ) -> ProviderFuture<'a> {
        Box::pin(async move { reply_message(integration, target, text).await })
    }

    fn send<'a>(
        &'a self,
        integration: &'a MessagingIntegration,
        conversation_id: &'a str,
        text: &'a str,
    ) -> ProviderFuture<'a> {
        Box::pin(async move { send_message(integration, conversation_id, text).await })
    }
}

fn header(headers: &HeaderMap, name: &str) -> AppResult<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .ok_or_else(|| AppError::new(format!("飞书事件缺少请求头: {name}")))
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    left.as_bytes().ct_eq(right.as_bytes()).into()
}

fn verify_timestamp_fresh(timestamp: &str) -> AppResult<()> {
    let ts = timestamp
        .parse::<i64>()
        .map_err(|_| AppError::new("飞书事件 timestamp 非法"))?;
    let now = crate::messaging::store::now_epoch() as i64;
    if (now - ts).abs() > MAX_SIGNATURE_AGE_SECS {
        return Err(AppError::new("飞书事件 timestamp 已过期"));
    }
    Ok(())
}

fn mentions_bot(mentions: &[Value], bot_open_id: &str) -> bool {
    if bot_open_id.is_empty() {
        return false;
    }
    mentions.iter().any(|mention| {
        mention
            .get("id")
            .and_then(|id| id.get("open_id").or_else(|| id.get("openId")))
            .and_then(Value::as_str)
            .is_some_and(|open_id| open_id == bot_open_id)
            || mention
                .get("open_id")
                .or_else(|| mention.get("openId"))
                .and_then(Value::as_str)
                .is_some_and(|open_id| open_id == bot_open_id)
    })
}

async fn reply_message(
    integration: &MessagingIntegration,
    target: &MessagingReplyTarget,
    text: &str,
) -> AppResult<ActionExecutionResult> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(integration.timeout_secs))
        .build()
        .map_err(|e| AppError::new(format!("飞书 HTTP client 初始化失败: {e}")))?;
    let token = tenant_access_token(&client, integration).await?;
    let url = format!(
        "https://open.feishu.cn/open-apis/im/v1/messages/{}/reply",
        target.message_id
    );
    let resp = client
        .post(url)
        .bearer_auth(token)
        .json(&json!({
            "msg_type": "text",
            "content": serde_json::to_string(&json!({ "text": text })).expect("text content serializes"),
        }))
        .send()
        .await
        .map_err(|e| AppError::new(format!("飞书回复请求失败: {e}")))?;
    classify_feishu_response(resp, "飞书回复消息").await
}

async fn send_message(
    integration: &MessagingIntegration,
    conversation_id: &str,
    text: &str,
) -> AppResult<ActionExecutionResult> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(integration.timeout_secs))
        .build()
        .map_err(|e| AppError::new(format!("飞书 HTTP client 初始化失败: {e}")))?;
    let token = tenant_access_token(&client, integration).await?;
    let resp = client
        .post("https://open.feishu.cn/open-apis/im/v1/messages")
        .query(&[("receive_id_type", "chat_id")])
        .bearer_auth(token)
        .json(&json!({
            "receive_id": conversation_id,
            "msg_type": "text",
            "content": serde_json::to_string(&json!({ "text": text })).expect("text content serializes"),
        }))
        .send()
        .await
        .map_err(|e| AppError::new(format!("飞书发送消息请求失败: {e}")))?;
    classify_feishu_response(resp, "飞书发送消息").await
}

async fn tenant_access_token(
    client: &reqwest::Client,
    integration: &MessagingIntegration,
) -> AppResult<String> {
    #[derive(Deserialize)]
    struct TokenResp {
        code: i64,
        msg: String,
        tenant_access_token: Option<String>,
    }
    let resp = client
        .post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
        .json(&json!({
            "app_id": integration.app_id,
            "app_secret": integration.app_secret,
        }))
        .send()
        .await
        .map_err(|e| AppError::new(format!("飞书 tenant_access_token 请求失败: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(AppError::new(format!(
            "飞书 tenant_access_token HTTP 失败: {status}"
        )));
    }
    let body: TokenResp = resp
        .json()
        .await
        .map_err(|e| AppError::new(format!("飞书 tenant_access_token 响应解析失败: {e}")))?;
    if body.code != 0 {
        return Err(AppError::new(format!(
            "飞书 tenant_access_token 失败: {}",
            body.msg
        )));
    }
    body.tenant_access_token
        .filter(|token| !token.is_empty())
        .ok_or_else(|| AppError::new("飞书 tenant_access_token 响应缺少 token"))
}

async fn classify_feishu_response(
    resp: reqwest::Response,
    op: &str,
) -> AppResult<ActionExecutionResult> {
    #[derive(Deserialize)]
    struct ApiResp {
        code: i64,
        msg: String,
    }
    let status = resp.status();
    if status.as_u16() == 429 || status.is_server_error() {
        return Ok(ActionExecutionResult::Retry {
            message: format!("{op} 暂时失败: HTTP {status}"),
            retry_after_secs: None,
        });
    }
    if !status.is_success() {
        return Ok(ActionExecutionResult::Dead {
            message: format!("{op} 不可重试失败: HTTP {status}"),
        });
    }
    let body: ApiResp = resp
        .json()
        .await
        .map_err(|e| AppError::new(format!("{op} 响应解析失败: {e}")))?;
    if body.code == 0 {
        Ok(ActionExecutionResult::done())
    } else if body.code == 99991663 || body.code == 99991664 {
        Ok(ActionExecutionResult::Retry {
            message: format!("{op} 暂时失败: {}", body.msg),
            retry_after_secs: None,
        })
    } else {
        Ok(ActionExecutionResult::Dead {
            message: format!("{op} 失败: {}", body.msg),
        })
    }
}

#[derive(Deserialize)]
struct FeishuEnvelope {
    header: FeishuHeader,
    event: FeishuEvent,
}

#[derive(Deserialize)]
struct FeishuHeader {
    event_id: String,
    event_type: String,
    #[serde(default)]
    token: String,
}

#[derive(Deserialize)]
struct FeishuEvent {
    sender: FeishuSender,
    message: FeishuMessage,
}

#[derive(Deserialize)]
struct FeishuSender {
    sender_id: Option<FeishuSenderId>,
}

#[derive(Deserialize)]
struct FeishuSenderId {
    user_id: Option<String>,
    open_id: Option<String>,
    union_id: Option<String>,
}

#[derive(Deserialize)]
struct FeishuMessage {
    message_id: String,
    chat_id: String,
    content: String,
    #[serde(default)]
    message_type: Option<String>,
    #[serde(default)]
    mentions: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct FeishuTextContent {
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::store;
    use axum::http::HeaderValue;

    fn integration() -> MessagingIntegration {
        MessagingIntegration {
            id: "fs".to_string(),
            verification_token: "verify-token-1234".to_string(),
            encrypt_key: "encrypt-key-1234".to_string(),
            bot_open_id: "bot-open-id".to_string(),
            allowed_conversation_ids: vec!["chat-a".to_string()],
            ..MessagingIntegration::feishu_default()
        }
    }

    fn event_body() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema": "2.0",
            "header": {
                "event_id": "evt-1",
                "event_type": "im.message.receive_v1",
                "token": "verify-token-1234"
            },
            "event": {
                "sender": { "sender_id": { "user_id": "u1" } },
                "message": {
                    "message_id": "m1",
                    "chat_id": "chat-a",
                    "message_type": "text",
                    "content": "{\"text\":\"/help\"}",
                    "mentions": [{ "name": "bot", "id": { "open_id": "bot-open-id" } }]
                }
            }
        }))
        .expect("body serializes")
    }

    fn signed_headers(raw: &[u8]) -> HeaderMap {
        signed_headers_with_timestamp(raw, &store::now_epoch().to_string())
    }

    fn signed_headers_with_timestamp(raw: &[u8], timestamp: &str) -> HeaderMap {
        let mut hasher = Sha256::new();
        hasher.update(timestamp.as_bytes());
        hasher.update(b"n");
        hasher.update(b"encrypt-key-1234");
        hasher.update(raw);
        let signature = hex::encode(hasher.finalize());
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-lark-request-timestamp",
            HeaderValue::from_str(timestamp).expect("timestamp header"),
        );
        headers.insert("x-lark-request-nonce", HeaderValue::from_static("n"));
        headers.insert(
            "x-lark-signature",
            HeaderValue::from_str(&signature).expect("signature header"),
        );
        headers
    }

    #[test]
    fn url_verification_checks_token_and_returns_challenge() {
        let provider = FeishuProvider;
        let raw = br#"{"type":"url_verification","token":"verify-token-1234","challenge":"abc"}"#;
        let out = provider
            .verify(&HeaderMap::new(), raw, &integration())
            .expect("verify challenge");
        assert_eq!(
            out,
            Verification::UrlVerification {
                challenge: "abc".to_string()
            }
        );
    }

    #[test]
    fn signature_must_match_before_event_parse() {
        let provider = FeishuProvider;
        let raw = event_body();
        assert!(provider
            .verify(&signed_headers(&raw), &raw, &integration())
            .is_ok());
        assert!(provider
            .verify(&signed_headers(b"other"), &raw, &integration())
            .is_err());
    }

    #[test]
    fn signature_rejects_missing_invalid_and_expired_timestamp() {
        let provider = FeishuProvider;
        let raw = event_body();
        let mut missing = signed_headers(&raw);
        missing.remove("x-lark-request-timestamp");
        assert!(provider.verify(&missing, &raw, &integration()).is_err());

        let invalid = signed_headers_with_timestamp(&raw, "not-a-timestamp");
        let err = provider
            .verify(&invalid, &raw, &integration())
            .expect_err("invalid timestamp rejected");
        assert!(err.to_string().contains("timestamp 非法"));

        let expired_ts = store::now_epoch().saturating_sub(301).to_string();
        let expired = signed_headers_with_timestamp(&raw, &expired_ts);
        let err = provider
            .verify(&expired, &raw, &integration())
            .expect_err("expired timestamp rejected");
        assert!(err.to_string().contains("timestamp 已过期"));
    }

    #[test]
    fn receive_event_normalizes_text_message() {
        let provider = FeishuProvider;
        let event = provider
            .parse_event(&event_body(), &integration(), 42)
            .expect("parse");
        assert_eq!(event.provider, MessagingProviderKind::Feishu);
        assert_eq!(event.integration_id, "fs");
        assert_eq!(event.event_id, "evt-1");
        assert_eq!(event.conversation_id, "chat-a");
        assert_eq!(event.thread_id, "m1");
        assert_eq!(event.sender_id, "u1");
        assert_eq!(event.text, "/help");
        assert!(event.mentioned_bot);
        assert_eq!(event.received_at_epoch, 42);
    }

    #[test]
    fn receive_event_rejects_bad_verification_token() {
        let provider = FeishuProvider;
        let raw = serde_json::to_vec(&json!({
            "schema": "2.0",
            "header": {
                "event_id": "evt-1",
                "event_type": "im.message.receive_v1",
                "token": "wrong"
            },
            "event": {
                "sender": { "sender_id": { "user_id": "u1" } },
                "message": {
                    "message_id": "m1",
                    "chat_id": "chat-a",
                    "message_type": "text",
                    "content": "{\"text\":\"/help\"}"
                }
            }
        }))
        .expect("body serializes");

        let err = provider
            .parse_event(&raw, &integration(), 42)
            .expect_err("bad token rejected");
        assert!(err.to_string().contains("token 校验失败"));
    }

    #[test]
    fn raw_summary_redacts_secret_like_fields() {
        let raw = br#"{"appSecret":"s","authorization":"bearer x","nested":{"token":"t"}}"#;
        let summary = redact_raw_summary(raw);
        assert!(!summary.contains("bearer x"));
        assert!(!summary.contains("\"s\""));
        assert!(!summary.contains("\"t\""));
        assert!(summary.contains("[redacted]"));
    }

    #[test]
    fn raw_summary_truncates_non_ascii_on_char_boundary() {
        let raw = serde_json::to_vec(&json!({ "text": "错误".repeat(3000) })).expect("json");
        let summary = redact_raw_summary(&raw);
        assert!(summary.len() <= crate::messaging::MAX_RAW_SUMMARY);
        assert!(summary.is_char_boundary(summary.len()));
    }

    #[test]
    fn mentions_only_match_configured_bot_open_id() {
        assert!(mentions_bot(
            &[json!({ "id": { "open_id": "bot-open-id" } })],
            "bot-open-id",
        ));
        assert!(!mentions_bot(
            &[json!({ "id": { "open_id": "someone-else" } })],
            "bot-open-id",
        ));
    }
}
