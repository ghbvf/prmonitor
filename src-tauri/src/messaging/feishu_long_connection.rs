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
use crate::messaging::provider::MessagingProvider;
use crate::messaging::{
    feishu::{build_feishu_http_client, FeishuProvider},
    service, store,
};
use crate::model::{FeishuConnectionState, FeishuConnectionStatus, MessagingProviderKind};

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
    statuses: Arc<Mutex<HashMap<String, FeishuConnectionStatus>>>,
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
        for id in obsolete {
            if let Some(task) = tasks.remove(&id) {
                next_generation(&self.generations, &id);
                task.cancel.cancel();
                task.task.abort();
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
                    task.cancel.cancel();
                    task.task.abort();
                }
                statuses.insert(
                    integration.id.clone(),
                    status(integration, FeishuConnectionState::Disabled),
                );
                continue;
            }
            if tasks.contains_key(&integration.id) {
                continue;
            }
            let token = CancellationToken::new();
            let generation = next_generation(&self.generations, &integration.id);
            let Some(fingerprint) = desired.get(&integration.id).copied() else {
                statuses.insert(
                    integration.id.clone(),
                    status(integration, FeishuConnectionState::Error),
                );
                continue;
            };
            statuses.insert(
                integration.id.clone(),
                status(integration, FeishuConnectionState::Connecting),
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
                    cancel: token.clone(),
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
            value.status = FeishuConnectionState::Stopped;
        }
    }

    pub fn statuses(&self) -> Vec<FeishuConnectionStatus> {
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
    state: FeishuConnectionState,
) -> FeishuConnectionStatus {
    FeishuConnectionStatus {
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
    statuses: Arc<Mutex<HashMap<String, FeishuConnectionStatus>>>,
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
            |value| value.status = FeishuConnectionState::Connecting,
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
                    value.status = FeishuConnectionState::Error;
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
                value.status = FeishuConnectionState::Reconnecting;
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
        |value| value.status = FeishuConnectionState::Stopped,
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
    statuses: &Arc<Mutex<HashMap<String, FeishuConnectionStatus>>>,
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
            value.status = FeishuConnectionState::Connected;
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
            handle_card(app, integration, payload)?;
            // The official SDK returns an empty CardActionTriggerResponse. Its JSON bytes are
            // encoded as the `data` field in the WebSocket ACK ("{}" -> "e30=").
            Ok(Some(b"{}".to_vec()))
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

fn lock_current_generation<'a>(
    generations: &'a Arc<Mutex<HashMap<String, u64>>>,
    integration_id: &str,
    generation: u64,
    cancel: &CancellationToken,
) -> AppResult<std::sync::MutexGuard<'a, HashMap<String, u64>>> {
    if cancel.is_cancelled() {
        return Err(AppError::new("飞书长连接 delivery 已取消"));
    }
    let current = generations.lock().unwrap_or_else(|p| p.into_inner());
    if current.get(integration_id).copied() != Some(generation) || cancel.is_cancelled() {
        return Err(AppError::new("飞书长连接 generation 已失效"));
    }
    Ok(current)
}

fn handle_card<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    integration: &MessagingIntegration,
    payload: &[u8],
) -> AppResult<()> {
    let value: serde_json::Value = serde_json::from_slice(payload)
        .map_err(|e| AppError::new(format!("飞书卡片回调 JSON 损坏: {e}")))?;
    let action = value
        .pointer("/event/action/value")
        .or_else(|| value.pointer("/action/value"))
        .unwrap_or(&value);
    let request_id = action
        .get("requestId")
        .or_else(|| action.get("request_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::new("飞书卡片回调缺少 requestId"))?;
    let question_id = action
        .get("questionId")
        .or_else(|| action.get("question_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("q1");
    let answer_value = action
        .get("answer")
        .or_else(|| action.get("value"))
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
        human_input::answer(
            db.inner(),
            broker.inner(),
            request_id,
            &[HumanAnswer {
                question_id: question_id.into(),
                answer: answer_value.into(),
            }],
            HumanAnswerSource::Feishu,
            store::now_epoch(),
        )?;
    }
    Ok(())
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

fn update_current(
    statuses: &Arc<Mutex<HashMap<String, FeishuConnectionStatus>>>,
    generations: &Arc<Mutex<HashMap<String, u64>>>,
    id: &str,
    generation: u64,
    f: impl FnOnce(&mut FeishuConnectionStatus),
) {
    let current = generations
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(id)
        .copied();
    if current != Some(generation) {
        return;
    }
    if let Some(value) = statuses
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_mut(id)
    {
        f(value);
    }
}

fn next_generation(generations: &Arc<Mutex<HashMap<String, u64>>>, id: &str) -> u64 {
    let mut generations = generations.lock().unwrap_or_else(|p| p.into_inner());
    let generation = generations.entry(id.to_string()).or_default();
    *generation = generation.saturating_add(1);
    *generation
}

fn integration_fingerprint(integration: &MessagingIntegration) -> AppResult<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let serialized = serde_json::to_vec(integration)
        .map_err(|error| AppError::new(format!("飞书集成配置序列化失败: {error}")))?;
    Ok(Sha256::digest(serialized).into())
}

fn clamp_reconnect_secs(value: u64) -> u64 {
    value.clamp(2, 300)
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
}
