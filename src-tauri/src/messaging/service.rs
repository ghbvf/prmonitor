//! Messaging ingress + command processing (#1559).

use std::sync::Arc;

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
    MessagingProviderKind, MessagingReplyPayload, MessagingReplyTarget, MessagingSendPayload,
    OutboxEntry, ReviewKind, ReviewReceiptId, SendMessagingRequest, SendMessagingResponse,
};
use crate::state::AppState;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestResponse {
    Challenge { challenge: String },
    Ack,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MessagingCommand {
    Help,
    Status,
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
    let text = request.text.trim();
    if text.is_empty() {
        return Err(AppError::new("消息正文不能为空"));
    }
    let request_id = request.request_id.trim();
    if request_id.is_empty() {
        return Err(AppError::new("messaging requestId 不能为空"));
    }
    let payload = MessagingSendPayload {
        integration_id: integration.id.clone(),
        provider: integration.kind,
        conversation_id: conversation_id.to_string(),
        text: crate::messaging::truncate_utf8_boundary(text, MAX_SEND_TEXT_BYTES),
    };
    let payload_json = serde_json::to_string(&payload)
        .map_err(|e| AppError::new(format!("messaging send payload 序列化失败: {e}")))?;
    let summary = format!(
        "Messaging send {} → {}",
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
        .send(&integration, &payload.conversation_id, &payload.text)
        .await
}

async fn process_event<R: Runtime>(
    app: &tauri::AppHandle<R>,
    actions: &dyn MessagingActions<R>,
    integration: &MessagingIntegration,
    event: &MessagingEvent,
) -> AppResult<ProcessingOutcome> {
    if integration.require_mention && !event.mentioned_bot {
        return Ok(ProcessingOutcome::default());
    }
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
    let command = match parse_command(&event.text) {
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

fn provider_for(kind: MessagingProviderKind) -> &'static dyn MessagingProvider {
    match kind {
        MessagingProviderKind::Feishu => &FeishuProvider,
        MessagingProviderKind::WeChatWork => &WeChatWorkProvider,
        MessagingProviderKind::DingTalk => &DingTalkProvider,
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
    "可用命令：\n/help\n/status\n/review <project|repo> <pr-number> [--check]"
}

#[derive(Debug, Clone, Copy)]
enum ReplyKind {
    Unauthorized,
    InvalidCommand,
    Help,
    Status,
    ReviewQueued,
    ReviewFailed,
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
                text: " hello ".to_string(),
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
}
