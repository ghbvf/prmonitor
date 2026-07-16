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
    ActionExecutionResult, MessagingCardTemplate, MessagingEvent, MessagingProviderCapability,
    MessagingProviderKind, MessagingReplyTarget, MessagingSendContent,
};

pub struct FeishuProvider;

const MAX_SIGNATURE_AGE_SECS: i64 = 5 * 60;
const FEISHU_BASE_URL: &str = "https://open.feishu.cn";
pub(crate) const HUMAN_INPUT_FORM_SUBMIT: &str = "human_input_submit";
pub(crate) const HUMAN_INPUT_CUSTOM_OPTION: &str = "custom";

pub(crate) fn human_input_choice_field_name(index: usize) -> String {
    format!("q_{index}_choice")
}

pub(crate) fn human_input_custom_field_name(index: usize) -> String {
    format!("q_{index}_custom")
}

pub(crate) fn human_input_option_value(index: usize) -> String {
    format!("o_{index}")
}

pub(crate) fn build_feishu_http_client(
    integration: &MessagingIntegration,
) -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(
            integration.timeout_secs.clamp(1, 300),
        ))
        .build()
        .map_err(|error| AppError::new(format!("飞书 HTTP client 初始化失败: {error}")))
}

struct FeishuApiClient {
    client: reqwest::Client,
    token: String,
}

impl FeishuApiClient {
    async fn authenticate(integration: &MessagingIntegration) -> AppResult<Self> {
        let client = build_feishu_http_client(integration)?;
        let token = tenant_access_token(&client, integration).await?;
        Ok(Self { client, token })
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{FEISHU_BASE_URL}{path}"))
            .bearer_auth(&self.token)
    }

    fn patch(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .patch(format!("{FEISHU_BASE_URL}{path}"))
            .bearer_auth(&self.token)
    }
}

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
            supports_information_card: true,
            supports_long_connection: true,
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
        if !integration.verification_token.trim().is_empty()
            && !constant_time_eq(
                envelope.header.token.trim(),
                integration.verification_token.trim(),
            )
        {
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
        content: &'a MessagingSendContent,
    ) -> ProviderFuture<'a> {
        Box::pin(async move {
            match content {
                MessagingSendContent::Text { text } => {
                    send_message(integration, conversation_id, text).await
                }
                MessagingSendContent::Card {
                    title,
                    text,
                    template,
                } => {
                    send_information_card(integration, conversation_id, title, text, *template)
                        .await
                }
            }
        })
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
    let api = FeishuApiClient::authenticate(integration).await?;
    let resp = api
        .post(&format!(
            "/open-apis/im/v1/messages/{}/reply",
            target.message_id
        ))
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
    let api = FeishuApiClient::authenticate(integration).await?;
    let resp = api
        .post("/open-apis/im/v1/messages")
        .query(&[("receive_id_type", "chat_id")])
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

/// Sends a display-only Feishu card. It deliberately has no action/button elements.
pub(crate) async fn send_information_card(
    integration: &MessagingIntegration,
    conversation_id: &str,
    title: &str,
    text: &str,
    template: MessagingCardTemplate,
) -> AppResult<ActionExecutionResult> {
    let api = FeishuApiClient::authenticate(integration).await?;
    let resp = api
        .post("/open-apis/im/v1/messages")
        .query(&[("receive_id_type", "chat_id")])
        .json(&information_card_request(
            conversation_id,
            title,
            text,
            template,
        ))
        .send()
        .await
        .map_err(|e| AppError::new(format!("飞书发送信息卡片请求失败: {e}")))?;
    classify_feishu_response(resp, "飞书发送信息卡片").await
}

fn information_card_request(
    conversation_id: &str,
    title: &str,
    text: &str,
    template: MessagingCardTemplate,
) -> Value {
    let card = json!({
        "config": { "wide_screen_mode": true },
        "header": {
            "template": template.as_wire(),
            "title": { "tag": "plain_text", "content": title }
        },
        "elements": [{ "tag": "markdown", "content": text }]
    });
    json!({
        "receive_id": conversation_id,
        "msg_type": "interactive",
        "content": serde_json::to_string(&card).expect("information card serializes")
    })
}

