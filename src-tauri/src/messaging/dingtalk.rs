//! DingTalk messaging provider — Stream ingress + OpenAPI egress.

use axum::http::HeaderMap;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::service::MessagingIntegration;
use crate::error::{AppError, AppResult};
use crate::messaging::human_input::HumanInputRequest;
use crate::messaging::provider::{MessagingProvider, ProviderFuture, Verification};
use crate::messaging::redact_raw_summary;
use crate::model::{
    ActionExecutionResult, MessagingCardTemplate, MessagingEvent, MessagingProviderCapability,
    MessagingProviderKind, MessagingReplyTarget, MessagingSendContent,
};

pub struct DingTalkProvider;

const DINGTALK_API_HOST: &str = "https://api.dingtalk.com";

pub(crate) fn build_dingtalk_http_client(
    integration: &MessagingIntegration,
) -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(
            integration.timeout_secs.clamp(1, 300),
        ))
        .build()
        .map_err(|e| AppError::new(format!("钉钉 HTTP client 初始化失败: {e}")))
}

struct DingTalkApiClient {
    client: reqwest::Client,
    token: String,
}

impl DingTalkApiClient {
    async fn authenticate(integration: &MessagingIntegration) -> AppResult<Self> {
        let client = build_dingtalk_http_client(integration)?;
        let token = access_token(&client, integration).await?;
        Ok(Self { client, token })
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{DINGTALK_API_HOST}{path}"))
            .header("x-acs-dingtalk-access-token", &self.token)
    }

    fn put(&self, path: &str) -> reqwest::RequestBuilder {
        self.client
            .put(format!("{DINGTALK_API_HOST}{path}"))
            .header("x-acs-dingtalk-access-token", &self.token)
    }
}

async fn access_token(
    client: &reqwest::Client,
    integration: &MessagingIntegration,
) -> AppResult<String> {
    #[derive(Deserialize)]
    struct TokenResp {
        #[serde(rename = "accessToken")]
        access_token: Option<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        code: Option<String>,
    }
    let resp = client
        .post(format!("{DINGTALK_API_HOST}/v1.0/oauth2/accessToken"))
        .json(&json!({
            "appKey": integration.app_id.trim(),
            "appSecret": integration.app_secret.trim(),
        }))
        .send()
        .await
        .map_err(|e| AppError::new(format!("钉钉 accessToken 请求失败: {}", e.without_url())))?;
    let status = resp.status();
    let body: TokenResp = resp
        .json()
        .await
        .map_err(|e| AppError::new(format!("钉钉 accessToken 响应损坏: {e}")))?;
    if !status.is_success() {
        return Err(AppError::new(format!(
            "钉钉 accessToken HTTP {status}: {}",
            body.message.unwrap_or_default()
        )));
    }
    body.access_token.filter(|t| !t.is_empty()).ok_or_else(|| {
        AppError::new(format!(
            "钉钉 accessToken 为空（{}）",
            body.code.unwrap_or_default()
        ))
    })
}

impl MessagingProvider for DingTalkProvider {
    fn kind(&self) -> MessagingProviderKind {
        MessagingProviderKind::DingTalk
    }

    fn capability(&self) -> MessagingProviderCapability {
        MessagingProviderCapability {
            provider: MessagingProviderKind::DingTalk,
            supports_reply: true,
            supports_send: true,
            supports_information_card: true,
            supports_long_connection: true,
            requires_allowed_conversations: true,
        }
    }

