//! DingTalk messaging provider.

use axum::http::HeaderMap;
use base64::{engine::general_purpose, Engine as _};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::config::service::MessagingIntegration;
use crate::error::{AppError, AppResult};
use crate::messaging::provider::{MessagingProvider, ProviderFuture, Verification};
use crate::messaging::redact_raw_summary;
use crate::model::{
    ActionExecutionResult, MessagingEvent, MessagingProviderCapability, MessagingProviderKind,
    MessagingReplyTarget,
};

type HmacSha256 = Hmac<Sha256>;

pub struct DingTalkProvider;

const MAX_TIMESTAMP_SKEW_SECS: i64 = 300;

impl MessagingProvider for DingTalkProvider {
    fn kind(&self) -> MessagingProviderKind {
        MessagingProviderKind::DingTalk
    }

    fn capability(&self) -> MessagingProviderCapability {
        MessagingProviderCapability {
            provider: MessagingProviderKind::DingTalk,
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
        let timestamp =
            header(headers, "timestamp").or_else(|_| header(headers, "x-dingtalk-timestamp"))?;
        let signature = header(headers, "sign").or_else(|_| header(headers, "x-dingtalk-sign"))?;
        verify_timestamp_fresh(timestamp, crate::messaging::store::now_epoch())?;
        verify_signature(integration.app_secret.trim(), timestamp, signature)?;
        if let Ok(value) = serde_json::from_slice::<Value>(raw) {
            if let Some(challenge) = value.get("challenge").and_then(Value::as_str) {
                return Ok(Verification::UrlVerification {
                    challenge: challenge.to_string(),
                });
            }
        }
        Ok(Verification::Event)
    }

    fn parse_event(
        &self,
        raw: &[u8],
        integration: &MessagingIntegration,
        now: u64,
    ) -> AppResult<MessagingEvent> {
        let event: DingTalkEvent = serde_json::from_slice(raw)
            .map_err(|e| AppError::new(format!("钉钉事件 JSON 解析失败: {e}")))?;
        if event.msg_type.as_deref().unwrap_or("text") != "text" {
            return Err(AppError::new("钉钉事件不是文本消息，已忽略"));
        }
        let event_id = event
            .msg_id
            .clone()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| stable_event_id(raw));
        Ok(MessagingEvent {
            provider: MessagingProviderKind::DingTalk,
            integration_id: integration.id.clone(),
            event_id,
            conversation_id: event
                .conversation_id
                .as_deref()
                .unwrap_or_default()
                .to_string(),
            thread_id: event.msg_id.as_deref().unwrap_or_default().to_string(),
            sender_id: event.sender_id.as_deref().unwrap_or_default().to_string(),
            text: event
                .text
                .as_ref()
                .and_then(|text| text.content.as_deref())
                .unwrap_or_default()
                .to_string(),
            mentioned_bot: event.mentioned_bot(),
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
        Box::pin(async move {
            let webhook = dingtalk_webhook(integration, &target.conversation_id)?;
            post_text(integration, &webhook, text).await
        })
    }

    fn send<'a>(
        &'a self,
        integration: &'a MessagingIntegration,
        conversation_id: &'a str,
        text: &'a str,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            let webhook = dingtalk_webhook(integration, conversation_id)?;
            post_text(integration, &webhook, text).await
        })
    }
}

fn verify_signature(secret: &str, timestamp: &str, signature: &str) -> AppResult<()> {
    let expected = signed_value(secret, timestamp)?;
    if expected.as_bytes().ct_eq(signature.as_bytes()).into() {
        Ok(())
    } else {
        Err(AppError::new("钉钉事件签名校验失败"))
    }
}

fn verify_timestamp_fresh(timestamp: &str, now_epoch: u64) -> AppResult<()> {
    let millis = timestamp
        .trim()
        .parse::<i64>()
        .map_err(|_| AppError::new("钉钉事件 timestamp 无效"))?;
    let seconds = millis / 1000;
    let now = now_epoch as i64;
    if (now - seconds).abs() > MAX_TIMESTAMP_SKEW_SECS {
        return Err(AppError::new("钉钉事件 timestamp 已过期"));
    }
    Ok(())
}

fn signed_value(secret: &str, timestamp: &str) -> AppResult<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|e| AppError::new(format!("钉钉签名初始化失败: {e}")))?;
    mac.update(format!("{timestamp}\n{secret}").as_bytes());
    Ok(general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
}

fn dingtalk_webhook(
    integration: &MessagingIntegration,
    conversation_id: &str,
) -> AppResult<String> {
    let mut url = reqwest::Url::parse("https://oapi.dingtalk.com/robot/send")
        .map_err(|e| AppError::new(format!("钉钉 webhook URL 初始化失败: {e}")))?;
    url.query_pairs_mut()
        .append_pair("access_token", integration.verification_token.trim());
    if !integration.app_secret.trim().is_empty() {
        let timestamp = crate::messaging::store::now_epoch()
            .saturating_mul(1000)
            .to_string();
        let sign = signed_value(integration.app_secret.trim(), &timestamp)?;
        url.query_pairs_mut()
            .append_pair("timestamp", &timestamp)
            .append_pair("sign", &sign);
    }
    if !conversation_id.trim().is_empty() && conversation_id.trim() != "default" {
        url.query_pairs_mut()
            .append_pair("openConversationId", conversation_id.trim());
    }
    Ok(url.to_string())
}