/// Sends an interactive human-input card and returns Feishu's message id so the winner can close
/// the other channel's UI. The card action value carries only the durable request/question ids.
pub async fn send_human_input_card(
    integration: &MessagingIntegration,
    conversation_id: &str,
    request: &crate::messaging::human_input::HumanInputRequest,
) -> AppResult<String> {
    let api = FeishuApiClient::authenticate(integration).await?;
    let card = human_input_card(request, false);
    let resp = api.post("/open-apis/im/v1/messages")
        .query(&[("receive_id_type", "chat_id")])
        .json(&json!({"receive_id": conversation_id, "msg_type": "interactive", "content": serde_json::to_string(&card).expect("card serializes")}))
        .send().await.map_err(|e| AppError::new(format!("飞书发送问答卡片失败: {e}")))?;
    #[derive(Deserialize)]
    struct Response {
        code: i64,
        msg: String,
        data: Option<ResponseData>,
    }
    #[derive(Deserialize)]
    struct ResponseData {
        message_id: String,
    }
    let status = resp.status();
    if !status.is_success() {
        return Err(AppError::new(format!("飞书发送问答卡片 HTTP {status}")));
    }
    let body: Response = resp
        .json()
        .await
        .map_err(|e| AppError::new(format!("飞书问答卡片响应损坏: {e}")))?;
    if body.code != 0 {
        return Err(AppError::new(format!("飞书发送问答卡片失败: {}", body.msg)));
    }
    body.data
        .map(|data| data.message_id)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| AppError::new("飞书问答卡片响应缺少 message_id"))
}

pub async fn update_human_input_card(
    integration: &MessagingIntegration,
    message_id: &str,
    request: &crate::messaging::human_input::HumanInputRequest,
) -> AppResult<()> {
    let api = FeishuApiClient::authenticate(integration).await?;
    let resp = api.patch(&format!("/open-apis/im/v1/messages/{message_id}"))
        .json(&json!({"content": serde_json::to_string(&human_input_card(request, true)).expect("card serializes")}))
        .send().await.map_err(|e| AppError::new(format!("飞书更新问答卡片失败: {e}")))?;
    let status = resp.status();
    let body = resp
        .bytes()
        .await
        .map_err(|e| AppError::new(format!("飞书更新问答卡片响应读取失败: {e}")))?;
    validate_card_update_response(status, &body)
}

fn validate_card_update_response(status: reqwest::StatusCode, raw: &[u8]) -> AppResult<()> {
    if !status.is_success() {
        return Err(AppError::new(format!("飞书更新问答卡片 HTTP {status}")));
    }
    #[derive(Deserialize)]
    struct Response {
        code: i64,
        #[serde(default)]
        msg: String,
    }
    let body: Response = serde_json::from_slice(raw)
        .map_err(|e| AppError::new(format!("飞书更新问答卡片响应损坏: {e}")))?;
    if body.code != 0 {
        return Err(AppError::new(format!(
            "飞书更新问答卡片失败（{}）：{}",
            body.code, body.msg
        )));
    }
    Ok(())
}

fn human_input_card(
    request: &crate::messaging::human_input::HumanInputRequest,
    terminal: bool,
) -> Value {
    let mut elements = vec![
        json!({"tag":"markdown", "content": format!("{}\n\n`{}`", request.message, request.id)}),
    ];
    if terminal {
        elements.push(json!({"tag":"note", "elements":[{"tag":"plain_text", "content": format!("已结束：{}", request.answer_source.as_deref().unwrap_or(&request.status))}]}));
    } else {
        elements.push(human_input_form(request));
        elements.push(json!({"tag":"note", "elements":[{"tag":"plain_text", "content": "也可回复 /answer <答案> 作答"}]}));
        elements.push(cancel_human_input_action(request));
    }
    json!({"config":{"wide_screen_mode":true,"update_multi":true}, "header":{"template": if terminal {"grey"} else {"blue"}, "title":{"tag":"plain_text","content":request.title}}, "elements":elements})
}

