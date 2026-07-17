//! Messaging ingress + command processing (#1559).

use std::sync::Arc;
use std::sync::Mutex;

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};
use tauri::{Manager, Runtime};

use crate::config::service::{self as config_service, MessagingIntegration};
use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::messaging::feishu::FeishuProvider;
use crate::messaging::provider::{MessagingProvider, Verification};
use crate::messaging::store;
use crate::messaging::{dingtalk::DingTalkProvider, wechat_work::WeChatWorkProvider};
use crate::model::{
    ActionExecutionResult, ActionKind, ExternalRequestId, MessagingEvent, MessagingEventStatus,
    MessagingProviderKind, MessagingReplyPayload, MessagingReplyTarget, MessagingSendContent,
    MessagingSendPayload, OutboxEntry, ReviewKind, ReviewReceiptId, SendMessagingRequest,
    SendMessagingResponse,
};
use crate::state::AppState;

#[cfg(test)]
use crate::model::MessagingCardTemplate;

pub trait MessagingActions<R: Runtime>: Send + Sync + 'static {
    fn enqueue_reply(
        &self,
        app: &tauri::AppHandle<R>,
        integration_id: &str,
        kind: ActionKind,
        summary: &str,
        payload_json: &str,
        dedupe_key: &str,
    ) -> AppResult<i64>;

    fn enqueue_send(
        &self,
        app: &tauri::AppHandle<R>,
        kind: ActionKind,
        summary: &str,
        payload_json: &str,
        dedupe_key: &str,
    ) -> AppResult<i64>;

    fn enqueue_send_after(
        &self,
        app: &tauri::AppHandle<R>,
        kind: ActionKind,
        summary: &str,
        payload_json: &str,
        dedupe_key: &str,
        delay_secs: u64,
    ) -> AppResult<i64>;

    fn enqueue_send_once_after(
        &self,
        app: &tauri::AppHandle<R>,
        kind: ActionKind,
        summary: &str,
        payload_json: &str,
        dedupe_key: &str,
        delay_secs: u64,
    ) -> AppResult<i64>;

    fn list_sends(
        &self,
        app: &tauri::AppHandle<R>,
        integration_id: Option<&str>,
    ) -> AppResult<Vec<OutboxEntry>>;

    fn submit_review(
        &self,
        app: &tauri::AppHandle<R>,
        reference: String,
        pr_number: u64,
        kind: ReviewKind,
        request_id: ExternalRequestId,
    ) -> AppResult<ReviewReceiptId>;
}

pub struct MessagingRuntime<R: Runtime> {
    pub actions: Arc<dyn MessagingActions<R>>,
}

/// Single-consumer durable ingress pump. Producers only insert + notify; the database is the queue,
/// so crashes lose no accepted delivery and a burst never creates an unbounded task set.
#[derive(Default)]
pub struct MessagingEventWorker {
    notify: Arc<tokio::sync::Notify>,
    cancel: tokio_util::sync::CancellationToken,
    task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

impl MessagingEventWorker {
    pub fn start(&self, app: tauri::AppHandle<tauri::Wry>) {
        let mut slot = self.task.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_some() {
            return;
        }
        let notify = self.notify.clone();
        let cancel = self.cancel.clone();
        *slot = Some(tauri::async_runtime::spawn(async move {
            loop {
                if let Err(error) = drain_received(&app).await {
                    eprintln!("messaging received drain failed: {}", error.message);
                }
                tokio::select! { _ = cancel.cancelled() => break, _ = notify.notified() => {} }
            }
        }));
    }

    pub fn wake(&self) {
        self.notify.notify_one();
    }