async fn post_text(
    integration: &MessagingIntegration,
    webhook: &str,
    text: &str,
) -> AppResult<ActionExecutionResult> {
    if webhook.trim().is_empty() {
        return Ok(ActionExecutionResult::Dead {
            message: "钉钉回复缺少 sessionWebhook".to_string(),
        });
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(integration.timeout_secs))
        .build()
        .map_err(|e| AppError::new(format!("钉钉 HTTP client 初始化失败: {e}")))?;
    let resp = client
        .post(webhook)
        .json(&json!({
            "msgtype": "text",
            "text": { "content": text },
        }))
        .send()
        .await
        .map_err(|e| AppError::new(format!("钉钉发送消息请求失败: {}", e.without_url())))?;
    classify_dingtalk_response(resp, "钉钉发送消息").await
}

async fn classify_dingtalk_response(
    resp: reqwest::Response,
    op: &str,
) -> AppResult<ActionExecutionResult> {
    #[derive(Deserialize)]
    struct ApiResp {
        errcode: i64,
        errmsg: String,
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
    if body.errcode == 0 {
        Ok(ActionExecutionResult::Done)
    } else if body.errcode == 130101 || body.errcode == 130102 {
        Ok(ActionExecutionResult::Retry {
            message: format!("{op} 暂时失败: {}", body.errmsg),
            retry_after_secs: None,
        })
    } else {
        Ok(ActionExecutionResult::Dead {
            message: format!("{op} 失败: {}", body.errmsg),
        })
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> AppResult<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| AppError::new(format!("钉钉事件缺少请求头: {name}")))
}

fn stable_event_id(raw: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = Sha256::new();
    hasher.update(raw);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DingTalkEvent {
    #[serde(default)]
    msg_id: Option<String>,
    #[serde(default, rename = "msgtype", alias = "msgType")]
    msg_type: Option<String>,
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    sender_id: Option<String>,
    #[serde(default)]
    text: Option<DingTalkText>,
    #[serde(default)]
    is_in_at_list: Option<bool>,
    #[serde(default)]
    at_users: Vec<Value>,
}

impl DingTalkEvent {
    fn mentioned_bot(&self) -> bool {
        self.is_in_at_list.unwrap_or(false) || !self.at_users.is_empty()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DingTalkText {
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn integration() -> MessagingIntegration {
        MessagingIntegration {
            id: "dt".to_string(),
            verification_token: "token-123456".to_string(),
            app_secret: "secret-123456".to_string(),
            bot_open_id: "robot-code".to_string(),
            allowed_conversation_ids: vec!["cid".to_string()],
            ..MessagingIntegration::feishu_default()
        }
    }

    fn signed_headers(timestamp: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "timestamp",
            HeaderValue::from_str(timestamp).expect("timestamp"),
        );
        headers.insert(
            "sign",
            HeaderValue::from_str(&signed_value("secret-123456", timestamp).expect("sign"))
                .expect("sign header"),
        );
        headers
    }

    #[test]
    fn verifies_signature_and_parses_text_event() {
        let raw = br#"{"msgId":"m1","msgtype":"text","conversationId":"cid","senderId":"u","sessionWebhook":"https://example.com/reply","text":{"content":"/help"},"isInAtList":true}"#;
        let provider = DingTalkProvider;
        let timestamp = format!("{}000", crate::messaging::store::now_epoch());
        provider
            .verify(&signed_headers(&timestamp), raw, &integration())
            .expect("verify");
        let event = provider
            .parse_event(raw, &integration(), 42)
            .expect("parse");
        assert_eq!(event.provider, MessagingProviderKind::DingTalk);
        assert_eq!(event.event_id, "m1");
        assert_eq!(event.conversation_id, "cid");
        assert_eq!(event.thread_id, "m1");
        assert_eq!(event.text, "/help");
        assert!(event.mentioned_bot);
    }

    #[test]
    fn rejects_actual_wire_non_text_msgtype() {
        let raw = br#"{"msgId":"m2","msgtype":"image","conversationId":"cid","senderId":"u","text":{"content":"not text"}}"#;
        let err = DingTalkProvider
            .parse_event(raw, &integration(), 42)
            .expect_err("non-text msgtype must not default to text");
        assert!(err.message.contains("不是文本消息"), "{}", err.message);
    }

    #[tokio::test]
    async fn request_error_does_not_include_secret_webhook_url() {
        let err = post_text(
            &MessagingIntegration {
                timeout_secs: 1,
                ..integration()
            },
            "http://127.0.0.1:9/robot/send?access_token=secret-token&sign=secret-sign",
            "hello",
        )
        .await
        .expect_err("closed local port should fail");
        assert!(!err.message.contains("secret-token"), "{}", err.message);
        assert!(!err.message.contains("secret-sign"), "{}", err.message);
        assert!(!err.message.contains("access_token"), "{}", err.message);
    }

    #[test]
    fn rejects_bad_signature_expired_timestamp_and_redacts_secrets() {
        let provider = DingTalkProvider;
        assert!(provider
            .verify(&HeaderMap::new(), b"{}", &integration())
            .is_err());
        assert!(
            verify_timestamp_fresh("1700000000000", crate::messaging::store::now_epoch()).is_err()
        );
        let summary =
            redact_raw_summary(br#"{"accessToken":"t","sessionWebhook":"https://secret"}"#);
        assert!(!summary.contains("https://secret"));
        assert!(!summary.contains("\"t\""));
        assert!(summary.contains("[redacted]"));
    }
}
