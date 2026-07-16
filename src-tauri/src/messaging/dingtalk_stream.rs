//! DingTalk Stream Mode long connection.
//!
//! Protocol adapted from open-dingtalk/dingtalk-stream-sdk-go: bootstrap
//! `POST /v1.0/gateway/connections/open`, connect `wss endpoint?ticket=`, speak JSON
//! DataFrames, ACK each callback, ping/pong, reconnect with backoff.
//! Lifecycle fencing / fingerprint / stage timeout share
//! [`crate::messaging::long_connection`] with Feishu (no LongConnection trait).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::Manager;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::config::service::MessagingIntegration;
use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::messaging::dingtalk::{build_dingtalk_http_client, DingTalkProvider};
use crate::messaging::human_input::{
    self, AnswerOutcome, HumanAnswer, HumanAnswerSource, HumanInputBroker,
};
use crate::messaging::long_connection::{
    abort_join_wait, await_connection_stage, cancel_abort_task, integration_fingerprint,
    lock_current_generation, next_generation, reconnect_delay, sanitize_ws_error, update_current,
};
use crate::messaging::provider::MessagingProvider;
use crate::messaging::{service, store};
use crate::model::{MessagingConnectionState, MessagingConnectionStatus, MessagingProviderKind};

const BOOTSTRAP_URL: &str = "https://api.dingtalk.com/v1.0/gateway/connections/open";
const BOT_TOPIC: &str = "/v1.0/im/bot/messages/get";
const CARD_TOPIC: &str = "/v1.0/card/instances/callback";
const MAX_WS_BYTES: usize = 1024 * 1024;
const KEEP_ALIVE_IDLE: Duration = Duration::from_secs(120);

#[derive(Default)]
pub struct DingTalkConnectionManager {
    tasks: Mutex<HashMap<String, ConnectionTask>>,
    statuses: Arc<Mutex<HashMap<String, MessagingConnectionStatus>>>,
    generations: Arc<Mutex<HashMap<String, u64>>>,
}

struct ConnectionTask {
    fingerprint: [u8; 32],
    cancel: CancellationToken,
    task: tauri::async_runtime::JoinHandle<()>,
}

