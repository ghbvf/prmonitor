//! WeChat Work messaging provider.

use aes::Aes256;
use axum::http::HeaderMap;
use base64::{engine::general_purpose, Engine as _};
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use quick_xml::events::Event as XmlEvent;
use quick_xml::Reader;
use serde::Deserialize;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use subtle::ConstantTimeEq;

use crate::config::service::MessagingIntegration;
use crate::error::{AppError, AppResult};
use crate::messaging::provider::{MessagingProvider, ProviderFuture, Verification};
use crate::messaging::redact_raw_summary;
use crate::model::{
    ActionExecutionResult, MessagingEvent, MessagingProviderCapability, MessagingProviderKind,
    MessagingReplyTarget,
};

pub struct WeChatWorkProvider;

type Aes256CbcDec = cbc::Decryptor<Aes256>;
const MAX_TIMESTAMP_SKEW_SECS: i64 = 300;

impl MessagingProvider for WeChatWorkProvider {
    fn kind(&self) -> MessagingProviderKind {
        MessagingProviderKind::WeChatWork
    }

    fn capability(&self) -> MessagingProviderCapability {
        MessagingProviderCapability {
            provider: MessagingProviderKind::WeChatWork,
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
        if let Some(challenge) = header_optional(headers, "x-wechatwork-echostr") {
            verify_signature(
                integration.verification_token.trim(),
                header(headers, "x-wechatwork-timestamp")?,
                header(headers, "x-wechatwork-nonce")?,
                challenge,
                header(headers, "x-wechatwork-msg-signature")?,
            )?;
            verify_timestamp_fresh(
                header(headers, "x-wechatwork-timestamp")?,
                crate::messaging::store::now_epoch(),
            )?;
            let challenge = decrypt_wechat_payload(integration, challenge)
                .unwrap_or_else(|_| challenge.to_string());
            return Ok(Verification::UrlVerification { challenge });
        }
        let body = std::str::from_utf8(raw).unwrap_or_default();
        let encrypted = encrypted_payload(raw);
        let signed_payload = encrypted.as_deref().unwrap_or(body);
        verify_signature(
            integration.verification_token.trim(),
            header(headers, "x-wechatwork-timestamp")?,
            header(headers, "x-wechatwork-nonce")?,
            signed_payload,
            header(headers, "x-wechatwork-msg-signature")?,
        )?;
        verify_timestamp_fresh(
            header(headers, "x-wechatwork-timestamp")?,
            crate::messaging::store::now_epoch(),
        )?;
        Ok(Verification::Event)
    }

    fn parse_event(
        &self,
        raw: &[u8],
        integration: &MessagingIntegration,
        now: u64,
    ) -> AppResult<MessagingEvent> {
        if let Some(encrypt) = encrypted_payload(raw) {
            let decrypted = decrypt_wechat_payload(integration, &encrypt)?;
            return parse_plain_event(decrypted.as_bytes(), integration, now, raw);
        }
        parse_plain_event(raw, integration, now, raw)
    }

    fn reply<'a>(
        &'a self,
        integration: &'a MessagingIntegration,
        target: &'a MessagingReplyTarget,
        text: &'a str,
    ) -> ProviderFuture<'a> {
        Box::pin(async move { send_text(integration, &target.conversation_id, text).await })
    }

    fn send<'a>(
        &'a self,
        integration: &'a MessagingIntegration,
        conversation_id: &'a str,
        text: &'a str,
    ) -> ProviderFuture<'a> {
        Box::pin(async move { send_text(integration, conversation_id, text).await })
    }
}

