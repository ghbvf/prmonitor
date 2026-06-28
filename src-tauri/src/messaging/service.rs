//! Messaging ingress + command processing (#1559).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::http::HeaderMap;
use tauri::{Manager, Runtime};

use crate::config::service::{self as config_service, MessagingIntegration};
use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::messaging::feishu::FeishuProvider;
use crate::messaging::provider::{MessagingProvider, Verification};
use crate::messaging::store;
use crate::model::{
    ActionExecutionResult, ActionKind, MessagingEvent, MessagingEventStatus, MessagingProviderKind,
    MessagingReplyPayload, MessagingReplyTarget,
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

    fn trigger_review<'a>(
        &'a self,
        app: &'a tauri::AppHandle<R>,
        reference: String,
        pr_number: u64,
        kind: String,
    ) -> Pin<Box<dyn Future<Output = AppResult<String>> + Send + 'a>>;
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
    fn as_wire(self) -> &'static str {
        match self {
            ReviewCommandKind::Review => "review",
            ReviewCommandKind::Check => "check",
        }
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
            let outcome = match actions
                .trigger_review(
                    app,
                    reference.clone(),
                    pr_number,
                    kind.as_wire().to_string(),
                )
                .await
            {
                Ok(session_id) => {
                    let reply = enqueue_reply(
                        actions,
                        app,
                        integration,
                        event,
                        ReplyKind::ReviewStarted,
                        &format!(
                            "已启动 {mode}：{reference} #{pr_number}\nSession: {session_id}",
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
                        &format!("启动 review 失败：{}", e.message),
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

fn provider_for(kind: MessagingProviderKind) -> &'static dyn MessagingProvider {
    match kind {
        MessagingProviderKind::Feishu => &FeishuProvider,
    }
}

fn conversation_allowed(integration: &MessagingIntegration, conversation_id: &str) -> bool {
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
    ReviewStarted,
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
            ReplyKind::ReviewStarted => "review-started",
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

        fn trigger_review<'a>(
            &'a self,
            _app: &'a tauri::AppHandle<R>,
            _reference: String,
            _pr_number: u64,
            _kind: String,
        ) -> Pin<Box<dyn Future<Output = AppResult<String>> + Send + 'a>> {
            Box::pin(async { Err(AppError::new("review boom")) })
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
}
