//! Feishu official long-connection transport (#1810).
//!
//! Protocol behavior is adapted from larksuite/oapi-sdk-go `ws/client.go`: bootstrap the endpoint,
//! speak binary protobuf frames, reassemble fragments, ACK each durable delivery immediately, ping,
//! and reconnect with server-provided backoff. No callback URL or tunnel is involved.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{engine::general_purpose, Engine as _};
use futures::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use serde::Deserialize;
use serde_json::json;
use tauri::Manager;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::config::service::MessagingIntegration;
use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::messaging::human_input::{self, HumanAnswer, HumanAnswerSource, HumanInputBroker};
use crate::messaging::long_connection::{
    self as long_connection, abort_join_wait, cancel_abort_task, integration_fingerprint,
    lock_current_generation, next_generation, update_current,
};
use crate::messaging::provider::MessagingProvider;
use crate::messaging::{
    feishu::{
        build_feishu_http_client, human_input_choice_field_name, human_input_custom_field_name,
        human_input_option_value, FeishuProvider, HUMAN_INPUT_CUSTOM_OPTION,
        HUMAN_INPUT_FORM_SUBMIT,
    },
    service, store,
};
use crate::model::{MessagingConnectionState, MessagingConnectionStatus, MessagingProviderKind};

const BOOTSTRAP_URL: &str = "https://open.feishu.cn/callback/ws/endpoint";
const MAX_FRAME_PARTS: usize = 64;
const MAX_INFLIGHT_FRAGMENTED_MESSAGES: usize = 128;
const MAX_REASSEMBLED_BYTES: usize = 1024 * 1024;
const MAX_WS_BYTES: usize = 1024 * 1024;

#[derive(Clone, PartialEq, ProstMessage)]
pub(crate) struct FrameHeader {
    #[prost(string, required, tag = "1")]
    pub key: String,
    #[prost(string, required, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, ProstMessage)]
pub(crate) struct Frame {
    #[prost(uint64, required, tag = "1")]
    pub seq_id: u64,
    #[prost(uint64, required, tag = "2")]
    pub log_id: u64,
    #[prost(int32, required, tag = "3")]
    pub service: i32,
    #[prost(int32, required, tag = "4")]
    pub method: i32,
    #[prost(message, repeated, tag = "5")]
    pub headers: Vec<FrameHeader>,
    #[prost(string, optional, tag = "6")]
    pub payload_encoding: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub payload_type: Option<String>,
    #[prost(bytes, optional, tag = "8")]
    pub payload: Option<Vec<u8>>,
    #[prost(string, optional, tag = "9")]
    pub log_id_new: Option<String>,
}

impl Frame {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.key == name)
            .map(|header| header.value.as_str())
    }

    fn add_header(&mut self, key: &str, value: impl Into<String>) {
        self.headers.push(FrameHeader {
            key: key.into(),
            value: value.into(),
        });
    }
}

#[derive(Default)]
pub struct FeishuConnectionManager {
    tasks: Mutex<HashMap<String, ConnectionTask>>,
    statuses: Arc<Mutex<HashMap<String, MessagingConnectionStatus>>>,
    generations: Arc<Mutex<HashMap<String, u64>>>,
}

struct ConnectionTask {
    fingerprint: [u8; 32],
    cancel: CancellationToken,
    task: tauri::async_runtime::JoinHandle<()>,
}