fn parse_plain_event(
    raw: &[u8],
    integration: &MessagingIntegration,
    now: u64,
    raw_summary_source: &[u8],
) -> AppResult<MessagingEvent> {
    let event = match serde_json::from_slice::<Value>(raw) {
        Ok(value) => parse_json_event(value)?,
        Err(_) => parse_xml_event(raw)?,
    };
    if event.msg_type.as_deref().unwrap_or("text") != "text" {
        return Err(AppError::new("企业微信事件不是文本消息，已忽略"));
    }
    let event_id = event
        .msg_id
        .clone()
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| stable_event_id(raw));
    Ok(MessagingEvent {
        provider: MessagingProviderKind::WeChatWork,
        integration_id: integration.id.clone(),
        event_id,
        conversation_id: event.conversation_id(),
        thread_id: event.msg_id.as_deref().unwrap_or_default().to_string(),
        sender_id: event
            .from_user_name
            .as_deref()
            .unwrap_or_default()
            .to_string(),
        text: event
            .content
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string(),
        mentioned_bot: event.mentioned_bot(integration.bot_open_id.trim()),
        raw_payload: redact_raw_summary(raw_summary_source),
        received_at_epoch: now,
    })
}

fn parse_json_event(value: Value) -> AppResult<WeChatWorkEvent> {
    serde_json::from_value(value)
        .map_err(|e| AppError::new(format!("企业微信事件结构解析失败: {e}")))
}

fn parse_xml_event(raw: &[u8]) -> AppResult<WeChatWorkEvent> {
    let xml = std::str::from_utf8(raw)
        .map_err(|e| AppError::new(format!("企业微信事件 XML 不是 UTF-8: {e}")))?;
    let values = xml_fields(xml)?;
    Ok(WeChatWorkEvent {
        msg_id: values.get("MsgId").cloned(),
        msg_type: values.get("MsgType").cloned(),
        from_user_name: values.get("FromUserName").cloned(),
        room_id: values.get("RoomId").cloned(),
        chat_id: values.get("ChatId").cloned(),
        content: values.get("Content").cloned(),
        is_at_all: values.get("IsAtAll").and_then(|v| v.parse::<u8>().ok()),
        to_user_name: values.get("ToUserName").cloned(),
    })
}

fn xml_fields(xml: &str) -> AppResult<std::collections::BTreeMap<String, String>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut fields = std::collections::BTreeMap::new();
    let mut current: Option<String> = None;
    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(e)) => {
                current = Some(String::from_utf8_lossy(e.name().as_ref()).to_string());
            }
            Ok(XmlEvent::Text(e)) => {
                if let Some(key) = current.as_ref() {
                    let value = e
                        .decode()
                        .map_err(|err| AppError::new(format!("企业微信 XML 文本解析失败: {err}")))?
                        .into_owned();
                    fields.insert(key.clone(), value);
                }
            }
            Ok(XmlEvent::CData(e)) => {
                if let Some(key) = current.as_ref() {
                    fields.insert(key.clone(), String::from_utf8_lossy(&e).to_string());
                }
            }
            Ok(XmlEvent::End(_)) => {
                current = None;
            }
            Ok(XmlEvent::Eof) => break,
            Err(e) => return Err(AppError::new(format!("企业微信事件 XML 解析失败: {e}"))),
            _ => {}
        }
    }
    Ok(fields)
}