    pub fn shutdown(&self) {
        self.cancel.cancel();
        if let Some(task) = self.task.lock().unwrap_or_else(|p| p.into_inner()).take() {
            task.abort();
        }
    }
}

async fn drain_received<R: Runtime>(app: &tauri::AppHandle<R>) -> AppResult<()> {
    let db = app.state::<Database>();
    let runtime = app.state::<MessagingRuntime<R>>();
    loop {
        let ids = store::received_ids(db.inner())?;
        if ids.is_empty() {
            break;
        }
        for id in ids {
            let Some(entry) = store::get_entry(db.inner(), id)? else {
                continue;
            };
            let integration =
                match config_service::messaging_integration(app, &entry.event.integration_id) {
                    Ok(value) if value.enabled && value.kind == entry.event.provider => value,
                    Ok(value) if value.enabled => {
                        store::mark_failed(
                            db.inner(),
                            id,
                            "消息集成 provider 已变更，拒绝处理旧事件",
                            store::now_epoch(),
                        )?;
                        continue;
                    }
                    Ok(_) => {
                        store::mark_failed(
                            db.inner(),
                            id,
                            "消息集成已禁用，无法处理持久化事件",
                            store::now_epoch(),
                        )?;
                        continue;
                    }
                    Err(error) => {
                        store::mark_failed(
                            db.inner(),
                            id,
                            &format!("消息集成不可用: {}", error.message),
                            store::now_epoch(),
                        )?;
                        continue;
                    }
                };
            let _ = process_and_mark(
                app,
                runtime.actions.as_ref(),
                db.inner(),
                id,
                &integration,
                &entry.event,
            )
            .await;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestResponse {
    Challenge { challenge: String },
    Ack,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MessagingCommand {
    Help,
    Status,
    Answer {
        request_id: String,
        value: String,
    },
    Cancel {
        request_id: String,
    },
    Review {
        reference: String,
        pr_number: u64,
        kind: ReviewCommandKind,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewCommandKind {
    Review,
    Check,
}

impl ReviewCommandKind {
    fn as_model(self) -> ReviewKind {
        match self {
            ReviewCommandKind::Review => ReviewKind::Review,
            ReviewCommandKind::Check => ReviewKind::Check,
        }
    }

    fn as_wire(self) -> &'static str {
        self.as_model().as_str()
    }
}

pub async fn ingest<R: Runtime>(
    app: &tauri::AppHandle<R>,
    actions: &dyn MessagingActions<R>,
    provider_kind: MessagingProviderKind,
    integration_id: String,
    headers: HeaderMap,
    raw: Vec<u8>,
) -> AppResult<IngestResponse> {
    let integration = config_service::messaging_integration(app, &integration_id)?;
    if !integration.enabled {
        return Err(AppError::new(format!(
            "messagingIntegrationId 已禁用: {integration_id}"
        )));
    }
    if integration.kind != provider_kind {
        return Err(AppError::new(format!(
            "messagingIntegrationId provider 不匹配: {integration_id}"
        )));
    }
    let provider = provider_for(provider_kind);
    match provider.verify(&headers, &raw, &integration)? {
        Verification::UrlVerification { challenge } => Ok(IngestResponse::Challenge { challenge }),
        Verification::Event => {
            let now = store::now_epoch();
            let event = provider.parse_event(&raw, &integration, now)?;
            let db = app.state::<Database>();
            let dedup = store::insert_dedup(db.inner(), &event)?;
            if delivery_should_process(&dedup) {
                if let store::DedupInsert::Inserted(id) = dedup {
                    process_and_mark(app, actions, db.inner(), id, &integration, &event).await?;
                }
            }
            Ok(IngestResponse::Ack)
        }
    }
}

/// Verified long-connection ingress funnel. The caller holds its generation fence while this
/// function re-reads current configuration and commits the durable delivery.
pub(crate) fn persist_verified_long_connection_event<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    connected_integration: &MessagingIntegration,
    event: &MessagingEvent,
) -> AppResult<()> {
    let current = config_service::messaging_integration(app, &connected_integration.id)?;
    validate_long_connection_event(&current, connected_integration, event)?;
    let db = app.state::<Database>();
    if let store::DedupInsert::Inserted(_) = store::insert_dedup(db.inner(), event)? {
        app.state::<MessagingEventWorker>().wake();
    }
    Ok(())
}

pub(crate) fn validate_current_feishu_long_connection<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    connected: &MessagingIntegration,
) -> AppResult<()> {
    validate_current_long_connection(app, connected, MessagingProviderKind::Feishu)
}

pub(crate) fn validate_current_dingtalk_long_connection<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    connected: &MessagingIntegration,
) -> AppResult<()> {
    validate_current_long_connection(app, connected, MessagingProviderKind::DingTalk)
}

fn validate_current_long_connection<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    connected: &MessagingIntegration,
    expected: MessagingProviderKind,
) -> AppResult<()> {
    let current = config_service::messaging_integration(app, &connected.id)?;
    validate_long_connection_integration(&current, connected, expected)
}

fn validate_long_connection_event(
    current: &MessagingIntegration,
    connected: &MessagingIntegration,
    event: &MessagingEvent,
) -> AppResult<()> {
    validate_long_connection_integration(current, connected, current.kind)?;
    if event.provider != current.kind || event.integration_id != current.id {
        return Err(AppError::new("长连接事件 provider 不匹配"));
    }
    Ok(())
}

fn validate_long_connection_integration(
    current: &MessagingIntegration,
    connected: &MessagingIntegration,
    expected: MessagingProviderKind,
) -> AppResult<()> {
    if !current.enabled {
        return Err(AppError::new("消息集成已禁用，拒绝长连接事件"));
    }
    if current.kind != expected || connected.kind != expected || connected.id != current.id {
        return Err(AppError::new("长连接事件 provider 不匹配"));
    }
    if !provider_for(expected).capability().supports_long_connection {
        return Err(AppError::new("该消息集成不支持长连接入站"));
    }
    let current_snapshot = serde_json::to_vec(current)
        .map_err(|error| AppError::new(format!("当前消息集成配置序列化失败: {error}")))?;
    let connected_snapshot = serde_json::to_vec(connected)
        .map_err(|error| AppError::new(format!("长连接消息集成配置序列化失败: {error}")))?;
    if current_snapshot != connected_snapshot {
        return Err(AppError::new("长连接配置 generation 已失效"));
    }
    Ok(())
}

fn delivery_should_process(dedup: &store::DedupInsert) -> bool {
    match dedup {
        store::DedupInsert::Inserted(_) => true,
        store::DedupInsert::Existing(_) => false,
    }
}

pub async fn replay<R: Runtime>(
    app: &tauri::AppHandle<R>,
    actions: &dyn MessagingActions<R>,
    id: i64,
) -> AppResult<()> {
    let db = app.state::<Database>();
    let entry = store::get_entry(db.inner(), id)?
        .ok_or_else(|| AppError::new(format!("messagingEventId 不存在: {id}")))?;
    let integration = config_service::messaging_integration(app, &entry.event.integration_id)?;
    if !integration.enabled {
        return Err(AppError::new(format!(
            "messagingIntegrationId 已禁用: {}",
            integration.id
        )));
    }
    if integration.kind != entry.event.provider {
        return Err(AppError::new("messagingEvent provider 与当前集成不匹配"));
    }
    ensure_replay_allowed(entry.status)?;
    process_and_mark(app, actions, db.inner(), id, &integration, &entry.event).await
}

fn ensure_replay_allowed(status: MessagingEventStatus) -> AppResult<()> {
    match status {
        MessagingEventStatus::Received | MessagingEventStatus::Failed => Ok(()),
        MessagingEventStatus::Processed => Err(AppError::new(
            "messagingEvent 已处理，不能重放；请只重放 failed/received 事件",
        )),
    }
}

async fn process_and_mark<R: Runtime>(
    app: &tauri::AppHandle<R>,
    actions: &dyn MessagingActions<R>,
    db: &Database,
    id: i64,
    integration: &MessagingIntegration,
    event: &MessagingEvent,
) -> AppResult<()> {
    let result = process_event(app, actions, integration, event).await;
    let now = store::now_epoch();
    match &result {
        Ok(outcome) => {
            if let Some(error) = &outcome.business_error {
                store::mark_failed_with_reply(db, id, error, outcome.reply.as_ref(), now)?;
            } else {
                store::mark_processed_with_reply(db, id, outcome.reply.as_ref(), now)?;
            }
        }
        Err(e) => store::mark_failed(db, id, &e.message, now)?,
    }
    result.map(|_| ())
}

pub async fn execute_reply<R: Runtime>(
    app: &tauri::AppHandle<R>,
    payload: MessagingReplyPayload,
) -> AppResult<ActionExecutionResult> {
    let integration = config_service::messaging_integration(app, &payload.integration_id)?;
    if !integration.enabled {
        return Ok(ActionExecutionResult::Dead {
            message: format!("消息集成「{}」已禁用，停止回复", integration.name),
        });
    }
    if integration.kind != payload.provider {
        return Ok(ActionExecutionResult::Dead {
            message: format!(
                "消息集成「{}」kind 已从 {:?} 改为 {:?}，停止回复",
                integration.name, payload.provider, integration.kind
            ),
        });
    }
    provider_for(integration.kind)
        .reply(&integration, &payload.target, &payload.text)
        .await
}

pub fn enqueue_send<R: Runtime>(
    app: &tauri::AppHandle<R>,
    actions: &dyn MessagingActions<R>,
    request: SendMessagingRequest,
) -> AppResult<SendMessagingResponse> {
    enqueue_send_after(app, actions, request, 0)
}

pub(crate) fn enqueue_send_after<R: Runtime>(
    app: &tauri::AppHandle<R>,
    actions: &dyn MessagingActions<R>,
    request: SendMessagingRequest,
    delay_secs: u64,
) -> AppResult<SendMessagingResponse> {
    enqueue_send_scoped_after(app, actions, request, delay_secs, false)
}

pub(crate) fn enqueue_send_once_after<R: Runtime>(
    app: &tauri::AppHandle<R>,
    actions: &dyn MessagingActions<R>,
    request: SendMessagingRequest,
    delay_secs: u64,
) -> AppResult<SendMessagingResponse> {
    enqueue_send_scoped_after(app, actions, request, delay_secs, true)
}

fn enqueue_send_scoped_after<R: Runtime>(
    app: &tauri::AppHandle<R>,
    actions: &dyn MessagingActions<R>,
    request: SendMessagingRequest,
    delay_secs: u64,
    all_status_dedupe: bool,
) -> AppResult<SendMessagingResponse> {
    let integration = config_service::messaging_integration(app, &request.integration_id)?;
    let prepared = prepare_send(&integration, request)?;
    let outbox_id = if all_status_dedupe {
        actions.enqueue_send_once_after(
            app,
            ActionKind::MessagingSend,
            &prepared.summary,
            &prepared.payload_json,
            &prepared.dedupe_key,
            delay_secs,
        )?
    } else {
        actions.enqueue_send_after(
            app,
            ActionKind::MessagingSend,
            &prepared.summary,
            &prepared.payload_json,
            &prepared.dedupe_key,
            delay_secs,
        )?
    };
    Ok(SendMessagingResponse { outbox_id })
}

pub(crate) struct PreparedSend {
    pub(crate) summary: String,
    pub(crate) payload_json: String,
    pub(crate) dedupe_key: String,
}

pub(crate) fn prepare_send(
    integration: &MessagingIntegration,
    request: SendMessagingRequest,
) -> AppResult<PreparedSend> {
    if !integration.enabled {
        return Err(AppError::new(format!(
            "消息集成「{}」已禁用，不能发送",
            integration.name
        )));
    }
    let conversation_id = request.conversation_id.trim();
    if !conversation_allowed(integration, conversation_id) {
        return Err(AppError::new(format!(
            "conversationId 未授权使用消息集成「{}」",
            integration.name
        )));
    }
    let request_id = request.request_id.trim();
    if request_id.is_empty() {
        return Err(AppError::new("messaging requestId 不能为空"));
    }
    let content = match request.content {
        MessagingSendContent::Text { text } => {
            let text = text.trim();
            if text.is_empty() {
                return Err(AppError::new("消息正文不能为空"));
            }
            MessagingSendContent::Text {
                text: crate::messaging::truncate_utf8_boundary(text, MAX_SEND_TEXT_BYTES),
            }
        }
        MessagingSendContent::Card {
            title,
            text,
            template,
        } => {
            if !provider_for(integration.kind)
                .capability()
                .supports_information_card
            {
                return Err(AppError::new(format!(
                    "信息卡片不受支持：消息集成「{}」是 {}",
                    integration.name,
                    integration.kind.as_wire()
                )));
            }
            let title = title.trim();
            if title.is_empty() {
                return Err(AppError::new("卡片标题不能为空"));
            }
            let text = text.trim();
            if text.is_empty() {
                return Err(AppError::new("卡片正文不能为空"));
            }
            MessagingSendContent::Card {
                title: crate::messaging::truncate_utf8_boundary(title, MAX_SEND_CARD_TITLE_BYTES),
                text: crate::messaging::truncate_utf8_boundary(text, MAX_SEND_TEXT_BYTES),
                template,
            }
        }
    };
    let payload = MessagingSendPayload {
        integration_id: integration.id.clone(),
        provider: integration.kind,
        conversation_id: conversation_id.to_string(),
        content,
    };
    let payload_json = serde_json::to_string(&payload)
        .map_err(|e| AppError::new(format!("messaging send payload 序列化失败: {e}")))?;
    let send_kind = match payload.content {
        MessagingSendContent::Text { .. } => "text",
        MessagingSendContent::Card { .. } => "card",
    };
    let summary = format!(
        "Messaging {send_kind} {} → {}",
        payload.provider.as_wire(),
        payload.conversation_id
    );
    let dedupe_key = active_send_dedupe_key(&integration.id, request_id);
    Ok(PreparedSend {
        summary,
        payload_json,
        dedupe_key,
    })
}

fn active_send_dedupe_key(integration_id: &str, request_id: &str) -> String {
    format!(
        "messaging-send:{}:{}",
        integration_id.trim(),
        request_id.trim()
    )
}

pub async fn execute_send<R: Runtime>(
    app: &tauri::AppHandle<R>,
    payload: MessagingSendPayload,
) -> AppResult<ActionExecutionResult> {
    let integration = match config_service::messaging_integration(app, &payload.integration_id) {
        Ok(integration) => integration,
        Err(e) if e.message.contains("messagingIntegrationId 不存在") => {
            return Ok(ActionExecutionResult::Dead { message: e.message });
        }
        Err(e) => return Err(e),
    };
    if !integration.enabled {
        return Ok(ActionExecutionResult::Dead {
            message: format!("消息集成「{}」已禁用，停止发送", integration.name),
        });
    }
    if integration.kind != payload.provider {
        return Ok(ActionExecutionResult::Dead {
            message: format!(
                "消息集成「{}」kind 已从 {:?} 改为 {:?}，停止发送",
                integration.name, payload.provider, integration.kind
            ),
        });
    }
    if !conversation_allowed(&integration, &payload.conversation_id) {
        return Ok(ActionExecutionResult::Dead {
            message: format!("conversationId 未授权使用消息集成「{}」", integration.name),
        });
    }
    provider_for(integration.kind)
        .send(&integration, &payload.conversation_id, &payload.content)
        .await
}

async fn process_event<R: Runtime>(
    app: &tauri::AppHandle<R>,
    actions: &dyn MessagingActions<R>,
    integration: &MessagingIntegration,
    event: &MessagingEvent,
) -> AppResult<ProcessingOutcome> {
    if !conversation_allowed(integration, &event.conversation_id) {
        let reply = enqueue_reply(
            actions,
            app,
            integration,
            event,
            ReplyKind::Unauthorized,
            "当前会话未授权使用 prmonitor。",
        )?;
        return Ok(ProcessingOutcome::with_reply(reply));
    }
    let parsed = parse_command(&event.text);
    if integration.require_mention
        && !event.mentioned_bot
        && !parsed.as_ref().is_ok_and(command_allowed_without_mention)
    {
        return Ok(ProcessingOutcome::default());
    }
    let command = match parsed {
        Ok(command) => command,
        Err(e) => {
            let reply = enqueue_reply(
                actions,
                app,
                integration,
                event,
                ReplyKind::InvalidCommand,
                &format!("{}\n{}", e.message, help_text()),
            )?;
            return Ok(ProcessingOutcome::with_reply(reply));
        }
    };
    match command {
        MessagingCommand::Help => {
            let reply = enqueue_reply(
                actions,
                app,
                integration,
                event,
                ReplyKind::Help,
                help_text(),
            )?;
            Ok(ProcessingOutcome::with_reply(reply))
        }
        MessagingCommand::Status => {
            let status = passive_status(app)?;
            let reply =
                enqueue_reply(actions, app, integration, event, ReplyKind::Status, &status)?;
            Ok(ProcessingOutcome::with_reply(reply))
        }
        MessagingCommand::Answer { request_id, value } => {
            let db = app.state::<Database>();
            let broker = app.state::<crate::messaging::human_input::HumanInputBroker>();
            let request = crate::messaging::human_input::get(db.inner(), &request_id)?
                .ok_or_else(|| AppError::new(format!("人工输入请求不存在: {request_id}")))?;
            if request.integration_id != integration.id
                || request.conversation_id != event.conversation_id
            {
                return Err(AppError::new("人工输入请求不属于当前集成/会话"));
            }
            let mut answers = value
                .split(';')
                .filter_map(|part| {
                    let (question_id, answer) = part.split_once('=')?;
                    request
                        .questions
                        .iter()
                        .any(|question| question.id == question_id.trim())
                        .then(|| crate::messaging::human_input::HumanAnswer {
                            question_id: question_id.trim().to_string(),
                            answer: answer.trim().to_string(),
                        })
                })
                .collect::<Vec<_>>();
            if answers.is_empty() {
                let question_id = request
                    .questions
                    .first()
                    .map(|q| q.id.clone())
                    .unwrap_or_else(|| "q1".into());
                answers.push(crate::messaging::human_input::HumanAnswer {
                    question_id,
                    answer: value,
                });
            }
            let answer_source = match event.provider {
                MessagingProviderKind::Feishu => {
                    crate::messaging::human_input::HumanAnswerSource::Feishu
                }
                MessagingProviderKind::DingTalk => {
                    crate::messaging::human_input::HumanAnswerSource::DingTalk
                }
                MessagingProviderKind::WeChatWork => {
                    return Err(AppError::new(
                        "企业微信不支持长连接 /answer；请使用支持互动问答的通道",
                    ));
                }
            };
            let outcome = crate::messaging::human_input::answer(
                db.inner(),
                broker.inner(),
                &request_id,
                &answers,
                answer_source,
                store::now_epoch(),
            )?;
            let text = match outcome {
                crate::messaging::human_input::AnswerOutcome::Won => {
                    "已提交，Codex 将继续执行。".to_string()
                }
                crate::messaging::human_input::AnswerOutcome::AlreadyAnswered { source } => {
                    format!(
                        "该问题已由 {} 回答。",
                        source
                            .map(|source| source.to_string())
                            .unwrap_or_else(|| "其他通道".into())
                    )
                }
            };
            let reply = enqueue_reply(
                actions,
                app,
                integration,
                event,
                ReplyKind::HumanInput,
                &text,
            )?;
            Ok(ProcessingOutcome::with_reply(reply))
        }
        MessagingCommand::Cancel { request_id } => {
            let db = app.state::<Database>();
            let broker = app.state::<crate::messaging::human_input::HumanInputBroker>();
            let request = crate::messaging::human_input::get(db.inner(), &request_id)?
                .ok_or_else(|| AppError::new(format!("人工输入请求不存在: {request_id}")))?;
            if request.integration_id != integration.id
                || request.conversation_id != event.conversation_id
            {
                return Err(AppError::new("人工输入请求不属于当前集成/会话"));
            }
            let cancelled = crate::messaging::human_input::cancel(
                db.inner(),
                broker.inner(),
                &request_id,
                store::now_epoch(),
            )?;
            let reply = enqueue_reply(
                actions,
                app,
                integration,
                event,
                ReplyKind::HumanInput,
                if cancelled {
                    "已取消人工输入请求。"
                } else {
                    "该请求已结束，无法取消。"
                },
            )?;
            Ok(ProcessingOutcome::with_reply(reply))
        }
        MessagingCommand::Review {
            reference,
            pr_number,
            kind,
        } => {
            let request_id = messaging_review_request_id(event);
            let outcome = match actions.submit_review(
                app,
                reference.clone(),
                pr_number,
                kind.as_model(),
                request_id,
            ) {
                Ok(receipt_id) => {
                    let reply = enqueue_reply(
                        actions,
                        app,
                        integration,
                        event,
                        ReplyKind::ReviewQueued,
                        &format!(
                            "已入队 {mode}：{reference} #{pr_number}\nReceipt: {}",
                            receipt_id.get(),
                            mode = kind.as_wire()
                        ),
                    )?;
                    ProcessingOutcome::with_reply(reply)
                }
                Err(e) => {
                    let reply = enqueue_reply(
                        actions,
                        app,
                        integration,
                        event,
                        ReplyKind::ReviewFailed,
                        &format!("review 入队失败：{}", e.message),
                    )?;
                    ProcessingOutcome {
                        reply: Some(reply),
                        business_error: Some(e.message),
                    }
                }
            };
            Ok(outcome)
        }
    }
}

fn messaging_review_request_id(event: &MessagingEvent) -> ExternalRequestId {
    let provider_message_id = if event.thread_id.trim().is_empty() {
        event.event_id.as_str()
    } else {
        event.thread_id.as_str()
    };
    let digest = Sha256::digest(
        format!(
            "{}\0{}\0{}",
            event.provider.as_wire(),
            event.integration_id,
            provider_message_id
        )
        .as_bytes(),
    );
    ExternalRequestId::parse(hex::encode(&digest[..16]))
        .expect("SHA-256 prefix is lowercase hexadecimal")
}

pub(crate) fn provider_for(kind: MessagingProviderKind) -> &'static dyn MessagingProvider {
    match kind {
        MessagingProviderKind::Feishu => &FeishuProvider,
        MessagingProviderKind::WeChatWork => &WeChatWorkProvider,
        MessagingProviderKind::DingTalk => &DingTalkProvider,
    }
}

pub(crate) fn supports_long_connection(kind: MessagingProviderKind) -> bool {
    provider_for(kind).capability().supports_long_connection
}

pub(crate) fn long_connection_http_gone_message(kind: MessagingProviderKind) -> &'static str {
    match kind {
        MessagingProviderKind::Feishu => "飞书 HTTP 回调已禁用；请在飞书开放平台启用官方长连接",
        MessagingProviderKind::DingTalk => "钉钉 HTTP 回调已禁用；请在钉钉开放平台启用 Stream 模式",
        MessagingProviderKind::WeChatWork => "该 messaging provider 未启用长连接 HTTP 410",
    }
}