fn human_input_form(request: &crate::messaging::human_input::HumanInputRequest) -> Value {
    let mut elements = Vec::with_capacity(request.questions.len() * 3 + 1);
    for (index, question) in request.questions.iter().enumerate() {
        elements.push(json!({
            "tag": "markdown",
            "content": format!("**{}**", question.question)
        }));
        if question.options.is_empty() {
            elements.push(json!({
                "tag": "input",
                "name": human_input_custom_field_name(index),
                "required": true,
                "placeholder": {"tag": "plain_text", "content": "请输入答案"}
            }));
        } else {
            let mut options = question
                .options
                .iter()
                .enumerate()
                .map(|(option_index, option)| {
                    json!({
                        "text": {"tag": "plain_text", "content": option},
                        "value": human_input_option_value(option_index)
                    })
                })
                .collect::<Vec<_>>();
            options.push(json!({
                "text": {"tag": "plain_text", "content": "自定义输入"},
                "value": HUMAN_INPUT_CUSTOM_OPTION
            }));
            elements.push(json!({
                "tag": "select_static",
                "name": human_input_choice_field_name(index),
                "required": true,
                "placeholder": {"tag": "plain_text", "content": "请选择"},
                "options": options
            }));
            elements.push(json!({
                "tag": "input",
                "name": human_input_custom_field_name(index),
                "required": false,
                "placeholder": {"tag": "plain_text", "content": "选择“自定义输入”时填写"}
            }));
        }
    }
    elements.push(json!({
        "tag": "button",
        "name": HUMAN_INPUT_FORM_SUBMIT,
        "text": {"tag": "plain_text", "content": "提交答案"},
        "type": "primary",
        "action_type": "form_submit",
        "value": {"requestId": request.id, "action": "submit"}
    }));
    json!({
        "tag": "form",
        "name": "human_input_form",
        "elements": elements
    })
}