fn encrypted_payload(raw: &[u8]) -> Option<String> {
    if let Ok(value) = serde_json::from_slice::<Value>(raw) {
        return value
            .get("Encrypt")
            .or_else(|| value.get("encrypt"))
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    let xml = std::str::from_utf8(raw).ok()?;
    xml_fields(xml).ok()?.get("Encrypt").cloned()
}

fn decrypt_wechat_payload(
    integration: &MessagingIntegration,
    encrypted: &str,
) -> AppResult<String> {
    let key = decode_encoding_aes_key(integration.encrypt_key.trim())?;
    let ciphertext = general_purpose::STANDARD
        .decode(encrypted)
        .map_err(|e| AppError::new(format!("企业微信加密消息 base64 解码失败: {e}")))?;
    let mut buf = ciphertext;
    let plaintext = Aes256CbcDec::new((&key).into(), (&key[..16]).into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| AppError::new(format!("企业微信加密消息 AES 解密失败: {e}")))?;
    if plaintext.len() < 20 {
        return Err(AppError::new("企业微信加密消息明文长度不足"));
    }
    let msg_len =
        u32::from_be_bytes([plaintext[16], plaintext[17], plaintext[18], plaintext[19]]) as usize;
    let msg_start: usize = 20;
    let msg_end = msg_start
        .checked_add(msg_len)
        .filter(|end| *end <= plaintext.len())
        .ok_or_else(|| AppError::new("企业微信加密消息长度字段无效"))?;
    let receive_id = std::str::from_utf8(&plaintext[msg_end..])
        .map_err(|e| AppError::new(format!("企业微信 receiveId 不是 UTF-8: {e}")))?;
    let expected = integration.app_id.trim();
    if !expected.is_empty() && receive_id != expected {
        return Err(AppError::new(
            "企业微信加密消息 receiveId 与 Corp ID 不匹配",
        ));
    }
    std::str::from_utf8(&plaintext[msg_start..msg_end])
        .map(str::to_string)
        .map_err(|e| AppError::new(format!("企业微信加密消息正文不是 UTF-8: {e}")))
}

fn decode_encoding_aes_key(value: &str) -> AppResult<[u8; 32]> {
    let padded = match value.len() % 4 {
        0 => value.to_string(),
        n => format!("{value}{}", "=".repeat(4 - n)),
    };
    let decoded = general_purpose::STANDARD
        .decode(padded)
        .map_err(|e| AppError::new(format!("企业微信 EncodingAESKey base64 解码失败: {e}")))?;
    decoded
        .try_into()
        .map_err(|_| AppError::new("企业微信 EncodingAESKey 必须解码为 32 字节"))
}

fn verify_signature(
    token: &str,
    timestamp: &str,
    nonce: &str,
    signed_payload: &str,
    signature: &str,
) -> AppResult<()> {
    let mut parts = [token, timestamp, nonce, signed_payload];
    parts.sort_unstable();
    let mut signed = String::new();
    for part in parts {
        signed.push_str(part);
    }
    let expected = sha1_hex(signed.as_bytes());
    if expected.as_bytes().ct_eq(signature.as_bytes()).into() {
        Ok(())
    } else {
        Err(AppError::new("企业微信事件签名校验失败"))
    }
}

fn verify_timestamp_fresh(timestamp: &str, now_epoch: u64) -> AppResult<()> {
    let seconds = timestamp
        .trim()
        .parse::<i64>()
        .map_err(|_| AppError::new("企业微信事件 timestamp 无效"))?;
    let now = now_epoch as i64;
    if (now - seconds).abs() > MAX_TIMESTAMP_SKEW_SECS {
        return Err(AppError::new("企业微信事件 timestamp 已过期"));
    }
    Ok(())
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> AppResult<&'a str> {
    header_optional(headers, name)
        .ok_or_else(|| AppError::new(format!("企业微信事件缺少请求头: {name}")))
}

fn header_optional<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn stable_event_id(raw: &[u8]) -> String {
    format!("sha1:{}", sha1_hex(raw))
}

fn sha1_hex(input: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(input);
    hex::encode(hasher.finalize())
}

async fn send_text(
    integration: &MessagingIntegration,
    conversation_id: &str,
    text: &str,
) -> AppResult<ActionExecutionResult> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(integration.timeout_secs))
        .build()
        .map_err(|e| AppError::new(format!("企业微信 HTTP client 初始化失败: {e}")))?;
    let token = access_token(&client, integration).await?;
    let agent_id = integration
        .bot_open_id
        .trim()
        .parse::<u64>()
        .map_err(|_| AppError::new("企业微信 AgentId 必须是数字"))?;
    let resp = client
        .post("https://qyapi.weixin.qq.com/cgi-bin/message/send")
        .query(&[("access_token", token.as_str())])
        .json(&json!({
            "touser": conversation_id,
            "msgtype": "text",
            "agentid": agent_id,
            "text": { "content": text },
            "safe": 0,
        }))
        .send()
        .await
        .map_err(|e| AppError::new(format!("企业微信发送消息请求失败: {}", e.without_url())))?;
    classify_wechat_response(resp, "企业微信发送消息").await
}