fn conversation_allowed(integration: &MessagingIntegration, conversation_id: &str) -> bool {
    let conversation_id = conversation_id.trim();
    integration
        .allowed_conversation_ids
        .iter()
        .any(|allowed| allowed.trim() == conversation_id)
}

fn parse_command(text: &str) -> AppResult<MessagingCommand> {
    let mut parts = text.split_whitespace();
    let Some(command) = parts.next() else {
        return Err(AppError::new("消息命令不能为空"));
    };
    match command {
        "/help" => Ok(MessagingCommand::Help),
        "/status" => Ok(MessagingCommand::Status),
        "/answer" => {
            let request_id = parts
                .next()
                .ok_or_else(|| AppError::new("/answer 需要 Q-id"))?
                .to_string();
            let value = parts.collect::<Vec<_>>().join(" ");
            if value.trim().is_empty() {
                return Err(AppError::new("/answer 需要答案"));
            }
            Ok(MessagingCommand::Answer { request_id, value })
        }
        "/cancel" => {
            let request_id = parts
                .next()
                .ok_or_else(|| AppError::new("/cancel 需要 Q-id"))?
                .to_string();
            if parts.next().is_some() {
                return Err(AppError::new("/cancel 只接受一个 Q-id"));
            }
            Ok(MessagingCommand::Cancel { request_id })
        }
        "/review" => {
            let reference = parts
                .next()
                .ok_or_else(|| AppError::new("/review 需要项目 id 或 repo"))?
                .to_string();
            let pr_number = parts
                .next()
                .ok_or_else(|| AppError::new("/review 需要 PR 号"))?
                .parse::<u64>()
                .map_err(|_| AppError::new("/review PR 号必须是正整数"))?;
            if pr_number == 0 {
                return Err(AppError::new("/review PR 号必须大于 0"));
            }
            let mut kind = ReviewCommandKind::Review;
            for part in parts {
                match part {
                    "--check" => kind = ReviewCommandKind::Check,
                    other => {
                        return Err(AppError::new(format!(
                            "/review 参数不支持: {other}（仅支持 --check）"
                        )));
                    }
                }
            }
            Ok(MessagingCommand::Review {
                reference,
                pr_number,
                kind,
            })
        }
        _ => Err(AppError::new(format!("未知命令：{command}"))),
    }
}