impl FeishuConnectionManager {
    pub fn reconcile(
        &self,
        app: &tauri::AppHandle<tauri::Wry>,
        integrations: &[MessagingIntegration],
    ) {
        let mut tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
        let desired = integrations
            .iter()
            .filter(|item| item.kind == MessagingProviderKind::Feishu && item.enabled)
            .filter_map(|item| match integration_fingerprint(item) {
                Ok(fingerprint) => Some((item.id.clone(), fingerprint)),
                Err(error) => {
                    eprintln!("飞书长连接配置 fingerprint 失败：{}", error.message);
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
            .filter(|item| item.kind == MessagingProviderKind::Feishu)
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
        abort_join_wait(aborted);
        for integration in integrations
            .iter()
            .filter(|item| item.kind == MessagingProviderKind::Feishu && item.enabled)
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
        provider: MessagingProviderKind::Feishu,
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
    let mut runtime_config = RuntimeClientConfig::default();
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
            &mut runtime_config,
        )
        .await
        {
            Ok(()) => {}
            Err(_error) if cancel.is_cancelled() => break,
            Err(error) => update_current(
                &statuses,
                &generations,
                &integration.id,
                generation,
                |value| {
                    value.status = MessagingConnectionState::Error;
                    value.last_error = Some(error.message);
                },
            ),
        }
        if cancel.is_cancelled() {
            break;
        }
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
            _ = tokio::time::sleep(runtime_config.reconnect_interval) => {}
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

#[derive(Deserialize)]
struct EndpointResponse {
    code: i64,
    #[serde(default)]
    msg: String,
    data: Option<EndpointData>,
}
#[derive(Deserialize)]
struct EndpointData {
    #[serde(rename = "URL")]
    url: String,
    #[serde(rename = "ClientConfig", default)]
    config: ClientConfig,
}
#[derive(Default, Deserialize)]
struct ClientConfig {
    #[serde(rename = "ReconnectInterval", default)]
    reconnect_interval: u64,
    #[serde(rename = "PingInterval", default)]
    ping_interval: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeClientConfig {
    reconnect_interval: Duration,
    ping_interval: Duration,
}

impl Default for RuntimeClientConfig {
    fn default() -> Self {
        Self {
            reconnect_interval: Duration::from_secs(2),
            ping_interval: Duration::from_secs(120),
        }
    }
}

impl RuntimeClientConfig {
    fn apply(&mut self, config: &ClientConfig) {
        self.reconnect_interval =
            Duration::from_secs(clamp_reconnect_secs(config.reconnect_interval));
        self.ping_interval = Duration::from_secs(clamp_ping_secs(config.ping_interval));
    }
}

async fn bootstrap(integration: &MessagingIntegration) -> AppResult<EndpointData> {
    let client = build_feishu_http_client(integration)?;
    let response = client
        .post(BOOTSTRAP_URL)
        .json(&json!({
            "AppID": integration.app_id,
            "AppSecret": integration.app_secret,
            "ClientAssertion": "",
        }))
        .send()
        .await
        .map_err(|error| reqwest_stage_error("飞书长连接 bootstrap 请求失败", &error))?;
    let status = response.status();
    if !status.is_success() {
        return Err(AppError::new(format!("飞书长连接 bootstrap HTTP {status}")));
    }
    let body = response
        .bytes()
        .await
        .map_err(|error| reqwest_stage_error("飞书长连接 bootstrap 响应读取失败", &error))?;
    if body.len() > 64 * 1024 {
        return Err(AppError::new("飞书长连接 bootstrap 响应过大"));
    }
    let envelope: EndpointResponse = serde_json::from_slice(&body)
        .map_err(|error| AppError::new(format!("飞书长连接 bootstrap 响应损坏: {error}")))?;
    if envelope.code != 0 {
        return Err(AppError::new(format!(
            "飞书长连接 bootstrap 拒绝: {} ({})",
            envelope.msg, envelope.code
        )));
    }
    let data = envelope
        .data
        .filter(|data| !data.url.is_empty())
        .ok_or_else(|| AppError::new("飞书长连接 bootstrap 缺少 URL"))?;
    let url = url::Url::parse(&data.url)
        .map_err(|error| AppError::new(format!("飞书长连接 URL 非法: {error}")))?;
    if url.scheme() != "wss"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(AppError::new("飞书长连接 URL 必须是无用户信息的 wss 地址"));
    }
    Ok(data)
}

async fn connect_once(
    app: &tauri::AppHandle<tauri::Wry>,
    integration: &MessagingIntegration,
    statuses: &Arc<Mutex<HashMap<String, MessagingConnectionStatus>>>,
    generations: &Arc<Mutex<HashMap<String, u64>>>,
    generation: u64,
    cancel: &CancellationToken,
    runtime_config: &mut RuntimeClientConfig,
) -> AppResult<()> {
    let stage_timeout = Duration::from_secs(integration.timeout_secs.clamp(1, 30));
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_WS_BYTES))
        .max_frame_size(Some(MAX_WS_BYTES));
    let (endpoint, (socket, _)) = establish_connection(
        cancel,
        stage_timeout,
        runtime_config,
        bootstrap(integration),
        |url| async move {
            tokio_tungstenite::connect_async_with_config(&url, Some(ws_config), false)
                .await
                .map_err(|error| websocket_stage_error("飞书 WebSocket 连接失败", &error))
        },
    )
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
    let mut ticker = ping_ticker(runtime_config.ping_interval);
    let mut fragments: HashMap<String, Vec<Option<Vec<u8>>>> = HashMap::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => { let _ = sink.close().await; return Ok(()); }
            _ = ticker.tick() => {
                let frame = Frame { seq_id: 0, log_id: 0, service: service_id(&endpoint.url), method: 0,
                    headers: vec![FrameHeader { key: "type".into(), value: "ping".into() }],
                    payload_encoding: None, payload_type: None, payload: None, log_id_new: None };
                sink.send(Message::Binary(frame.encode_to_vec().into())).await.map_err(|e| AppError::new(format!("飞书 ping 失败: {e}")))?;
            }
            incoming = stream.next() => {
                let message = incoming.ok_or_else(|| AppError::new("飞书 WebSocket 已关闭"))?
                    .map_err(|e| AppError::new(format!("飞书 WebSocket 读取失败: {e}")))?;
                let Message::Binary(bytes) = message else { continue; };
                let mut frame = Frame::decode(bytes).map_err(|e| AppError::new(format!("飞书 frame protobuf 损坏: {e}")))?;
                if frame.method == 0 {
                    match apply_control_frame(runtime_config, &frame) {
                        Ok(true) => ticker = ping_ticker(runtime_config.ping_interval),
                        Ok(false) => {}
                        Err(error) => update_current(statuses, generations, &integration.id, generation, |value| {
                            value.last_error = Some(error.message);
                        }),
                    }
                    continue;
                }
                let payload = combine(&mut fragments, &frame)?;
                let Some(payload) = payload else { continue; };
                let started = std::time::Instant::now();
                let result = persist_delivery(
                    app,
                    integration,
                    frame.header("type").unwrap_or(""),
                    &payload,
                    generations,
                    generation,
                    cancel,
                );
                let ack = delivery_ack(&result);
                let mut ack_payload = json!({"code": ack.code, "headers": {}});
                if let Some(data) = &ack.data {
                    ack_payload["data"] = json!(data);
                }
                frame.payload = Some(serde_json::to_vec(&ack_payload)
                    .map_err(|error| AppError::new(format!("飞书 ACK 序列化失败: {error}")))?);
                frame.add_header("biz_rt", started.elapsed().as_millis().to_string());
                sink.send(Message::Binary(frame.encode_to_vec().into())).await.map_err(|e| AppError::new(format!("飞书 ACK 失败: {e}")))?;
                update_current(statuses, generations, &integration.id, generation, |value| {
                    value.last_event_at_epoch = Some(store::now_epoch());
                    if let Some(error) = &ack.business_error {
                        value.last_error = Some(error.clone());
                    }
                });
            }
        }
    }
}