async fn access_token(
    client: &reqwest::Client,
    integration: &MessagingIntegration,
) -> AppResult<String> {
    #[derive(Deserialize)]
    struct TokenResp {
        errcode: i64,
        errmsg: String,
        access_token: Option<String>,
    }
    let resp = client
        .get("https://qyapi.weixin.qq.com/cgi-bin/gettoken")
        .query(&[
            ("corpid", integration.app_id.trim()),
            ("corpsecret", integration.app_secret.trim()),
        ])
        .send()
        .await
        .map_err(|e| {
            AppError::new(format!(
                "企业微信 access_token 请求失败: {}",
                e.without_url()
            ))
        })?;
    let body: TokenResp = resp
        .json()
        .await
        .map_err(|e| AppError::new(format!("企业微信 access_token 响应解析失败: {e}")))?;
    if body.errcode != 0 {
        return Err(AppError::new(format!(
            "企业微信 access_token 失败: {}",
            body.errmsg
        )));
    }
    body.access_token
        .filter(|token| !token.is_empty())
        .ok_or_else(|| AppError::new("企业微信 access_token 响应缺少 token"))
}

async fn classify_wechat_response(
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
        Ok(ActionExecutionResult::done())
    } else if body.errcode == 45009 || body.errcode == 45047 {
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

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WeChatWorkEvent {
    #[serde(default)]
    msg_id: Option<String>,
    #[serde(default)]
    msg_type: Option<String>,
    #[serde(default)]
    from_user_name: Option<String>,
    #[serde(default)]
    room_id: Option<String>,
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    is_at_all: Option<u8>,
    #[serde(default)]
    to_user_name: Option<String>,
}

impl WeChatWorkEvent {
    fn conversation_id(&self) -> String {
        self.chat_id
            .clone()
            .or_else(|| self.room_id.clone())
            .or_else(|| self.from_user_name.clone())
            .unwrap_or_default()
    }

    fn mentioned_bot(&self, bot_open_id: &str) -> bool {
        self.is_at_all == Some(1)
            || (!bot_open_id.is_empty()
                && self
                    .to_user_name
                    .as_deref()
                    .is_some_and(|to| to == bot_open_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use cbc::cipher::BlockEncryptMut;

    type Aes256CbcEnc = cbc::Encryptor<Aes256>;
    const TEST_AES_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn integration() -> MessagingIntegration {
        MessagingIntegration {
            id: "wx".to_string(),
            verification_token: "verify-token-1234".to_string(),
            app_id: "corp".to_string(),
            app_secret: "secret".to_string(),
            encrypt_key: TEST_AES_KEY.to_string(),
            bot_open_id: "1000002".to_string(),
            allowed_conversation_ids: vec!["u1".to_string()],
            ..MessagingIntegration::feishu_default()
        }
    }

    fn signed_headers(raw: &str) -> HeaderMap {
        let timestamp = crate::messaging::store::now_epoch().to_string();
        signed_headers_at(raw, &timestamp)
    }

    fn signed_headers_at(raw: &str, timestamp: &str) -> HeaderMap {
        let nonce = "n";
        let mut parts = ["verify-token-1234", timestamp, nonce, raw];
        parts.sort_unstable();
        let mut signed = String::new();
        for part in parts {
            signed.push_str(part);
        }
        let signature = sha1_hex(signed.as_bytes());
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-wechatwork-timestamp",
            HeaderValue::from_str(timestamp).expect("timestamp"),
        );
        headers.insert("x-wechatwork-nonce", HeaderValue::from_static(nonce));
        headers.insert(
            "x-wechatwork-msg-signature",
            HeaderValue::from_str(&signature).expect("signature"),
        );
        headers
    }

    fn encrypt_msg(message: &str) -> String {
        let key = decode_encoding_aes_key(TEST_AES_KEY).expect("key");
        let mut plaintext = vec![1u8; 16];
        plaintext.extend_from_slice(&(message.len() as u32).to_be_bytes());
        plaintext.extend_from_slice(message.as_bytes());
        plaintext.extend_from_slice(b"corp");
        let len = plaintext.len();
        plaintext.resize(len + 16, 0);
        let encrypted = Aes256CbcEnc::new((&key).into(), (&key[..16]).into())
            .encrypt_padded_mut::<Pkcs7>(&mut plaintext, len)
            .expect("encrypt");
        general_purpose::STANDARD.encode(encrypted)
    }

    #[test]
    fn verifies_signature_and_parses_text_event() {
        let raw = r#"{"MsgId":"m1","MsgType":"text","FromUserName":"u1","Content":"/help"}"#;
        let provider = WeChatWorkProvider;
        provider
            .verify(&signed_headers(raw), raw.as_bytes(), &integration())
            .expect("verify");
        let event = provider
            .parse_event(raw.as_bytes(), &integration(), 42)
            .expect("parse");
        assert_eq!(event.provider, MessagingProviderKind::WeChatWork);
        assert_eq!(event.event_id, "m1");
        assert_eq!(event.conversation_id, "u1");
        assert_eq!(event.text, "/help");
    }

    #[test]
    fn encrypted_url_challenge_decrypts_to_plaintext() {
        let provider = WeChatWorkProvider;
        let encrypted = encrypt_msg("challenge-ok");
        let mut headers = signed_headers(&encrypted);
        headers.insert(
            "x-wechatwork-echostr",
            HeaderValue::from_str(&encrypted).expect("challenge"),
        );
        let verified = provider
            .verify(&headers, b"", &integration())
            .expect("verify challenge");
        assert_eq!(
            verified,
            Verification::UrlVerification {
                challenge: "challenge-ok".to_string()
            }
        );
    }

    #[test]
    fn encrypted_xml_event_decrypts_and_normalizes_text() {
        let plain = r#"<xml><MsgId>m2</MsgId><MsgType><![CDATA[text]]></MsgType><FromUserName><![CDATA[u1]]></FromUserName><ToUserName><![CDATA[1000002]]></ToUserName><Content><![CDATA[/help]]></Content></xml>"#;
        let encrypted = encrypt_msg(plain);
        let raw = format!(r#"<xml><Encrypt><![CDATA[{encrypted}]]></Encrypt></xml>"#);
        let provider = WeChatWorkProvider;
        provider
            .verify(&signed_headers(&encrypted), raw.as_bytes(), &integration())
            .expect("verify encrypted event");
        let event = provider
            .parse_event(raw.as_bytes(), &integration(), 42)
            .expect("parse encrypted event");
        assert_eq!(event.event_id, "m2");
        assert_eq!(event.conversation_id, "u1");
        assert_eq!(event.text, "/help");
        assert!(event.mentioned_bot);
    }

    #[test]
    fn rejects_bad_signature_and_invalid_encrypted_payload() {
        let provider = WeChatWorkProvider;
        let raw = r#"{"MsgId":"m1"}"#;
        assert!(provider
            .verify(&HeaderMap::new(), raw.as_bytes(), &integration())
            .is_err());
        assert!(provider
            .parse_event(br#"{"Encrypt":"secret"}"#, &integration(), 1)
            .is_err());
    }

    #[test]
    fn rejects_expired_timestamp_even_with_valid_signature() {
        let raw = r#"{"MsgId":"m1","MsgType":"text","FromUserName":"u1","Content":"/help"}"#;
        let provider = WeChatWorkProvider;
        let old = crate::messaging::store::now_epoch()
            .saturating_sub(600)
            .to_string();
        let err = provider
            .verify(
                &signed_headers_at(raw, &old),
                raw.as_bytes(),
                &integration(),
            )
            .expect_err("expired timestamp must be rejected");
        assert!(err.message.contains("timestamp 已过期"), "{}", err.message);
    }

    #[test]
    fn raw_summary_redacts_secret_like_fields() {
        let summary =
            redact_raw_summary(br#"{"corpSecret":"s","access_token":"t","msg_signature":"sig"}"#);
        assert!(!summary.contains("\"s\""));
        assert!(!summary.contains("\"t\""));
        assert!(!summary.contains("\":\"sig\""));
        assert!(summary.contains("[redacted]"));
    }
}