fn command_allowed_without_mention(command: &MessagingCommand) -> bool {
    matches!(
        command,
        MessagingCommand::Answer { .. } | MessagingCommand::Cancel { .. }
    )
}

fn passive_status<R: Runtime>(app: &tauri::AppHandle<R>) -> AppResult<String> {
    let cfg = config_service::load(app)?;
    let state = app.state::<AppState>();
    let codex = if state.codex.is_stopped() {
        "codex: stopped"
    } else {
        "codex: desired-running"
    };
    Ok(format!(
        "prmonitor 状态\n项目数：{}\n消息集成：{}\n{}",
        cfg.projects.len(),
        cfg.messaging
            .integrations
            .iter()
            .filter(|i| i.enabled)
            .count(),
        codex
    ))
}

fn help_text() -> &'static str {
    "可用命令：\n/help\n/status\n/review <project|repo> <pr-number> [--check]\n/answer <Q-id> <答案>\n/cancel <Q-id>"
}

#[derive(Debug, Clone, Copy)]
enum ReplyKind {
    Unauthorized,
    InvalidCommand,
    Help,
    Status,
    ReviewQueued,
    ReviewFailed,
    HumanInput,
}

#[derive(Debug, Clone, Default)]
struct ProcessingOutcome {
    reply: Option<store::ReplyRecord>,
    business_error: Option<String>,
}