async fn establish_connection<T, BootstrapFuture, Connector, ConnectFuture>(
    cancel: &CancellationToken,
    timeout: Duration,
    runtime_config: &mut RuntimeClientConfig,
    bootstrap_future: BootstrapFuture,
    connector: Connector,
) -> AppResult<(EndpointData, T)>
where
    BootstrapFuture: Future<Output = AppResult<EndpointData>>,
    Connector: FnOnce(String) -> ConnectFuture,
    ConnectFuture: Future<Output = AppResult<T>>,
{
    let endpoint = await_connection_stage(cancel, timeout, "bootstrap", bootstrap_future).await?;
    runtime_config.apply(&endpoint.config);
    let connection = await_connection_stage(
        cancel,
        timeout,
        "WebSocket 握手",
        connector(endpoint.url.clone()),
    )
    .await?;
    Ok((endpoint, connection))
}

async fn await_connection_stage<T, F>(
    cancel: &CancellationToken,
    timeout: Duration,
    stage: &str,
    future: F,
) -> AppResult<T>
where
    F: Future<Output = AppResult<T>>,
{
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(AppError::new(format!("飞书长连接 {stage} 已取消"))),
        result = tokio::time::timeout(timeout, future) => match result {
            Ok(result) => result,
            Err(_) => Err(AppError::new(format!("飞书长连接 {stage} 超时"))),
        },
    }
}

fn ping_ticker(interval: Duration) -> tokio::time::Interval {
    tokio::time::interval_at(tokio::time::Instant::now() + interval, interval)
}

fn apply_control_frame(runtime_config: &mut RuntimeClientConfig, frame: &Frame) -> AppResult<bool> {
    if frame.header("type") != Some("pong")
        || frame.payload.as_deref().unwrap_or_default().is_empty()
    {
        return Ok(false);
    }
    let config: ClientConfig = serde_json::from_slice(frame.payload.as_deref().unwrap_or_default())
        .map_err(|error| AppError::new(format!("飞书 Pong 配置损坏: {error}")))?;
    runtime_config.apply(&config);
    Ok(true)
}

fn reqwest_stage_error(stage: &str, error: &reqwest::Error) -> AppError {
    let category = if error.is_timeout() {
        "超时"
    } else if error.is_connect() {
        "连接失败"
    } else if error.is_body() || error.is_decode() {
        "响应损坏"
    } else {
        "传输失败"
    };
    let status = error
        .status()
        .map(|value| format!(" HTTP {value}"))
        .unwrap_or_default();
    let cause = std::error::Error::source(error)
        .map(|value| format!(": {value}"))
        .unwrap_or_default();
    AppError::new(format!("{stage}: {category}{status}{cause}"))
}

fn websocket_stage_error(stage: &str, error: &tokio_tungstenite::tungstenite::Error) -> AppError {
    use tokio_tungstenite::tungstenite::Error;
    let detail = match error {
        Error::ConnectionClosed | Error::AlreadyClosed => "连接已关闭".to_string(),
        Error::Io(error) => format!("I/O: {error}"),
        Error::Tls(_) => "TLS 握手失败".to_string(),
        Error::Capacity(error) => format!("容量限制: {error}"),
        Error::Protocol(error) => format!("协议错误: {error}"),
        Error::WriteBufferFull(_) => "写缓冲区已满".to_string(),
        Error::Utf8(_) => "UTF-8 损坏".to_string(),
        Error::AttackAttempt => "检测到异常握手".to_string(),
        Error::Url(error) => format!("URL 错误: {error}"),
        Error::Http(response) => format!("HTTP {}", response.status()),
        Error::HttpFormat(error) => format!("HTTP 格式错误: {error}"),
    };
    AppError::new(format!("{stage}: {detail}"))
}

fn service_id(url: &str) -> i32 {
    url::Url::parse(url)
        .ok()
        .and_then(|url| {
            url.query_pairs()
                .find(|(key, _)| key == "service_id")
                .and_then(|(_, value)| value.parse().ok())
        })
        .unwrap_or(0)
}