impl DingTalkConnectionManager {
    pub fn reconcile(
        &self,
        app: &tauri::AppHandle<tauri::Wry>,
        integrations: &[MessagingIntegration],
    ) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
        let desired = integrations
            .iter()
            .filter(|item| item.kind == MessagingProviderKind::DingTalk && item.enabled)
            .filter_map(|item| match integration_fingerprint(item) {
                Ok(fingerprint) => Some((item.id.clone(), fingerprint)),
                Err(error) => {
                    eprintln!("钉钉 Stream 配置 fingerprint 失败：{}", error.message);
                    None
                }
            })
            .collect::<HashMap<String, [u8; 32]>>();
        let obsolete = tasks
            .iter()
            .filter(|(id, task)| desired.get(*id) != Some(&task.fingerprint))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut aborted = Vec::new();
        for id in obsolete {
            if let Some(task) = tasks.remove(&id) {
                next_generation(&self.generations, &id);
                cancel_abort_task(task.cancel, task.task, &mut aborted);
            }
        }
        let mut statuses = self.statuses.lock().unwrap_or_else(|p| p.into_inner());
        statuses.retain(|id, _| integrations.iter().any(|item| item.id == *id));
        for integration in integrations
            .iter()
            .filter(|item| item.kind == MessagingProviderKind::DingTalk)
        {
            if !integration.enabled {
                if let Some(task) = tasks.remove(&integration.id) {
                    next_generation(&self.generations, &integration.id);
                    cancel_abort_task(task.cancel, task.task, &mut aborted);
                }
                statuses.insert(
                    integration.id.clone(),
                    status(integration, MessagingConnectionState::Disabled),
                );
            }
        }
        // Join aborted tasks before bootstrap so the same app_id cannot dual-connect.
        abort_join_wait(aborted);
        for integration in integrations
            .iter()
            .filter(|item| item.kind == MessagingProviderKind::DingTalk && item.enabled)
        {
            if tasks.contains_key(&integration.id) {
                continue;
            }
            let token = CancellationToken::new();
            let generation = next_generation(&self.generations, &integration.id);
            let Some(fingerprint) = desired.get(&integration.id).copied() else {
                statuses.insert(
                    integration.id.clone(),
                    status(integration, MessagingConnectionState::Error),
                );
                continue;
            };
            statuses.insert(
                integration.id.clone(),
                status(integration, MessagingConnectionState::Connecting),
            );
            let app = app.clone();
            let integration = integration.clone();
            let integration_id = integration.id.clone();
            let statuses = self.statuses.clone();
            let generations = self.generations.clone();
            let task_token = token.clone();
            let task = tauri::async_runtime::spawn(async move {
                run_supervisor(
                    app,
                    integration,
                    statuses,
                    generations,
                    generation,
                    task_token,
                )
                .await;
            });
            tasks.insert(
                integration_id,
                ConnectionTask {
                    fingerprint,
                    cancel: token,
                    task,
                },
            );
        }
    }

    pub fn stop(&self) {
        let ids = self
            .tasks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for id in ids {
            next_generation(&self.generations, &id);
        }
        for (_, task) in self.tasks.lock().unwrap_or_else(|p| p.into_inner()).drain() {
            task.cancel.cancel();
            task.task.abort();
        }
        for value in self
            .statuses
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values_mut()
        {
            value.status = MessagingConnectionState::Stopped;
        }
    }

    pub fn statuses(&self) -> Vec<MessagingConnectionStatus> {
        let mut values = self
            .statuses
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        values.sort_by(|a, b| a.integration_id.cmp(&b.integration_id));
        values
    }
}

fn status(
    integration: &MessagingIntegration,
    state: MessagingConnectionState,
) -> MessagingConnectionStatus {
    MessagingConnectionStatus {
        provider: MessagingProviderKind::DingTalk,
        integration_id: integration.id.clone(),
        status: state,
        last_connected_at_epoch: None,
        last_event_at_epoch: None,
        last_error: None,
        reconnect_count: 0,
    }
}

async fn run_supervisor(
    app: tauri::AppHandle<tauri::Wry>,
    integration: MessagingIntegration,
    statuses: Arc<Mutex<HashMap<String, MessagingConnectionStatus>>>,
    generations: Arc<Mutex<HashMap<String, u64>>>,
    generation: u64,
    cancel: CancellationToken,
) {
    let mut reconnect_count = 0u64;
    loop {
        if cancel.is_cancelled() {
            break;
        }
        update_current(
            &statuses,
            &generations,
            &integration.id,
            generation,
            |value| value.status = MessagingConnectionState::Connecting,
        );
        match connect_once(
            &app,
            &integration,
            &statuses,
            &generations,
            generation,
            &cancel,
        )
        .await
        {
            Ok(()) => {
                reconnect_count = 0;
            }
            Err(_error) if cancel.is_cancelled() => break,
            Err(error) => update_current(
                &statuses,
                &generations,
                &integration.id,
                generation,
                |value| {
                    value.status = MessagingConnectionState::Error;
                    value.last_error.replace(error.message);
                },
            ),
        }
        if cancel.is_cancelled() {
            break;
        }
        reconnect_count = reconnect_count.saturating_add(1);
        update_current(
            &statuses,
            &generations,
            &integration.id,
            generation,
            |value| {
                value.status = MessagingConnectionState::Reconnecting;
                value.reconnect_count = value.reconnect_count.saturating_add(1);
            },
        );
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(reconnect_delay(reconnect_count.saturating_sub(1))) => {}
        }
    }
    update_current(
        &statuses,
        &generations,
        &integration.id,
        generation,
        |value| value.status = MessagingConnectionState::Stopped,
    );
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionEndpointRequest<'a> {
    client_id: &'a str,
    client_secret: &'a str,
    ua: &'a str,
    subscriptions: Vec<SubscriptionModel>,
}