impl ProcessingOutcome {
    fn with_reply(reply: store::ReplyRecord) -> Self {
        Self {
            reply: Some(reply),
            business_error: None,
        }
    }
}

impl ReplyKind {
    fn as_wire(self) -> &'static str {
        match self {
            ReplyKind::Unauthorized => "unauthorized",
            ReplyKind::InvalidCommand => "invalid-command",
            ReplyKind::Help => "help",
            ReplyKind::Status => "status",
            ReplyKind::ReviewQueued => "review-queued",
            ReplyKind::ReviewFailed => "review-failed",
            ReplyKind::HumanInput => "human-input",
        }
    }
}

fn enqueue_reply<R: Runtime>(
    actions: &dyn MessagingActions<R>,
    app: &tauri::AppHandle<R>,
    integration: &MessagingIntegration,
    event: &MessagingEvent,
    kind: ReplyKind,
    text: &str,
) -> AppResult<store::ReplyRecord> {
    let payload = MessagingReplyPayload {
        integration_id: integration.id.clone(),
        provider: integration.kind,
        target: MessagingReplyTarget {
            conversation_id: event.conversation_id.clone(),
            message_id: event.thread_id.clone(),
            thread_id: event.thread_id.clone(),
        },
        text: text.to_string(),
    };
    let payload_json = serde_json::to_string(&payload)
        .map_err(|e| AppError::new(format!("messaging reply payload 序列化失败: {e}")))?;
    let kind_wire = kind.as_wire();
    let dedupe_key = format!(
        "reply:{}:{}:{}",
        event.provider.as_wire(),
        event.event_id,
        kind_wire
    );
    let summary = format!("Messaging reply {kind_wire}");
    let outbox_id = actions.enqueue_reply(
        app,
        &integration.id,
        ActionKind::MessagingReply,
        &summary,
        &payload_json,
        &dedupe_key,
    )?;
    Ok(store::ReplyRecord {
        outbox_id,
        kind: kind_wire.to_string(),
        summary,
    })
}