fn combine(
    cache: &mut HashMap<String, Vec<Option<Vec<u8>>>>,
    frame: &Frame,
) -> AppResult<Option<Vec<u8>>> {
    let sum = frame
        .header("sum")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1);
    let seq = frame
        .header("seq")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let payload = frame.payload.clone().unwrap_or_default();
    if sum <= 1 {
        if payload.len() > MAX_REASSEMBLED_BYTES {
            return Err(AppError::new("飞书 frame payload 过大"));
        }
        return Ok(Some(payload));
    }
    if sum > MAX_FRAME_PARTS {
        return Err(AppError::new("飞书 frame 分片数超过限制"));
    }
    if seq >= sum {
        return Err(AppError::new("飞书 frame 分片序号越界"));
    }
    let id = frame
        .header("message_id")
        .ok_or_else(|| AppError::new("飞书分片缺少 message_id"))?
        .to_string();
    if !cache.contains_key(&id) && cache.len() >= MAX_INFLIGHT_FRAGMENTED_MESSAGES {
        return Err(AppError::new("飞书并发分片消息超过限制"));
    }
    let parts = cache.entry(id.clone()).or_insert_with(|| vec![None; sum]);
    if parts.len() != sum {
        return Err(AppError::new("飞书 frame 分片总数不一致"));
    }
    if let Some(previous) = &parts[seq] {
        if previous != &payload {
            return Err(AppError::new("飞书重复分片内容冲突"));
        }
        return Ok(None);
    }
    let accumulated = parts
        .iter()
        .filter_map(Option::as_ref)
        .map(Vec::len)
        .sum::<usize>();
    if accumulated.saturating_add(payload.len()) > MAX_REASSEMBLED_BYTES {
        cache.remove(&id);
        return Err(AppError::new("飞书分片消息累计大小超过限制"));
    }
    parts[seq] = Some(payload);
    if parts.iter().any(Option::is_none) {
        return Ok(None);
    }
    let joined = parts
        .iter()
        .flat_map(|part| part.as_deref().unwrap_or_default())
        .copied()
        .collect();
    cache.remove(&id);
    Ok(Some(joined))
}