#[derive(Debug, Serialize, Clone)]
struct SubscriptionModel {
    #[serde(rename = "type")]
    kind: &'static str,
    topic: &'static str,
}

#[derive(Debug, Deserialize)]
struct ConnectionEndpointResponse {
    endpoint: String,
    ticket: String,
}

pub(crate) fn bootstrap_request_body(integration: &MessagingIntegration) -> Value {
    serde_json::to_value(ConnectionEndpointRequest {
        client_id: integration.app_id.trim(),
        client_secret: integration.app_secret.trim(),
        ua: "prmonitor-dingtalk-stream/1",
        subscriptions: vec![
            SubscriptionModel {
                kind: "SYSTEM",
                topic: "ping",
            },
            SubscriptionModel {
                kind: "SYSTEM",
                topic: "disconnect",
            },
            SubscriptionModel {
                kind: "CALLBACK",
                topic: BOT_TOPIC,
            },
            SubscriptionModel {
                kind: "CALLBACK",
                topic: CARD_TOPIC,
            },
        ],
    })
    .expect("bootstrap request serializes")
}

fn build_stream_url(endpoint: &str, ticket: &str) -> AppResult<String> {
    let mut parsed = url::Url::parse(endpoint.trim())
        .map_err(|e| AppError::new(format!("钉钉 Stream endpoint 非法: {e}")))?;
    if parsed.scheme() != "wss" {
        return Err(AppError::new("钉钉 Stream URL 必须是 wss"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(AppError::new("钉钉 Stream URL 不得含 userinfo"));
    }
    if parsed.host_str().unwrap_or_default().is_empty() {
        return Err(AppError::new("钉钉 Stream URL 缺少 host"));
    }
    {
        let mut pairs = parsed.query_pairs_mut();
        pairs.clear();
        pairs.append_pair("ticket", ticket);
    }
    Ok(parsed.to_string())
}

async fn bootstrap(integration: &MessagingIntegration) -> AppResult<String> {
    let client = build_dingtalk_http_client(integration)?;
    let response = client
        .post(BOOTSTRAP_URL)
        .json(&bootstrap_request_body(integration))
        .send()
        .await
        .map_err(|e| AppError::new(format!("钉钉 Stream bootstrap 失败: {}", e.without_url())))?;
    let status = response.status();
    if !status.is_success() {
        return Err(AppError::new(format!(
            "钉钉 Stream bootstrap HTTP {status}"
        )));
    }
    let body: ConnectionEndpointResponse = response
        .json()
        .await
        .map_err(|e| AppError::new(format!("钉钉 Stream bootstrap 响应损坏: {e}")))?;
    if body.endpoint.is_empty() || body.ticket.is_empty() {
        return Err(AppError::new("钉钉 Stream bootstrap 缺少 endpoint/ticket"));
    }
    build_stream_url(&body.endpoint, &body.ticket)
}

async fn connect_once(
    app: &tauri::AppHandle<tauri::Wry>,
    integration: &MessagingIntegration,
    statuses: &Arc<Mutex<HashMap<String, MessagingConnectionStatus>>>,
    generations: &Arc<Mutex<HashMap<String, u64>>>,
    generation: u64,
    cancel: &CancellationToken,
) -> AppResult<()> {
    let stage_timeout = Duration::from_secs(integration.timeout_secs.clamp(1, 30));
    let url =
        await_connection_stage(cancel, stage_timeout, "bootstrap", bootstrap(integration)).await?;
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_WS_BYTES))
        .max_frame_size(Some(MAX_WS_BYTES));
    let connect_url = url.clone();
    let (socket, _) = await_connection_stage(cancel, stage_timeout, "WebSocket 握手", async {
        tokio_tungstenite::connect_async_with_config(&connect_url, Some(ws_config), false)
            .await
            .map_err(|e| AppError::new(format!("钉钉 Stream {}", sanitize_ws_error(&e))))
    })
    .await?;
    update_current(
        statuses,
        generations,
        &integration.id,
        generation,
        |value| {
            value.status = MessagingConnectionState::Connected;
            value.last_connected_at_epoch = Some(store::now_epoch());
            value.last_error = None;
        },
    );
    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = sink.close().await;
                break;
            }
            _ = tokio::time::sleep(KEEP_ALIVE_IDLE) => {
                if let Err(e) = sink.send(Message::Ping(Vec::new().into())).await {
                    return Err(AppError::new(format!("钉钉 Stream ping 失败: {e}")));
                }
            }
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let (ack, business_error) = match handle_data_frame(
                            app,
                            integration,
                            text.as_bytes(),
                            generations,
                            generation,
                            cancel,
                        ) {
                            Ok(ack) => (ack, None),
                            Err(error) => {
                                let message_id = peek_message_id(text.as_bytes());
                                (error_ack(&message_id, &error.message), Some(error.message))
                            }
                        };
                        update_current(statuses, generations, &integration.id, generation, |value| {
                            value.last_event_at_epoch = Some(store::now_epoch());
                            if let Some(error) = business_error {
                                value.last_error = Some(error);
                            }
                        });
                        let payload = serde_json::to_string(&ack)
                            .map_err(|e| AppError::new(format!("钉钉 Stream ACK 序列化失败: {e}")))?;
                        sink.send(Message::Text(payload.into()))
                            .await
                            .map_err(|e| AppError::new(format!("钉钉 Stream ACK 发送失败: {e}")))?;
                        if ack.force_disconnect {
                            let _ = sink.close().await;
                            return Err(AppError::new("钉钉 Stream 收到 disconnect"));
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        sink.send(Message::Pong(data))
                            .await
                            .map_err(|e| AppError::new(format!("钉钉 Stream pong 失败: {e}")))?;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        return Err(AppError::new(format!("钉钉 Stream 读失败: {error}")));
                    }
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DataFrame {
    #[serde(default)]
    #[allow(dead_code)]
    spec_version: String,
    #[serde(rename = "type")]
    frame_type: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    data: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DataFrameResponse {
    code: i32,
    headers: HashMap<String, String>,
    message: String,
    data: String,
    #[serde(skip)]
    force_disconnect: bool,
}

fn success_ack(message_id: &str) -> DataFrameResponse {
    let mut headers = HashMap::new();
    headers.insert("messageId".into(), message_id.to_string());
    headers.insert("contentType".into(), "application/json".into());
    DataFrameResponse {
        code: 200,
        headers,
        message: String::new(),
        data: String::new(),
        force_disconnect: false,
    }
}

fn not_found_ack(message_id: &str) -> DataFrameResponse {
    let mut ack = success_ack(message_id);
    ack.code = 404;
    ack.message = "handler not found".into();
    ack
}

fn error_ack(message_id: &str, message: &str) -> DataFrameResponse {
    let mut ack = success_ack(message_id);
    ack.code = 500;
    ack.message = message.to_string();
    ack
}

fn peek_message_id(raw: &[u8]) -> String {
    serde_json::from_slice::<DataFrame>(raw)
        .ok()
        .and_then(|frame| frame.headers.get("messageId").cloned())
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn handle_data_frame_for_test<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    integration: &MessagingIntegration,
    raw: &[u8],
) -> AppResult<DataFrameResponse> {
    let generations = Arc::new(Mutex::new(HashMap::from([(integration.id.clone(), 1u64)])));
    let cancel = CancellationToken::new();
    handle_data_frame(app, integration, raw, &generations, 1, &cancel)
}

fn handle_data_frame<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    integration: &MessagingIntegration,
    raw: &[u8],
    generations: &Arc<Mutex<HashMap<String, u64>>>,
    generation: u64,
    cancel: &CancellationToken,
) -> AppResult<DataFrameResponse> {
    let frame: DataFrame = serde_json::from_slice(raw)
        .map_err(|e| AppError::new(format!("钉钉 Stream DataFrame 损坏: {e}")))?;
    let message_id = frame.headers.get("messageId").cloned().unwrap_or_default();
    let topic = frame.headers.get("topic").map(String::as_str).unwrap_or("");
    if frame.frame_type == "SYSTEM" && topic == "ping" {
        let mut ack = success_ack(&message_id);
        ack.data = frame.data;
        return Ok(ack);
    }
    if frame.frame_type == "SYSTEM" && topic == "disconnect" {
        let mut ack = success_ack(&message_id);
        ack.force_disconnect = true;
        return Ok(ack);
    }
    if frame.frame_type != "CALLBACK" {
        return Ok(not_found_ack(&message_id));
    }
    match topic {
        BOT_TOPIC => {
            let _guard = lock_current_generation(generations, &integration.id, generation, cancel)?;
            let event = DingTalkProvider.parse_event(
                frame.data.as_bytes(),
                integration,
                store::now_epoch(),
            )?;
            service::validate_current_dingtalk_long_connection(app, integration)?;
            service::persist_verified_long_connection_event(app, integration, &event)?;
            Ok(success_ack(&message_id))
        }
        CARD_TOPIC => {
            let _guard = lock_current_generation(generations, &integration.id, generation, cancel)?;
            service::validate_current_dingtalk_long_connection(app, integration)?;
            match handle_card_callback(app, integration, &frame.data)? {
                CardCallbackResult::Answered => Ok(card_callback_ack(&message_id)),
                CardCallbackResult::Neutral => Ok(success_ack(&message_id)),
            }
        }
        _ => Ok(not_found_ack(&message_id)),
    }
}