const MAX_SEND_TEXT_BYTES: usize = 4096;
const MAX_SEND_CARD_TITLE_BYTES: usize = 128;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn parse_review_command_requires_ref_and_positive_pr() {
        assert_eq!(
            parse_command("/review repo/name 7 --check").expect("parse"),
            MessagingCommand::Review {
                reference: "repo/name".to_string(),
                pr_number: 7,
                kind: ReviewCommandKind::Check,
            }
        );
        assert!(parse_command("/review repo/name 0").is_err());
        assert!(parse_command("/review repo/name abc").is_err());
        assert!(parse_command("/review repo/name 7 --bad").is_err());
    }

    #[test]
    fn parse_only_known_commands() {
        assert_eq!(
            parse_command("/help").expect("help"),
            MessagingCommand::Help
        );
        assert_eq!(
            parse_command("/status").expect("status"),
            MessagingCommand::Status
        );
        assert!(parse_command("review repo 1").is_err());
    }

    #[test]
    fn human_input_recovery_commands_bypass_mention_gate_only() {
        assert!(command_allowed_without_mention(
            &parse_command("/answer Q-1 yes").unwrap()
        ));
        assert!(command_allowed_without_mention(
            &parse_command("/cancel Q-1").unwrap()
        ));
        assert!(!command_allowed_without_mention(
            &parse_command("/status").unwrap()
        ));
    }

    #[test]
    fn long_connection_event_rejects_provider_replacement() {
        let connected = MessagingIntegration {
            id: "fs".into(),
            enabled: true,
            ..MessagingIntegration::feishu_default()
        };
        let mut current = connected.clone();
        let event = MessagingEvent {
            provider: MessagingProviderKind::Feishu,
            integration_id: "fs".into(),
            event_id: "evt".into(),
            conversation_id: "chat".into(),
            thread_id: "msg".into(),
            sender_id: "u".into(),
            text: "/help".into(),
            mentioned_bot: true,
            raw_payload: "{}".into(),
            received_at_epoch: 1,
        };
        assert!(validate_long_connection_event(&current, &connected, &event).is_ok());
        current.app_secret = "rotated".into();
        assert!(validate_long_connection_event(&current, &connected, &event).is_err());
        current = connected.clone();
        current.kind = MessagingProviderKind::DingTalk;
        assert!(validate_long_connection_event(&current, &connected, &event).is_err());
    }

    #[test]
    fn long_connection_accepts_dingtalk_and_rejects_wechat() {
        let connected = MessagingIntegration {
            id: "dt".into(),
            kind: MessagingProviderKind::DingTalk,
            enabled: true,
            app_id: "app".into(),
            app_secret: "secret".into(),
            bot_open_id: "robot".into(),
            card_template_id: "tpl".into(),
            ..MessagingIntegration::feishu_default()
        };
        let event = MessagingEvent {
            provider: MessagingProviderKind::DingTalk,
            integration_id: "dt".into(),
            event_id: "evt".into(),
            conversation_id: "chat".into(),
            thread_id: "msg".into(),
            sender_id: "u".into(),
            text: "/help".into(),
            mentioned_bot: true,
            raw_payload: "{}".into(),
            received_at_epoch: 1,
        };
        assert!(validate_long_connection_event(&connected, &connected, &event).is_ok());
        assert!(
            provider_for(MessagingProviderKind::DingTalk)
                .capability()
                .supports_long_connection
        );
        assert!(
            !provider_for(MessagingProviderKind::WeChatWork)
                .capability()
                .supports_long_connection
        );
        let wechat = MessagingIntegration {
            id: "ww".into(),
            kind: MessagingProviderKind::WeChatWork,
            enabled: true,
            ..MessagingIntegration::feishu_default()
        };
        let wechat_event = MessagingEvent {
            provider: MessagingProviderKind::WeChatWork,
            integration_id: "ww".into(),
            ..event.clone()
        };
        assert!(validate_long_connection_event(&wechat, &wechat, &wechat_event).is_err());
    }

    #[test]
    fn replay_allows_only_unprocessed_events() {
        assert!(ensure_replay_allowed(MessagingEventStatus::Received).is_ok());
        assert!(ensure_replay_allowed(MessagingEventStatus::Failed).is_ok());
        assert!(ensure_replay_allowed(MessagingEventStatus::Processed).is_err());
    }

    #[test]
    fn reply_payload_excludes_provider_secrets() {
        let payload = MessagingReplyPayload {
            integration_id: "fs".to_string(),
            provider: MessagingProviderKind::Feishu,
            target: MessagingReplyTarget {
                conversation_id: "chat".to_string(),
                message_id: "msg".to_string(),
                thread_id: "msg".to_string(),
            },
            text: "ok".to_string(),
        };
        let json = serde_json::to_string(&payload).expect("serialize");
        for denied in [
            "secret",
            "token",
            "appSecret",
            "encryptKey",
            "authorization",
        ] {
            assert!(
                !json.contains(denied),
                "reply payload must not contain {denied}: {json}"
            );
        }
    }

    #[test]
    fn duplicate_delivery_never_reprocesses() {
        let event = MessagingEvent {
            provider: MessagingProviderKind::Feishu,
            integration_id: "fs".to_string(),
            event_id: "evt".to_string(),
            conversation_id: "chat".to_string(),
            thread_id: "msg".to_string(),
            sender_id: "u".to_string(),
            text: "/help".to_string(),
            mentioned_bot: true,
            raw_payload: "{}".to_string(),
            received_at_epoch: 1,
        };
        assert!(delivery_should_process(&store::DedupInsert::Inserted(1)));
        assert!(!delivery_should_process(&store::DedupInsert::Existing(
            Box::new(crate::model::MessagingEventEntry {
                id: 1,
                event,
                status: MessagingEventStatus::Failed,
                processed_at_epoch: Some(2),
                error: Some("boom".to_string()),
                reply: None,
            })
        )));
    }

    struct FailingReviewActions {
        enqueued: AtomicUsize,
    }

    impl<R: Runtime> MessagingActions<R> for FailingReviewActions {
        fn enqueue_reply(
            &self,
            _app: &tauri::AppHandle<R>,
            _integration_id: &str,
            _kind: ActionKind,
            _summary: &str,
            _payload_json: &str,
            _dedupe_key: &str,
        ) -> AppResult<i64> {
            self.enqueued.fetch_add(1, Ordering::SeqCst);
            Ok(42)
        }

        fn enqueue_send(
            &self,
            _app: &tauri::AppHandle<R>,
            _kind: ActionKind,
            _summary: &str,
            _payload_json: &str,
            _dedupe_key: &str,
        ) -> AppResult<i64> {
            self.enqueued.fetch_add(1, Ordering::SeqCst);
            Ok(43)
        }

        fn enqueue_send_after(
            &self,
            app: &tauri::AppHandle<R>,
            kind: ActionKind,
            summary: &str,
            payload_json: &str,
            dedupe_key: &str,
            _delay_secs: u64,
        ) -> AppResult<i64> {
            self.enqueue_send(app, kind, summary, payload_json, dedupe_key)
        }

        fn enqueue_send_once_after(
            &self,
            app: &tauri::AppHandle<R>,
            kind: ActionKind,
            summary: &str,
            payload_json: &str,
            dedupe_key: &str,
            delay_secs: u64,
        ) -> AppResult<i64> {
            self.enqueue_send_after(app, kind, summary, payload_json, dedupe_key, delay_secs)
        }

        fn list_sends(
            &self,
            _app: &tauri::AppHandle<R>,
            _integration_id: Option<&str>,
        ) -> AppResult<Vec<OutboxEntry>> {
            Ok(Vec::new())
        }

        fn submit_review(
            &self,
            _app: &tauri::AppHandle<R>,
            _reference: String,
            _pr_number: u64,
            _kind: ReviewKind,
            _request_id: ExternalRequestId,
        ) -> AppResult<ReviewReceiptId> {
            Err(AppError::new("review boom"))
        }
    }

    #[tokio::test]
    async fn durable_worker_drains_multiple_pages_and_terminalizes_missing_integration() {
        let app = tauri::test::mock_app();
        let db = Database::open_in_memory().expect("open");
        let config = serde_json::json!({
            "projects": [],
            "activeProjectId": "",
            "messaging": { "integrations": [{
                "id": "fs", "name": "Feishu", "kind": "feishu", "enabled": true,
                "appId": "cli_test", "appSecret": "secret", "botOpenId": "bot",
                "allowedConversationIds": ["chat"], "requireMention": true
            }]}
        });
        db.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO config_blob (id, json) VALUES (1, ?1)",
                [config.to_string()],
            )?;
            Ok(())
        })
        .expect("seed config");
        for index in 0..501 {
            let event = MessagingEvent {
                provider: MessagingProviderKind::Feishu,
                integration_id: "fs".into(),
                event_id: format!("evt-{index}"),
                conversation_id: "chat".into(),
                thread_id: format!("msg-{index}"),
                sender_id: "u".into(),
                text: "/help".into(),
                mentioned_bot: false,
                raw_payload: "{}".into(),
                received_at_epoch: 1,
            };
            store::insert_dedup(&db, &event).expect("insert");
        }
        let missing = MessagingEvent {
            provider: MessagingProviderKind::Feishu,
            integration_id: "missing".into(),
            event_id: "evt-missing".into(),
            conversation_id: "chat".into(),
            thread_id: "msg-missing".into(),
            sender_id: "u".into(),
            text: "/help".into(),
            mentioned_bot: false,
            raw_payload: "{}".into(),
            received_at_epoch: 1,
        };
        let missing_id = match store::insert_dedup(&db, &missing).expect("insert") {
            store::DedupInsert::Inserted(id) => id,
            store::DedupInsert::Existing(_) => panic!("new event"),
        };
        app.manage(db);
        app.manage(MessagingRuntime::<tauri::test::MockRuntime> {
            actions: Arc::new(FailingReviewActions {
                enqueued: AtomicUsize::new(0),
            }),
        });

        drain_received(app.handle()).await.expect("drain");

        let db = app.state::<Database>();
        assert!(store::received_ids(db.inner())
            .expect("received")
            .is_empty());
        assert_eq!(
            store::get_entry(db.inner(), missing_id)
                .expect("get")
                .expect("entry")
                .status,
            MessagingEventStatus::Failed
        );
    }

    #[tokio::test]
    async fn review_business_failure_marks_failed_but_acks_delivery() {
        let app = tauri::test::mock_app().handle().clone();
        let db = Database::open_in_memory().expect("open");
        let integration = MessagingIntegration {
            id: "fs".to_string(),
            name: "Feishu".to_string(),
            allowed_conversation_ids: vec!["chat".to_string()],
            require_mention: true,
            ..MessagingIntegration::feishu_default()
        };
        let event = MessagingEvent {
            provider: MessagingProviderKind::Feishu,
            integration_id: "fs".to_string(),
            event_id: "evt".to_string(),
            conversation_id: "chat".to_string(),
            thread_id: "msg".to_string(),
            sender_id: "u".to_string(),
            text: "/review repo/name 7".to_string(),
            mentioned_bot: true,
            raw_payload: "{}".to_string(),
            received_at_epoch: 1,
        };
        let id = match store::insert_dedup(&db, &event).expect("insert") {
            store::DedupInsert::Inserted(id) => id,
            store::DedupInsert::Existing(_) => panic!("first insert must be new"),
        };
        let actions = FailingReviewActions {
            enqueued: AtomicUsize::new(0),
        };

        process_and_mark(&app, &actions, &db, id, &integration, &event)
            .await
            .expect("business failure is acked");

        assert_eq!(actions.enqueued.load(Ordering::SeqCst), 1);
        let entry = store::get_entry(&db, id).expect("get").expect("entry");
        assert_eq!(entry.status, MessagingEventStatus::Failed);
        assert_eq!(entry.error.as_deref(), Some("review boom"));
        let reply = entry.reply.expect("reply");
        assert_eq!(reply.outbox_id, 42);
        assert_eq!(reply.kind, "review-failed");
    }

    #[test]
    fn review_request_id_is_stable_per_provider_message() {
        let event = MessagingEvent {
            provider: MessagingProviderKind::Feishu,
            integration_id: "fs".to_string(),
            event_id: "delivery-1".to_string(),
            conversation_id: "chat".to_string(),
            thread_id: "provider-message-1".to_string(),
            sender_id: "u".to_string(),
            text: "/review repo/name 7".to_string(),
            mentioned_bot: true,
            raw_payload: "{}".to_string(),
            received_at_epoch: 1,
        };
        let first = messaging_review_request_id(&event);
        assert_eq!(first, messaging_review_request_id(&event));
        assert_eq!(first.as_str().len(), 32);

        let mut another = event;
        another.thread_id = "provider-message-2".to_string();
        assert_ne!(first, messaging_review_request_id(&another));
    }

    struct CapturingSendActions {
        dedupe_key: std::sync::Mutex<Option<String>>,
    }

    impl<R: Runtime> MessagingActions<R> for CapturingSendActions {
        fn enqueue_reply(
            &self,
            _app: &tauri::AppHandle<R>,
            _integration_id: &str,
            _kind: ActionKind,
            _summary: &str,
            _payload_json: &str,
            _dedupe_key: &str,
        ) -> AppResult<i64> {
            Ok(42)
        }

        fn enqueue_send(
            &self,
            _app: &tauri::AppHandle<R>,
            _kind: ActionKind,
            _summary: &str,
            _payload_json: &str,
            dedupe_key: &str,
        ) -> AppResult<i64> {
            *self.dedupe_key.lock().expect("lock") = Some(dedupe_key.to_string());
            Ok(43)
        }

        fn enqueue_send_after(
            &self,
            app: &tauri::AppHandle<R>,
            kind: ActionKind,
            summary: &str,
            payload_json: &str,
            dedupe_key: &str,
            _delay_secs: u64,
        ) -> AppResult<i64> {
            self.enqueue_send(app, kind, summary, payload_json, dedupe_key)
        }

        fn enqueue_send_once_after(
            &self,
            app: &tauri::AppHandle<R>,
            kind: ActionKind,
            summary: &str,
            payload_json: &str,
            dedupe_key: &str,
            delay_secs: u64,
        ) -> AppResult<i64> {
            self.enqueue_send_after(app, kind, summary, payload_json, dedupe_key, delay_secs)
        }

        fn list_sends(
            &self,
            _app: &tauri::AppHandle<R>,
            _integration_id: Option<&str>,
        ) -> AppResult<Vec<OutboxEntry>> {
            Ok(Vec::new())
        }

        fn submit_review(
            &self,
            _app: &tauri::AppHandle<R>,
            _reference: String,
            _pr_number: u64,
            _kind: ReviewKind,
            _request_id: ExternalRequestId,
        ) -> AppResult<ReviewReceiptId> {
            ReviewReceiptId::new(17).map_err(AppError::new)
        }
    }

    #[tokio::test]
    async fn successful_review_command_replies_with_queued_receipt() {
        let app = tauri::test::mock_app().handle().clone();
        let db = Database::open_in_memory().expect("open");
        let integration = MessagingIntegration {
            id: "fs".to_string(),
            name: "Feishu".to_string(),
            allowed_conversation_ids: vec!["chat".to_string()],
            require_mention: true,
            ..MessagingIntegration::feishu_default()
        };
        let event = MessagingEvent {
            provider: MessagingProviderKind::Feishu,
            integration_id: "fs".to_string(),
            event_id: "evt".to_string(),
            conversation_id: "chat".to_string(),
            thread_id: "provider-message".to_string(),
            sender_id: "u".to_string(),
            text: "/review repo/name 7".to_string(),
            mentioned_bot: true,
            raw_payload: "{}".to_string(),
            received_at_epoch: 1,
        };
        let id = match store::insert_dedup(&db, &event).expect("insert") {
            store::DedupInsert::Inserted(id) => id,
            store::DedupInsert::Existing(_) => panic!("first insert must be new"),
        };
        let actions = CapturingSendActions {
            dedupe_key: std::sync::Mutex::new(None),
        };

        process_and_mark(&app, &actions, &db, id, &integration, &event)
            .await
            .expect("queued receipt is acked");

        let entry = store::get_entry(&db, id).expect("get").expect("entry");
        assert_eq!(entry.status, MessagingEventStatus::Processed);
        assert_eq!(entry.reply.expect("reply").kind, "review-queued");
    }

    #[test]
    fn active_send_uses_request_id_as_pending_dedupe_key() {
        let app = tauri::test::mock_app();
        let db = Database::open_in_memory().expect("open");
        let config = serde_json::json!({
            "projects": [],
            "activeProjectId": "",
            "messaging": {
                "integrations": [{
                    "id": "fs",
                    "name": "Feishu",
                    "kind": "feishu",
                    "enabled": true,
                    "allowedConversationIds": ["chat"]
                }]
            }
        });
        db.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO config_blob (id, json) VALUES (1, ?1)",
                [config.to_string()],
            )?;
            Ok(())
        })
        .expect("seed config");
        app.manage(db);
        let actions = CapturingSendActions {
            dedupe_key: std::sync::Mutex::new(None),
        };

        let response = enqueue_send(
            app.handle(),
            &actions,
            SendMessagingRequest {
                integration_id: "fs".to_string(),
                conversation_id: " chat ".to_string(),
                content: MessagingSendContent::Text {
                    text: " hello ".to_string(),
                },
                request_id: "req-123".to_string(),
            },
        )
        .expect("enqueue send");

        assert_eq!(response.outbox_id, 43);
        assert_eq!(
            actions.dedupe_key.lock().expect("lock").as_deref(),
            Some("messaging-send:fs:req-123")
        );
    }

    #[test]
    fn prepare_send_accepts_feishu_card_and_truncates_utf8_boundaries() {
        let integration = MessagingIntegration {
            id: "fs".to_string(),
            name: "Feishu".to_string(),
            allowed_conversation_ids: vec!["chat".to_string()],
            enabled: true,
            ..MessagingIntegration::feishu_default()
        };
        let prepared = prepare_send(
            &integration,
            SendMessagingRequest {
                integration_id: "fs".to_string(),
                conversation_id: "chat".to_string(),
                content: MessagingSendContent::Card {
                    title: "标".repeat(100),
                    text: "文".repeat(2000),
                    template: MessagingCardTemplate::Blue,
                },
                request_id: "req-card".to_string(),
            },
        )
        .expect("prepare card");
        let payload: MessagingSendPayload =
            serde_json::from_str(&prepared.payload_json).expect("payload");
        let MessagingSendContent::Card { title, text, .. } = payload.content else {
            panic!("expected card");
        };
        assert!(title.len() <= MAX_SEND_CARD_TITLE_BYTES);
        assert!(text.len() <= MAX_SEND_TEXT_BYTES);
        assert!(title.is_char_boundary(title.len()));
        assert!(text.is_char_boundary(text.len()));
    }

    #[test]
    fn prepare_send_rejects_cards_for_unsupported_providers_and_empty_fields() {
        let integration = MessagingIntegration {
            id: "wecom".to_string(),
            name: "weChatWork".to_string(),
            kind: MessagingProviderKind::WeChatWork,
            allowed_conversation_ids: vec!["chat".to_string()],
            enabled: true,
            ..MessagingIntegration::feishu_default()
        };
        let error = match prepare_send(
            &integration,
            SendMessagingRequest {
                integration_id: integration.id.clone(),
                conversation_id: "chat".to_string(),
                content: MessagingSendContent::Card {
                    title: "title".to_string(),
                    text: "body".to_string(),
                    template: MessagingCardTemplate::Blue,
                },
                request_id: "req-card".to_string(),
            },
        ) {
            Ok(_) => panic!("WeChat Work cards must fail before enqueue"),
            Err(error) => error,
        };
        assert!(
            error.message.contains("信息卡片不受支持"),
            "{}",
            error.message
        );

        let dingtalk = MessagingIntegration {
            id: "dingtalk".to_string(),
            name: "dingTalk".to_string(),
            kind: MessagingProviderKind::DingTalk,
            allowed_conversation_ids: vec!["chat".to_string()],
            enabled: true,
            app_id: "app".into(),
            app_secret: "secret".into(),
            bot_open_id: "robot".into(),
            card_template_id: "tpl".into(),
            ..MessagingIntegration::feishu_default()
        };
        prepare_send(
            &dingtalk,
            SendMessagingRequest {
                integration_id: dingtalk.id.clone(),
                conversation_id: "chat".to_string(),
                content: MessagingSendContent::Card {
                    title: "title".to_string(),
                    text: "body".to_string(),
                    template: MessagingCardTemplate::Blue,
                },
                request_id: "req-dt-card".to_string(),
            },
        )
        .expect("DingTalk information cards must enqueue");

        let integration = MessagingIntegration {
            id: "fs".to_string(),
            name: "Feishu".to_string(),
            allowed_conversation_ids: vec!["chat".to_string()],
            enabled: true,
            ..MessagingIntegration::feishu_default()
        };
        for content in [
            MessagingSendContent::Card {
                title: " ".to_string(),
                text: "body".to_string(),
                template: MessagingCardTemplate::Blue,
            },
            MessagingSendContent::Card {
                title: "title".to_string(),
                text: " \n ".to_string(),
                template: MessagingCardTemplate::Blue,
            },
        ] {
            assert!(prepare_send(
                &integration,
                SendMessagingRequest {
                    integration_id: "fs".to_string(),
                    conversation_id: "chat".to_string(),
                    content,
                    request_id: "req-card".to_string(),
                },
            )
            .is_err());
        }
    }
}