fn persist_delivery<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    integration: &MessagingIntegration,
    kind: &str,
    payload: &[u8],
    generations: &Arc<Mutex<HashMap<String, u64>>>,
    generation: u64,
    cancel: &CancellationToken,
) -> AppResult<Option<Vec<u8>>> {
    let _generation_guard =
        lock_current_generation(generations, &integration.id, generation, cancel)?;
    service::validate_current_feishu_long_connection(app, integration)?;
    match delivery_route(kind, payload)? {
        DeliveryRoute::CardAction => {
            match handle_card(app, integration, payload) {
                Ok(()) => Ok(Some(b"{}".to_vec())),
                // Validation / answer failures stay non-terminal: ACK 200 + Feishu toast so the user
                // can retry. Transport stays healthy; see CardActionTriggerResponse toast contract.
                Err(error) => Ok(Some(card_action_error_toast(&error.message))),
            }
        }
        DeliveryRoute::Event => {
            let event = FeishuProvider.parse_event(payload, integration, store::now_epoch())?;
            service::persist_verified_long_connection_event(app, integration, &event)?;
            Ok(None)
        }
        DeliveryRoute::Ignore => Ok(None),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DeliveryRoute {
    CardAction,
    Event,
    Ignore,
}

fn delivery_route(kind: &str, payload: &[u8]) -> AppResult<DeliveryRoute> {
    if kind == "card" {
        return Ok(DeliveryRoute::CardAction);
    }
    if kind != "event" {
        return Ok(DeliveryRoute::Ignore);
    }
    let value: serde_json::Value = serde_json::from_slice(payload)
        .map_err(|error| AppError::new(format!("飞书 event JSON 损坏: {error}")))?;
    let event_type = value
        .pointer("/header/event_type")
        .or_else(|| value.get("event_type"))
        .and_then(serde_json::Value::as_str);
    if event_type == Some("card.action.trigger") {
        Ok(DeliveryRoute::CardAction)
    } else {
        Ok(DeliveryRoute::Event)
    }
}

fn handle_card<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    integration: &MessagingIntegration,
    payload: &[u8],
) -> AppResult<()> {
    let value: serde_json::Value = serde_json::from_slice(payload)
        .map_err(|e| AppError::new(format!("飞书卡片回调 JSON 损坏: {e}")))?;
    let callback_action = value
        .pointer("/event/action")
        .or_else(|| value.get("action"))
        .unwrap_or(&value);
    let action_value = callback_action.get("value").unwrap_or(callback_action);
    let request_id = action_value
        .get("requestId")
        .or_else(|| action_value.get("request_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::new("飞书卡片回调缺少 requestId"))?;
    let question_id = action_value
        .get("questionId")
        .or_else(|| action_value.get("question_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("q1");
    let answer_value = action_value
        .get("answer")
        .or_else(|| action_value.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let db = app.state::<Database>();
    let broker = app.state::<HumanInputBroker>();
    let request = human_input::get(db.inner(), request_id)?
        .ok_or_else(|| AppError::new(format!("human input request 不存在: {request_id}")))?;
    if request.integration_id != integration.id {
        return Err(AppError::new("飞书卡片回调不属于当前消息集成"));
    }
    validate_callback_conversation(&value, &request.conversation_id)?;
    if question_id == "__cancel__" || answer_value == "__cancel__" {
        human_input::cancel(db.inner(), broker.inner(), request_id, store::now_epoch())?;
    } else {
        let answers = card_answers(callback_action, action_value, &request)?;
        human_input::answer(
            db.inner(),
            broker.inner(),
            request_id,
            &answers,
            HumanAnswerSource::Feishu,
            store::now_epoch(),
        )?;
    }
    Ok(())
}

fn card_answers(
    callback_action: &serde_json::Value,
    action_value: &serde_json::Value,
    request: &human_input::HumanInputRequest,
) -> AppResult<Vec<HumanAnswer>> {
    let is_form_submit = callback_action.get("name").and_then(|value| value.as_str())
        == Some(HUMAN_INPUT_FORM_SUBMIT)
        || action_value.get("action").and_then(|value| value.as_str()) == Some("submit");
    if is_form_submit {
        let values = callback_action
            .get("form_value")
            .and_then(|value| value.as_object())
            .ok_or_else(|| AppError::new("飞书表单回调缺少 form_value"))?;
        return request
            .questions
            .iter()
            .enumerate()
            .map(|(index, question)| {
                let custom_field = human_input_custom_field_name(index);
                let custom_answer = || {
                    values
                        .get(&custom_field)
                        .and_then(|value| value.as_str())
                        .filter(|value| !value.trim().is_empty())
                        .map(str::to_string)
                        .ok_or_else(|| {
                            AppError::new(format!("飞书表单回调缺少自定义答案 {custom_field}"))
                        })
                };
                let answer = if question.options.is_empty() {
                    custom_answer()?
                } else {
                    let choice_field = human_input_choice_field_name(index);
                    let choice = values
                        .get(&choice_field)
                        .and_then(|value| value.as_str())
                        .ok_or_else(|| {
                            AppError::new(format!("飞书表单回调缺少字段 {choice_field}"))
                        })?;
                    if choice == HUMAN_INPUT_CUSTOM_OPTION {
                        custom_answer()?
                    } else {
                        question
                            .options
                            .iter()
                            .enumerate()
                            .find(|(option_index, _)| {
                                choice == human_input_option_value(*option_index)
                            })
                            .map(|(_, option)| option.clone())
                            .ok_or_else(|| {
                                AppError::new(format!("飞书表单选项非法: {choice_field}"))
                            })?
                    }
                };
                Ok(HumanAnswer {
                    question_id: question.id.clone(),
                    answer,
                })
            })
            .collect();
    }

    let question_id = action_value
        .get("questionId")
        .or_else(|| action_value.get("question_id"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| AppError::new("飞书卡片回调缺少 questionId"))?;
    let answer = action_value
        .get("answer")
        .or_else(|| action_value.get("value"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| AppError::new("飞书卡片回调缺少 answer"))?;
    Ok(vec![HumanAnswer {
        question_id: question_id.to_string(),
        answer: answer.to_string(),
    }])
}

fn card_action_error_toast(message: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "toast": {
            "type": "error",
            "content": message,
        }
    }))
    .expect("card action toast serializes")
}

fn validate_callback_conversation(value: &serde_json::Value, expected: &str) -> AppResult<()> {
    let conversation = [
        "/event/context/open_chat_id",
        "/event/context/openChatId",
        "/context/open_chat_id",
        "/context/openChatId",
    ]
    .iter()
    .find_map(|pointer| value.pointer(pointer).and_then(|item| item.as_str()))
    .filter(|value| !value.trim().is_empty())
    .ok_or_else(|| AppError::new("飞书卡片回调缺少会话上下文"))?;
    if conversation != expected {
        return Err(AppError::new("飞书卡片回调不属于原会话"));
    }
    Ok(())
}

fn clamp_reconnect_secs(value: u64) -> u64 {
    long_connection::clamp_reconnect_secs(value)
}

fn clamp_ping_secs(value: u64) -> u64 {
    value.clamp(30, 300)
}

struct DeliveryAck {
    code: u16,
    business_error: Option<String>,
    data: Option<String>,
}

fn delivery_ack(result: &AppResult<Option<Vec<u8>>>) -> DeliveryAck {
    match result {
        Ok(data) => DeliveryAck {
            code: 200,
            business_error: None,
            data: data
                .as_ref()
                .map(|bytes| general_purpose::STANDARD.encode(bytes)),
        },
        Err(error) => DeliveryAck {
            code: 500,
            business_error: Some(error.message.clone()),
            data: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_callback_requires_the_original_conversation() {
        assert!(validate_callback_conversation(&json!({}), "chat-a").is_err());
        assert!(validate_callback_conversation(
            &json!({"event":{"context":{"open_chat_id":"chat-b"}}}),
            "chat-a"
        )
        .is_err());
        assert!(validate_callback_conversation(
            &json!({"event":{"context":{"open_chat_id":"chat-a"}}}),
            "chat-a"
        )
        .is_ok());
    }

    #[test]
    fn pong_updates_runtime_intervals() {
        let mut config = RuntimeClientConfig::default();
        let pong = Frame {
            seq_id: 1,
            log_id: 2,
            service: 3,
            method: 0,
            headers: vec![FrameHeader {
                key: "type".into(),
                value: "pong".into(),
            }],
            payload_encoding: None,
            payload_type: Some("application/json".into()),
            payload: Some(
                serde_json::to_vec(&json!({
                    "ReconnectInterval": 41,
                    "PingInterval": 73
                }))
                .unwrap(),
            ),
            log_id_new: None,
        };

        assert!(apply_control_frame(&mut config, &pong).unwrap());
        assert_eq!(config.reconnect_interval, Duration::from_secs(41));
        assert_eq!(config.ping_interval, Duration::from_secs(73));
    }

    #[test]
    fn delivery_fence_rejects_cancelled_and_stale_generations() {
        let generations = Arc::new(Mutex::new(HashMap::from([("fs".to_string(), 2)])));
        let active = CancellationToken::new();
        assert!(lock_current_generation(&generations, "fs", 2, &active).is_ok());
        assert!(lock_current_generation(&generations, "fs", 1, &active).is_err());
        active.cancel();
        assert!(lock_current_generation(&generations, "fs", 2, &active).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn connection_stage_observes_cancel_and_timeout() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let cancelled = await_connection_stage(
            &cancel,
            Duration::from_secs(30),
            "test",
            std::future::pending::<AppResult<()>>(),
        )
        .await
        .expect_err("cancel wins");
        assert!(cancelled.message.contains("已取消"));

        let timed_out = tokio::spawn(async {
            let timeout_cancel = CancellationToken::new();
            await_connection_stage(
                &timeout_cancel,
                Duration::from_secs(3),
                "test",
                std::future::pending::<AppResult<()>>(),
            )
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        let timed_out = timed_out.await.unwrap().expect_err("timeout wins");
        assert!(timed_out.message.contains("超时"));
    }

    #[tokio::test]
    async fn failed_handshake_preserves_bootstrap_reconnect_config() {
        let cancel = CancellationToken::new();
        let mut config = RuntimeClientConfig::default();
        let result = establish_connection(
            &cancel,
            Duration::from_secs(30),
            &mut config,
            async {
                Ok(EndpointData {
                    url: "wss://example.invalid/ws".into(),
                    config: ClientConfig {
                        reconnect_interval: 41,
                        ping_interval: 73,
                    },
                })
            },
            |_| async { Err::<(), _>(AppError::new("handshake failed")) },
        )
        .await;

        assert!(result.is_err());
        assert_eq!(config.reconnect_interval, Duration::from_secs(41));
        assert_eq!(config.ping_interval, Duration::from_secs(73));
    }

    #[test]
    fn protobuf_frame_round_trips_official_field_numbers() {
        let frame = Frame {
            seq_id: 1,
            log_id: 2,
            service: 3,
            method: 1,
            headers: vec![FrameHeader {
                key: "type".into(),
                value: "event".into(),
            }],
            payload_encoding: None,
            payload_type: Some("application/json".into()),
            payload: Some(b"{}".to_vec()),
            log_id_new: None,
        };
        assert_eq!(
            Frame::decode(frame.encode_to_vec().as_slice()).unwrap(),
            frame
        );
    }

    #[test]
    fn fragments_are_reassembled_in_sequence_order() {
        let mut cache = HashMap::new();
        let part = |seq: usize, bytes: &[u8]| Frame {
            seq_id: 0,
            log_id: 0,
            service: 0,
            method: 1,
            headers: vec![
                FrameHeader {
                    key: "sum".into(),
                    value: "2".into(),
                },
                FrameHeader {
                    key: "seq".into(),
                    value: seq.to_string(),
                },
                FrameHeader {
                    key: "message_id".into(),
                    value: "m1".into(),
                },
            ],
            payload_encoding: None,
            payload_type: None,
            payload: Some(bytes.to_vec()),
            log_id_new: None,
        };
        assert!(combine(&mut cache, &part(1, b"B")).unwrap().is_none());
        assert_eq!(
            combine(&mut cache, &part(0, b"A")).unwrap(),
            Some(b"AB".to_vec())
        );
    }

    #[test]
    fn runtime_fingerprint_changes_for_every_consumed_integration_field() {
        let mut integration = MessagingIntegration::feishu_default();
        integration.id = "fs".into();
        integration.enabled = true;
        integration.app_id = "id".into();
        integration.app_secret = "secret".into();
        integration.bot_open_id = "bot-1".into();
        let first = integration_fingerprint(&integration).unwrap();
        integration.bot_open_id = "bot-2".into();
        assert_ne!(first, integration_fingerprint(&integration).unwrap());
        let second = integration_fingerprint(&integration).unwrap();
        integration.timeout_secs += 1;
        assert_ne!(second, integration_fingerprint(&integration).unwrap());
    }

    #[test]
    fn server_intervals_are_clamped() {
        assert_eq!(clamp_reconnect_secs(0), 2);
        assert_eq!(clamp_reconnect_secs(60), 60);
        assert_eq!(clamp_reconnect_secs(86_400), 300);
        assert_eq!(clamp_ping_secs(0), 30);
        assert_eq!(clamp_ping_secs(86_400), 300);
    }

    #[test]
    fn business_delivery_failure_is_acknowledged_without_transport_failure() {
        let error = AppError::new("unsupported event");
        let outcome = delivery_ack(&Err(error));
        assert_eq!(outcome.code, 500);
        assert!(outcome.business_error.is_some());
        assert!(outcome.data.is_none());
    }

    #[test]
    fn official_card_action_payload_is_routed_from_event_frame() {
        let payload = serde_json::to_vec(&json!({
            "schema": "2.0",
            "header": {
                "event_id": "evt-card-1",
                "event_type": "card.action.trigger"
            },
            "event": {
                "context": {
                    "open_message_id": "om-card-1",
                    "open_chat_id": "oc-card-1"
                },
                "action": {
                    "value": {
                        "requestId": "Q-card-1",
                        "questionId": "decision",
                        "answer": "通过"
                    },
                    "tag": "button"
                }
            }
        }))
        .unwrap();

        assert_eq!(
            delivery_route("event", &payload).unwrap(),
            DeliveryRoute::CardAction
        );
    }

    #[test]
    fn card_callback_ack_matches_official_ws_response_contract() {
        let outcome = delivery_ack(&Ok(Some(b"{}".to_vec())));
        assert_eq!(outcome.code, 200);
        assert_eq!(outcome.data.as_deref(), Some("e30="));
        assert!(outcome.business_error.is_none());
    }

    #[test]
    fn card_action_validation_failure_acks_with_error_toast() {
        let toast = card_action_error_toast("飞书表单回调缺少 form_value");
        let outcome = delivery_ack(&Ok(Some(toast.clone())));
        assert_eq!(outcome.code, 200);
        assert!(outcome.business_error.is_none());
        let decoded = general_purpose::STANDARD
            .decode(outcome.data.as_deref().expect("toast data"))
            .expect("base64");
        assert_eq!(decoded, toast);
        let body: serde_json::Value = serde_json::from_slice(&decoded).expect("json");
        assert_eq!(body["toast"]["type"], "error");
        assert_eq!(body["toast"]["content"], "飞书表单回调缺少 form_value");
    }

    #[test]
    fn card_answers_covers_empty_options_and_rejects_bad_form_or_legacy() {
        let open = human_input::HumanInputRequest {
            id: "Q-open".into(),
            integration_id: "fs".into(),
            conversation_id: "oc".into(),
            purpose: "q".into(),
            title: "t".into(),
            message: "m".into(),
            questions: vec![human_input::HumanQuestion {
                id: "open".into(),
                question: "补充？".into(),
                options: vec![],
            }],
            context: json!({}),
            status: human_input::HumanInputStatus::Pending,
            answer: None,
            answer_source: None,
            card_message_id: None,
            created_at_epoch: 1,
            expires_at_epoch: 2,
            answered_at_epoch: None,
        };
        let choice = human_input::HumanInputRequest {
            questions: vec![human_input::HumanQuestion {
                id: "decision".into(),
                question: "选？".into(),
                options: vec!["通过".into(), "拒绝".into()],
            }],
            ..open.clone()
        };

        let empty_ok = card_answers(
            &json!({
                "name": HUMAN_INPUT_FORM_SUBMIT,
                "form_value": {"q_0_custom": "自由填写"}
            }),
            &json!({"action": "submit"}),
            &open,
        )
        .expect("empty-options custom answer");
        assert_eq!(empty_ok.len(), 1);
        assert_eq!(empty_ok[0].question_id, "open");
        assert_eq!(empty_ok[0].answer, "自由填写");

        assert!(card_answers(
            &json!({"name": HUMAN_INPUT_FORM_SUBMIT}),
            &json!({"action": "submit"}),
            &open
        )
        .expect_err("missing form_value")
        .message
        .contains("form_value"));

        assert!(card_answers(
            &json!({
                "name": HUMAN_INPUT_FORM_SUBMIT,
                "form_value": {"q_0_choice": "o_9"}
            }),
            &json!({"action": "submit"}),
            &choice
        )
        .expect_err("illegal option")
        .message
        .contains("非法"));

        assert!(card_answers(
            &json!({
                "name": HUMAN_INPUT_FORM_SUBMIT,
                "form_value": {
                    "q_0_choice": HUMAN_INPUT_CUSTOM_OPTION,
                    "q_0_custom": "   "
                }
            }),
            &json!({"action": "submit"}),
            &choice
        )
        .expect_err("empty custom")
        .message
        .contains("自定义答案"));

        let custom_ok = card_answers(
            &json!({
                "name": HUMAN_INPUT_FORM_SUBMIT,
                "form_value": {
                    "q_0_choice": HUMAN_INPUT_CUSTOM_OPTION,
                    "q_0_custom": "人工填写"
                }
            }),
            &json!({"action": "submit"}),
            &choice,
        )
        .expect("custom path");
        assert_eq!(custom_ok[0].answer, "人工填写");

        assert!(card_answers(&json!({}), &json!({}), &choice)
            .expect_err("legacy missing fields")
            .message
            .contains("questionId"));
        assert!(
            card_answers(&json!({}), &json!({"questionId": "decision"}), &choice)
                .expect_err("legacy missing answer")
                .message
                .contains("answer")
        );
    }

    #[test]
    fn official_event_frame_persists_card_answer_and_builds_success_ack() {
        let app = tauri::test::mock_app();
        let db = Database::open_in_memory().expect("open test db");
        let broker = HumanInputBroker::default();
        let integration = MessagingIntegration {
            id: "feishu-card-test".into(),
            enabled: true,
            app_id: "cli_card_test".into(),
            app_secret: "test-secret".into(),
            allowed_conversation_ids: vec!["oc-card-1".into()],
            ..MessagingIntegration::feishu_default()
        };

        let mut config = crate::config::service::load_db(&db).expect("load config");
        config.messaging.integrations = vec![integration.clone()];
        crate::config::service::persist_db(&db, &config).expect("persist config");

        let now = store::now_epoch();
        let request = human_input::HumanInputRequest {
            id: "Q-card-1".into(),
            integration_id: integration.id.clone(),
            conversation_id: "oc-card-1".into(),
            purpose: "question".into(),
            title: "Choose".into(),
            message: "Approve?".into(),
            questions: vec![human_input::HumanQuestion {
                id: "decision".into(),
                question: "Approve?".into(),
                options: vec!["通过".into(), "拒绝".into()],
            }],
            context: json!({}),
            status: human_input::HumanInputStatus::Pending,
            answer: None,
            answer_source: None,
            card_message_id: Some("om-card-1".into()),
            created_at_epoch: now,
            expires_at_epoch: now + 60,
            answered_at_epoch: None,
        };
        human_input::create(&db, &request).expect("create request");
        app.manage(db);
        app.manage(broker);

        let payload = serde_json::to_vec(&json!({
            "schema": "2.0",
            "header": {
                "event_id": "evt-card-1",
                "event_type": "card.action.trigger"
            },
            "event": {
                "context": {
                    "open_message_id": "om-card-1",
                    "open_chat_id": "oc-card-1"
                },
                "action": {
                    "value": {
                        "requestId": request.id,
                        "questionId": "decision",
                        "answer": "通过"
                    },
                    "tag": "button"
                }
            }
        }))
        .unwrap();
        let generations = Arc::new(Mutex::new(HashMap::from([(integration.id.clone(), 1)])));
        let cancel = CancellationToken::new();

        let result = persist_delivery(
            app.handle(),
            &integration,
            "event",
            &payload,
            &generations,
            1,
            &cancel,
        );
        let ack = delivery_ack(&result);
        assert_eq!(ack.code, 200);
        assert_eq!(ack.data.as_deref(), Some("e30="));
        let stored = human_input::get(app.state::<Database>().inner(), "Q-card-1")
            .expect("load request")
            .expect("request exists");
        assert_eq!(stored.status, human_input::HumanInputStatus::Answered);
        assert_eq!(stored.answer_source, Some(HumanAnswerSource::Feishu));
        assert_eq!(stored.answer.expect("answers")[0].answer, "通过");
    }

    #[test]
    fn form_callback_persists_all_question_answers_atomically() {
        let app = tauri::test::mock_app();
        let db = Database::open_in_memory().expect("open test db");
        let broker = HumanInputBroker::default();
        let integration = MessagingIntegration {
            id: "feishu-form-test".into(),
            enabled: true,
            app_id: "cli_form_test".into(),
            app_secret: "test-secret".into(),
            allowed_conversation_ids: vec!["oc-form-1".into()],
            ..MessagingIntegration::feishu_default()
        };

        let mut config = crate::config::service::load_db(&db).expect("load config");
        config.messaging.integrations = vec![integration.clone()];
        crate::config::service::persist_db(&db, &config).expect("persist config");

        let now = store::now_epoch();
        let request = human_input::HumanInputRequest {
            id: "Q-form-1".into(),
            integration_id: integration.id.clone(),
            conversation_id: "oc-form-1".into(),
            purpose: "plan".into(),
            title: "Choose both".into(),
            message: "Complete the form".into(),
            questions: vec![
                human_input::HumanQuestion {
                    id: "provenance_fix".into(),
                    question: "How should provenance be fixed?".into(),
                    options: vec!["current".into(), "later".into()],
                },
                human_input::HumanQuestion {
                    id: "discovery_boundary".into(),
                    question: "Where is discovery bounded?".into(),
                    options: vec!["generated".into(), "canonical".into()],
                },
            ],
            context: json!({}),
            status: human_input::HumanInputStatus::Pending,
            answer: None,
            answer_source: None,
            card_message_id: Some("om-form-1".into()),
            created_at_epoch: now,
            expires_at_epoch: now + 60,
            answered_at_epoch: None,
        };
        human_input::create(&db, &request).expect("create request");
        app.manage(db);
        app.manage(broker);

        let payload = serde_json::to_vec(&json!({
            "schema": "2.0",
            "header": {
                "event_id": "evt-form-1",
                "event_type": "card.action.trigger"
            },
            "event": {
                "context": {
                    "open_message_id": "om-form-1",
                    "open_chat_id": "oc-form-1"
                },
                "action": {
                    "value": {
                        "requestId": request.id,
                        "action": "submit"
                    },
                    "tag": "button",
                    "name": "human_input_submit",
                    "form_value": {
                        "q_0_choice": "custom",
                        "q_0_custom": "custom policy",
                        "q_1_choice": "o_0",
                        "q_1_custom": ""
                    }
                }
            }
        }))
        .unwrap();
        let generations = Arc::new(Mutex::new(HashMap::from([(integration.id.clone(), 1)])));
        let cancel = CancellationToken::new();

        let result = persist_delivery(
            app.handle(),
            &integration,
            "event",
            &payload,
            &generations,
            1,
            &cancel,
        );
        assert!(result.is_ok(), "form callback failed: {result:?}");
        let stored = human_input::get(app.state::<Database>().inner(), "Q-form-1")
            .expect("load request")
            .expect("request exists");
        assert_eq!(stored.status, human_input::HumanInputStatus::Answered);
        assert_eq!(stored.answer_source, Some(HumanAnswerSource::Feishu));
        let answers = stored.answer.expect("answers");
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].question_id, "provenance_fix");
        assert_eq!(answers[0].answer, "custom policy");
        assert_eq!(answers[1].question_id, "discovery_boundary");
        assert_eq!(answers[1].answer, "generated");
    }
}