fn card_callback_ack(message_id: &str) -> DataFrameResponse {
    let mut ack = success_ack(message_id);
    ack.data = json!({
        "cardUpdateOptions": { "updateCardDataByKey": true },
        "cardData": { "cardParamMap": { "status": "answered" } },
    })
    .to_string();
    ack
}

enum CardCallbackResult {
    Answered,
    Neutral,
}

fn handle_card_callback<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    integration: &MessagingIntegration,
    data: &str,
) -> AppResult<CardCallbackResult> {
    let payload: Value = serde_json::from_str(data)
        .map_err(|e| AppError::new(format!("钉钉卡片回调 JSON 损坏: {e}")))?;
    let out_track_id = payload
        .get("outTrackId")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::new("钉钉卡片回调缺少 outTrackId"))?;
    let db = app.state::<Database>();
    let Some(request) = human_input::get(db.inner(), out_track_id)? else {
        return Ok(CardCallbackResult::Neutral);
    };
    if request.integration_id != integration.id {
        return Err(AppError::new("钉钉卡片回调 integration 不匹配"));
    }
    let current = crate::config::service::messaging_integration(app, &integration.id)?;
    if !current
        .allowed_conversation_ids
        .iter()
        .any(|id| id == &request.conversation_id)
    {
        return Err(AppError::new("钉钉卡片回调会话未授权"));
    }
    let answers = parse_card_answers(&payload, &request)?;
    if answers.is_empty() {
        return Ok(CardCallbackResult::Neutral);
    }
    let broker = app.state::<HumanInputBroker>();
    let outcome = human_input::answer(
        db.inner(),
        broker.inner(),
        out_track_id,
        &answers,
        HumanAnswerSource::DingTalk,
        store::now_epoch(),
    )?;
    match outcome {
        AnswerOutcome::Won | AnswerOutcome::AlreadyAnswered { .. } => {
            Ok(CardCallbackResult::Answered)
        }
    }
}