fn cancel_human_input_action(request: &crate::messaging::human_input::HumanInputRequest) -> Value {
    json!({"tag":"action", "actions":[{"tag":"button","text":{"tag":"plain_text","content":"取消"},"type":"danger","value":{"requestId":request.id,"questionId":"__cancel__","answer":"__cancel__"}}]})
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
        .post(format!(
            "{FEISHU_BASE_URL}/open-apis/auth/v3/tenant_access_token/internal"
        ))
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
    use crate::messaging::human_input::{
        HumanAnswerSource, HumanInputRequest, HumanInputStatus, HumanQuestion,
    };
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

    fn human_request(status: HumanInputStatus) -> HumanInputRequest {
        HumanInputRequest {
            id: "Q-card".to_string(),
            integration_id: "fs".to_string(),
            conversation_id: "chat-a".to_string(),
            purpose: "approval".to_string(),
            title: "需要确认".to_string(),
            message: "请选择".to_string(),
            questions: vec![HumanQuestion {
                id: "decision".to_string(),
                question: "是否继续？".to_string(),
                options: vec!["继续".to_string(), "取消".to_string()],
            }],
            context: json!({}),
            status,
            answer: None,
            answer_source: (status != HumanInputStatus::Pending)
                .then_some(HumanAnswerSource::Feishu),
            card_message_id: Some("om-card".to_string()),
            created_at_epoch: 1,
            expires_at_epoch: 2,
            answered_at_epoch: (status != HumanInputStatus::Pending).then_some(2),
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

    #[test]
    fn human_input_cards_enable_shared_updates_and_terminal_card_has_no_actions() {
        let pending = human_input_card(&human_request(HumanInputStatus::Pending), false);
        assert_eq!(pending.pointer("/config/update_multi"), Some(&json!(true)));
        assert!(pending["elements"]
            .as_array()
            .expect("pending elements")
            .iter()
            .any(|element| element["tag"] == "action"));

        let terminal = human_input_card(&human_request(HumanInputStatus::Answered), true);
        assert_eq!(terminal.pointer("/config/update_multi"), Some(&json!(true)));
        assert_eq!(terminal.pointer("/header/template"), Some(&json!("grey")));
        assert!(terminal["elements"]
            .as_array()
            .expect("terminal elements")
            .iter()
            .all(|element| element["tag"] != "action"));
    }

    #[test]
    fn multi_question_card_renders_required_form_controls_and_submit() {
        let mut request = human_request(HumanInputStatus::Pending);
        request.questions.push(HumanQuestion {
            id: "scope".to_string(),
            question: "发现边界放在哪里？".to_string(),
            options: vec!["当前任务".to_string(), "后续任务".to_string()],
        });

        let card = human_input_card(&request, false);
        let form = card["elements"]
            .as_array()
            .expect("card elements")
            .iter()
            .find(|element| element["tag"] == "form")
            .expect("multi-question card must contain a form");
        let controls = form["elements"].as_array().expect("form elements");
        let selects = controls
            .iter()
            .filter(|element| element["tag"] == "select_static")
            .collect::<Vec<_>>();
        assert_eq!(selects.len(), 2);
        assert_eq!(selects[0]["name"], "q_0_choice");
        assert_eq!(selects[1]["name"], "q_1_choice");
        assert!(selects.iter().all(|element| element["required"] == true));
        for select in selects {
            let custom_option = select["options"]
                .as_array()
                .expect("select options")
                .last()
                .expect("forced custom option");
            assert_eq!(custom_option["text"]["content"], "自定义输入");
            assert_eq!(custom_option["value"], "custom");
        }
        let inputs = controls
            .iter()
            .filter(|element| element["tag"] == "input")
            .collect::<Vec<_>>();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0]["name"], "q_0_custom");
        assert_eq!(inputs[1]["name"], "q_1_custom");
        assert!(inputs.iter().all(|element| element["required"] == false));
        let submit = controls
            .iter()
            .find(|element| element["action_type"] == "form_submit")
            .expect("form submit button");
        assert_eq!(submit["tag"], "button");
        assert_eq!(submit["name"], "human_input_submit");
        assert_eq!(submit["value"]["requestId"], request.id);
        assert!(serde_json::to_string(&card)
            .expect("serialize card")
            .contains("回复 /answer"));
    }

    #[test]
    fn empty_options_question_renders_required_free_text_input() {
        let mut request = human_request(HumanInputStatus::Pending);
        request.questions = vec![HumanQuestion {
            id: "open".to_string(),
            question: "还有补充吗？".to_string(),
            options: vec![],
        }];
        let card = human_input_card(&request, false);
        let form = card["elements"]
            .as_array()
            .expect("card elements")
            .iter()
            .find(|element| element["tag"] == "form")
            .expect("empty-options card must contain a form");
        let controls = form["elements"].as_array().expect("form elements");
        assert!(controls
            .iter()
            .all(|element| element["tag"] != "select_static"));
        let input = controls
            .iter()
            .find(|element| element["tag"] == "input")
            .expect("empty-options question uses a required input");
        assert_eq!(input["name"], "q_0_custom");
        assert_eq!(input["required"], true);
    }

    #[test]
    fn information_card_is_interactive_markdown_without_actions() {
        let request = information_card_request(
            "oc_123",
            "Task stopped",
            "**status:** done",
            MessagingCardTemplate::Grey,
        );
        assert_eq!(request["receive_id"], "oc_123");
        assert_eq!(request["msg_type"], "interactive");
        let card: Value =
            serde_json::from_str(request["content"].as_str().expect("content string"))
                .expect("card content");
        assert_eq!(card["header"]["template"], "grey");
        assert_eq!(card["header"]["title"]["content"], "Task stopped");
        assert_eq!(card["elements"][0]["tag"], "markdown");
        assert_eq!(card["elements"][0]["content"], "**status:** done");
        let serialized = serde_json::to_string(&card).expect("serialize card");
        assert!(!serialized.contains("action"));
        assert!(!serialized.contains("button"));
    }

    #[test]
    fn card_update_checks_feishu_business_code() {
        validate_card_update_response(reqwest::StatusCode::OK, br#"{"code":0,"msg":"ok"}"#)
            .expect("successful business response");

        let error = validate_card_update_response(
            reqwest::StatusCode::OK,
            br#"{"code":230020,"msg":"card cannot be updated"}"#,
        )
        .expect_err("non-zero business code rejected");
        assert!(error.to_string().contains("230020"));

        assert!(validate_card_update_response(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            br#"{"code":0,"msg":"ok"}"#,
        )
        .is_err());
        assert!(validate_card_update_response(reqwest::StatusCode::OK, b"not-json").is_err());
    }
}