    fn verify(
        &self,
        _headers: &HeaderMap,
        _raw: &[u8],
        _integration: &MessagingIntegration,
    ) -> AppResult<Verification> {
        Err(AppError::new(
            "钉钉 HTTP 回调已禁用；请在钉钉开放平台启用 Stream 模式",
        ))
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
        Box::pin(async move { send_text(integration, &target.conversation_id, text).await })
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
                    send_text(integration, conversation_id, text).await
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

async fn send_text(
    integration: &MessagingIntegration,
    conversation_id: &str,
    text: &str,
) -> AppResult<ActionExecutionResult> {
    let api = DingTalkApiClient::authenticate(integration).await?;
    let resp = api
        .post("/v1.0/robot/groupMessages/send")
        .json(&json!({
            "robotCode": integration.bot_open_id.trim(),
            "openConversationId": conversation_id.trim(),
            "msgKey": "sampleText",
            "msgParam": serde_json::to_string(&json!({ "content": text }))
                .expect("sampleText param serializes"),
        }))
        .send()
        .await
        .map_err(|e| AppError::new(format!("钉钉发送消息请求失败: {}", e.without_url())))?;
    classify_dingtalk_open_api(resp, "钉钉发送消息").await
}

/// Display-only information card via robot `sampleMarkdown` (no buttons / callbacks).
pub(crate) async fn send_information_card(
    integration: &MessagingIntegration,
    conversation_id: &str,
    title: &str,
    text: &str,
    template: MessagingCardTemplate,
) -> AppResult<ActionExecutionResult> {
    let api = DingTalkApiClient::authenticate(integration).await?;
    let resp = api
        .post("/v1.0/robot/groupMessages/send")
        .json(&information_card_request(
            integration,
            conversation_id,
            title,
            text,
            template,
        ))
        .send()
        .await
        .map_err(|e| AppError::new(format!("钉钉发送信息卡片请求失败: {}", e.without_url())))?;
    classify_dingtalk_open_api(resp, "钉钉发送信息卡片").await
}

pub(crate) fn information_card_request(
    integration: &MessagingIntegration,
    conversation_id: &str,
    title: &str,
    text: &str,
    template: MessagingCardTemplate,
) -> Value {
    let titled = format!("[{}] {}", template.as_wire(), title);
    json!({
        "robotCode": integration.bot_open_id.trim(),
        "openConversationId": conversation_id.trim(),
        "msgKey": "sampleMarkdown",
        "msgParam": serde_json::to_string(&json!({
            "title": titled,
            "text": text,
        }))
        .expect("sampleMarkdown param serializes"),
    })
}

/// Creates and delivers an interactive human-input card with Stream callbacks.
pub async fn send_human_input_card(
    integration: &MessagingIntegration,
    conversation_id: &str,
    request: &HumanInputRequest,
) -> AppResult<String> {
    let api = DingTalkApiClient::authenticate(integration).await?;
    let body = human_input_create_and_deliver_request(integration, conversation_id, request);
    let resp = api
        .post("/v1.0/card/instances/createAndDeliver")
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::new(format!("钉钉发送问答卡片失败: {}", e.without_url())))?;
    let status = resp.status();
    if !status.is_success() {
        let detail = resp.text().await.unwrap_or_default();
        return Err(AppError::new(format!(
            "钉钉发送问答卡片 HTTP {status}: {}",
            truncate_err(&detail)
        )));
    }
    Ok(request.id.clone())
}

pub async fn update_human_input_card(
    integration: &MessagingIntegration,
    out_track_id: &str,
    request: &HumanInputRequest,
) -> AppResult<()> {
    let api = DingTalkApiClient::authenticate(integration).await?;
    let resp = api
        .put("/v1.0/card/instances")
        .json(&human_input_update_request(out_track_id, request))
        .send()
        .await
        .map_err(|e| AppError::new(format!("钉钉更新问答卡片失败: {}", e.without_url())))?;
    let status = resp.status();
    if !status.is_success() {
        let detail = resp.text().await.unwrap_or_default();
        return Err(AppError::new(format!(
            "钉钉更新问答卡片 HTTP {status}: {}",
            truncate_err(&detail)
        )));
    }
    Ok(())
}

pub(crate) fn human_input_create_and_deliver_request(
    integration: &MessagingIntegration,
    conversation_id: &str,
    request: &HumanInputRequest,
) -> Value {
    json!({
        "cardTemplateId": integration.card_template_id.trim(),
        "outTrackId": request.id,
        "callbackType": "STREAM",
        "cardData": {
            "cardParamMap": human_input_card_params(request, false),
        },
        "openSpaceId": format!("dtv1.card//IM_GROUP.{}", conversation_id.trim()),
        "imGroupOpenSpaceModel": {
            "supportForward": false,
        },
        "imGroupOpenDeliverModel": {
            "robotCode": integration.bot_open_id.trim(),
        },
        "userIdType": 1,
    })
}

fn human_input_update_request(out_track_id: &str, request: &HumanInputRequest) -> Value {
    json!({
        "outTrackId": out_track_id,
        "cardData": {
            "cardParamMap": human_input_card_params(request, true),
        },
    })
}

fn human_input_card_params(request: &HumanInputRequest, terminal: bool) -> Value {
    let options = request
        .questions
        .iter()
        .flat_map(|q| q.options.iter().cloned())
        .collect::<Vec<_>>()
        .join(" / ");
    let status = if terminal {
        format!(
            "已结束：{}",
            request
                .answer_source
                .as_deref()
                .unwrap_or(request.status.as_str())
        )
    } else {
        "pending".to_string()
    };
    json!({
        "title": request.title,
        "message": request.message,
        "requestId": request.id,
        "options": options,
        "status": status,
    })
}

async fn classify_dingtalk_open_api(
    resp: reqwest::Response,
    op: &str,
) -> AppResult<ActionExecutionResult> {
    let status = resp.status();
    if status.as_u16() == 429 || status.is_server_error() {
        return Ok(ActionExecutionResult::Retry {
            message: format!("{op} 暂时失败: HTTP {status}"),
            retry_after_secs: None,
        });
    }
    if !status.is_success() {
        let detail = resp.text().await.unwrap_or_default();
        return Ok(ActionExecutionResult::Dead {
            message: format!(
                "{op} 不可重试失败: HTTP {status}: {}",
                truncate_err(&detail)
            ),
        });
    }
    Ok(ActionExecutionResult::done())
}

fn truncate_err(value: &str) -> String {
    crate::messaging::truncate_utf8_boundary(value, 240)
}

fn stable_event_id(raw: &[u8]) -> String {
    use sha2::{Digest, Sha256};
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
        // DingTalk sets isInAtList when the bot itself is @-mentioned. Non-empty
        // atUsers alone means someone else was mentioned — must not open the gate.
        let _ = &self.at_users;
        self.is_in_at_list.unwrap_or(false)
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
    use crate::messaging::human_input::HumanInputStatus;

    fn integration() -> MessagingIntegration {
        MessagingIntegration {
            id: "dt".to_string(),
            app_id: "ding-app-key".to_string(),
            app_secret: "secret-123456".to_string(),
            bot_open_id: "robot-code".to_string(),
            card_template_id: "tpl-human".to_string(),
            allowed_conversation_ids: vec!["cid".to_string()],
            ..MessagingIntegration::feishu_default()
        }
    }

    #[test]
    fn http_verify_is_disabled() {
        let err = DingTalkProvider
            .verify(&HeaderMap::new(), b"{}", &integration())
            .expect_err("HTTP verify must fail closed");
        assert!(err.message.contains("HTTP 回调已禁用"), "{}", err.message);
    }

    #[test]
    fn parses_text_event() {
        let raw = br#"{"msgId":"m1","msgtype":"text","conversationId":"cid","senderId":"u","text":{"content":"/help"},"isInAtList":true}"#;
        let event = DingTalkProvider
            .parse_event(raw, &integration(), 42)
            .expect("parse");
        assert_eq!(event.provider, MessagingProviderKind::DingTalk);
        assert_eq!(event.event_id, "m1");
        assert_eq!(event.conversation_id, "cid");
        assert_eq!(event.text, "/help");
        assert!(event.mentioned_bot);
    }

    #[test]
    fn mentioned_bot_ignores_at_users_when_bot_not_in_list() {
        let raw = br#"{"msgId":"m1","msgtype":"text","conversationId":"cid","senderId":"u","text":{"content":"/help"},"isInAtList":false,"atUsers":[{"dingtalkId":"other"}]}"#;
        let event = DingTalkProvider
            .parse_event(raw, &integration(), 42)
            .expect("parse");
        assert!(!event.mentioned_bot);
    }

    #[test]
    fn capability_supports_long_connection() {
        assert!(DingTalkProvider.capability().supports_long_connection);
        assert!(DingTalkProvider.capability().supports_information_card);
    }

    #[test]
    fn rejects_non_text_msgtype() {
        let raw = br#"{"msgId":"m2","msgtype":"image","conversationId":"cid","senderId":"u","text":{"content":"not text"}}"#;
        let err = DingTalkProvider
            .parse_event(raw, &integration(), 42)
            .expect_err("non-text");
        assert!(err.message.contains("不是文本消息"), "{}", err.message);
    }

    #[test]
    fn information_card_embeds_template_prefix_in_title() {
        let body = information_card_request(
            &integration(),
            "cid",
            "Task stopped",
            "**Status:** done",
            MessagingCardTemplate::Grey,
        );
        assert_eq!(body["msgKey"], "sampleMarkdown");
        let param: Value = serde_json::from_str(body["msgParam"].as_str().unwrap()).unwrap();
        assert_eq!(param["title"], "[grey] Task stopped");
        assert_eq!(param["text"], "**Status:** done");
    }

    #[test]
    fn human_input_create_and_deliver_requires_stream_callback() {
        let request = HumanInputRequest {
            id: "Q-1".into(),
            integration_id: "dt".into(),
            conversation_id: "cid".into(),
            purpose: "ask".into(),
            title: "Choose".into(),
            message: "Pick one".into(),
            questions: vec![],
            context: Value::Null,
            status: HumanInputStatus::Pending,
            answer: None,
            answer_source: None,
            card_message_id: None,
            created_at_epoch: 1,
            expires_at_epoch: 2,
            answered_at_epoch: None,
        };
        let body = human_input_create_and_deliver_request(&integration(), "cid", &request);
        assert_eq!(body["callbackType"], "STREAM");
        assert_eq!(body["cardTemplateId"], "tpl-human");
        assert_eq!(body["outTrackId"], "Q-1");
        let forbidden = format!("{}{}", "oapi.dingtalk.com", "/robot/send");
        assert!(!serde_json::to_string(&body).unwrap().contains(&forbidden));
    }

    #[test]
    fn messaging_tree_does_not_resurrect_robot_send_webhook() {
        use std::fs;
        use std::path::Path;

        // Split so this test file does not contain the forbidden contiguous literal.
        let forbidden = format!("{}{}", "oapi.dingtalk.com", "/robot/send");
        fn scan(dir: &Path, needle: &str, offenders: &mut Vec<String>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    scan(&path, needle, offenders);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                for (i, line) in text.lines().enumerate() {
                    let code = line.split("//").next().unwrap_or(line);
                    if code.contains(needle) {
                        offenders.push(format!("{}:{}", path.display(), i + 1));
                    }
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/messaging");
        let mut offenders = Vec::new();
        scan(&root, &forbidden, &mut offenders);
        assert!(
            offenders.is_empty(),
            "DingTalk webhook robot/send must stay deleted: {offenders:?}"
        );
    }
}