fn parse_card_answers(
    payload: &Value,
    request: &human_input::HumanInputRequest,
) -> AppResult<Vec<HumanAnswer>> {
    let content = payload
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let content_json: Value = serde_json::from_str(content).unwrap_or(Value::Null);
    let params = content_json
        .pointer("/cardPrivateData/params")
        .cloned()
        .or_else(|| payload.get("params").cloned())
        .unwrap_or(Value::Null);
    let mut answers = Vec::new();
    for question in &request.questions {
        let key = format!("answer_{}", question.id);
        if let Some(answer) = params.get(&key).and_then(Value::as_str) {
            answers.push(HumanAnswer {
                question_id: question.id.clone(),
                answer: answer.to_string(),
            });
            continue;
        }
        if let Some(answer) = params.get("answer").and_then(Value::as_str) {
            if request.questions.len() == 1 {
                answers.push(HumanAnswer {
                    question_id: question.id.clone(),
                    answer: answer.to_string(),
                });
            }
        }
    }
    Ok(answers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::human_input::{HumanInputRequest, HumanInputStatus, HumanQuestion};

    fn integration() -> MessagingIntegration {
        MessagingIntegration {
            id: "dingtalk-main".into(),
            kind: MessagingProviderKind::DingTalk,
            enabled: true,
            app_id: "app-key".into(),
            app_secret: "app-secret".into(),
            bot_open_id: "robot".into(),
            card_template_id: "tpl".into(),
            allowed_conversation_ids: vec!["cid".into()],
            ..MessagingIntegration::feishu_default()
        }
    }

    #[test]
    fn bootstrap_subscribes_bot_and_card_topics() {
        let body = bootstrap_request_body(&integration());
        let topics = body["subscriptions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item.get("topic").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert!(topics.contains(&BOT_TOPIC));
        assert!(topics.contains(&CARD_TOPIC));
        assert!(topics.contains(&"ping"));
        assert!(topics.contains(&"disconnect"));
    }

    #[test]
    fn stream_url_encodes_ticket_and_rejects_userinfo() {
        let url = build_stream_url("wss://example.com/stream", "tick et/+/=").unwrap();
        assert!(url.contains("ticket="));
        assert!(url.starts_with("wss://"));
        assert!(!url.contains("tick et"));
        assert!(build_stream_url("wss://user:pass@example.com/s", "t").is_err());
        assert!(build_stream_url("ws://example.com/s", "t").is_err());
    }

    #[test]
    fn fingerprint_includes_allowlist() {
        let mut item = integration();
        let first = crate::messaging::long_connection::integration_fingerprint(&item).unwrap();
        item.allowed_conversation_ids.push("other".into());
        assert_ne!(
            first,
            crate::messaging::long_connection::integration_fingerprint(&item).unwrap()
        );
    }

    #[test]
    fn unknown_topic_returns_404_ack() {
        let app = tauri::test::mock_app();
        let raw = json!({
            "specVersion": "1.0",
            "type": "CALLBACK",
            "headers": { "topic": "/unknown", "messageId": "m-1" },
            "data": "{}"
        });
        let ack = handle_data_frame_for_test(
            app.handle(),
            &integration(),
            serde_json::to_vec(&raw).unwrap().as_slice(),
        )
        .unwrap();
        assert_eq!(ack.code, 404);
    }

    #[test]
    fn ping_echoes_data() {
        let app = tauri::test::mock_app();
        let raw = json!({
            "type": "SYSTEM",
            "headers": { "topic": "ping", "messageId": "p1" },
            "data": "pong-body"
        });
        let ack = handle_data_frame_for_test(
            app.handle(),
            &integration(),
            serde_json::to_vec(&raw).unwrap().as_slice(),
        )
        .unwrap();
        assert_eq!(ack.code, 200);
        assert_eq!(ack.data, "pong-body");
    }

    #[test]
    fn disconnect_sets_force_flag() {
        let app = tauri::test::mock_app();
        let raw = json!({
            "type": "SYSTEM",
            "headers": { "topic": "disconnect", "messageId": "d1" },
            "data": ""
        });
        let ack = handle_data_frame_for_test(
            app.handle(),
            &integration(),
            serde_json::to_vec(&raw).unwrap().as_slice(),
        )
        .unwrap();
        assert!(ack.force_disconnect);
    }

    #[test]
    fn card_callback_answers_pending_request() {
        let app = tauri::test::mock_app();
        let db = Database::open_in_memory().expect("db");
        let broker = HumanInputBroker::default();
        let integration = integration();
        let mut config = crate::config::service::load_db(&db).expect("load");
        config.messaging.integrations = vec![integration.clone()];
        crate::config::service::persist_db(&db, &config).expect("persist");
        let now = store::now_epoch();
        human_input::create(
            &db,
            &HumanInputRequest {
                id: "Q-card-1".into(),
                integration_id: "dingtalk-main".into(),
                conversation_id: "cid".into(),
                purpose: "ask".into(),
                title: "t".into(),
                message: "m".into(),
                questions: vec![HumanQuestion {
                    id: "q1".into(),
                    question: "Q?".into(),
                    options: vec!["yes".into()],
                }],
                context: json!({}),
                status: HumanInputStatus::Pending,
                answer: None,
                answer_source: None,
                card_message_id: Some("Q-card-1".into()),
                created_at_epoch: now,
                expires_at_epoch: now + 600,
                answered_at_epoch: None,
            },
        )
        .unwrap();
        app.manage(db);
        app.manage(broker);
        let content = json!({
            "cardPrivateData": { "params": { "answer_q1": "yes" } }
        })
        .to_string();
        let raw = json!({
            "type": "CALLBACK",
            "headers": { "topic": CARD_TOPIC, "messageId": "m-card" },
            "data": json!({ "outTrackId": "Q-card-1", "content": content }).to_string()
        });
        let ack = handle_data_frame_for_test(
            app.handle(),
            &integration,
            serde_json::to_vec(&raw).unwrap().as_slice(),
        )
        .unwrap();
        assert_eq!(ack.code, 200);
        assert!(ack.data.contains("answered"));
        let stored = human_input::get(app.state::<Database>().inner(), "Q-card-1")
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, HumanInputStatus::Answered);
        assert_eq!(stored.answer_source, Some(HumanAnswerSource::DingTalk));
    }

    #[test]
    fn card_callback_unknown_request_is_neutral_ack() {
        let app = tauri::test::mock_app();
        let db = Database::open_in_memory().expect("db");
        let broker = HumanInputBroker::default();
        let integration = integration();
        let mut config = crate::config::service::load_db(&db).expect("load");
        config.messaging.integrations = vec![integration.clone()];
        crate::config::service::persist_db(&db, &config).expect("persist");
        app.manage(db);
        app.manage(broker);
        let raw = json!({
            "type": "CALLBACK",
            "headers": { "topic": CARD_TOPIC, "messageId": "m-miss" },
            "data": json!({ "outTrackId": "missing", "content": "{}" }).to_string()
        });
        let ack = handle_data_frame_for_test(
            app.handle(),
            &integration,
            serde_json::to_vec(&raw).unwrap().as_slice(),
        )
        .unwrap();
        assert_eq!(ack.code, 200);
        assert!(!ack.data.contains("answered"));
    }

    #[test]
    fn stale_generation_rejects_card_callback() {
        let app = tauri::test::mock_app();
        let generations = Arc::new(Mutex::new(HashMap::from([(
            "dingtalk-main".to_string(),
            2u64,
        )])));
        let cancel = CancellationToken::new();
        let raw = json!({
            "type": "CALLBACK",
            "headers": { "topic": CARD_TOPIC, "messageId": "m" },
            "data": "{}"
        });
        let err = handle_data_frame(
            app.handle(),
            &integration(),
            serde_json::to_vec(&raw).unwrap().as_slice(),
            &generations,
            1,
            &cancel,
        )
        .expect_err("stale");
        assert!(err.message.contains("generation"), "{}", err.message);
    }
}
