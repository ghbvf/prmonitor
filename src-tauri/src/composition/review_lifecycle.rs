//! Review lifecycle notification orchestration.
//!
//! This is a horizontal composition module: review emits a lifecycle event through `AppState`, and
//! this module fans it into the existing notification/messaging outbox funnels.

use tauri::{Manager, Runtime};

use crate::config;
use crate::error::{AppError, AppResult};
use crate::messaging;
use crate::model::{
    MessagingSendContent, NotificationLevel, ReviewLifecycleDispatch, ReviewLifecycleEvent,
    SendMessagingRequest, SendNotificationRequest,
};
use crate::notification;
use crate::outbox;

pub(super) fn enqueue<R: Runtime>(
    app: &tauri::AppHandle<R>,
    messaging_actions: &dyn messaging::service::MessagingActions<R>,
    event: ReviewLifecycleDispatch,
) -> AppResult<()> {
    if event.event != ReviewLifecycleEvent::Started {
        cancel_pending_started(app, &event)?;
    }

    let cfg = config::service::load(app)?;
    let lifecycle = cfg.review_lifecycle_notifications.clone();
    if !lifecycle.enabled || !lifecycle.events.contains(&event.event) {
        return Ok(());
    }
    let delay_secs = match event.event {
        ReviewLifecycleEvent::Started => lifecycle.start_delay_secs,
        ReviewLifecycleEvent::Completed
        | ReviewLifecycleEvent::Failed
        | ReviewLifecycleEvent::Interrupted => lifecycle.end_delay_secs,
    };

    let mut errors = Vec::new();
    for target in lifecycle.targets {
        match target {
            config::service::ReviewLifecycleTarget::NotificationChannels { channel_ids } => {
                let (title, body, url) = message(&cfg, &event);
                let request = SendNotificationRequest {
                    level: Some(NotificationLevel::Info),
                    title,
                    body: Some(body),
                    url,
                    project_id: Some(event.project_id.clone()),
                    channel_ids,
                };
                let dedupe_prefix = dedupe_prefix(&event, "notification");
                if let Err(e) = notification::enqueue_notification_with_dedupe_prefix_after(
                    app,
                    request,
                    Some(&dedupe_prefix),
                    delay_secs,
                ) {
                    errors.push(e.message);
                }
            }
            config::service::ReviewLifecycleTarget::MessagingConversation {
                integration_id,
                conversation_id,
            } => {
                let (title, body, url) = message(&cfg, &event);
                let text = match url {
                    Some(url) if !url.is_empty() => format!("{title}\n{body}\n{url}"),
                    Some(_) | None => format!("{title}\n{body}"),
                };
                let request_id = dedupe_prefix(
                    &event,
                    &format!("messaging:{integration_id}:{conversation_id}"),
                );
                let request = SendMessagingRequest {
                    integration_id,
                    conversation_id,
                    content: MessagingSendContent::Text { text },
                    request_id,
                };
                if let Err(e) = messaging::service::enqueue_send_once_after(
                    app,
                    messaging_actions,
                    request,
                    delay_secs,
                ) {
                    errors.push(e.message);
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(AppError::new(format!(
            "review lifecycle 通知入队失败：{}",
            errors.join("; ")
        )))
    }
}

fn cancel_pending_started<R: Runtime>(
    app: &tauri::AppHandle<R>,
    event: &ReviewLifecycleDispatch,
) -> AppResult<()> {
    let fragment = started_dedupe_fragment(event);
    let db = app.state::<crate::db::Database>();
    let now = outbox::store::now_epoch();
    let ids = outbox::store::mark_pending_by_dedupe_fragment_dead(
        db.inner(),
        &fragment,
        "review lifecycle Started canceled by terminal event",
        now,
    )?;
    for id in ids {
        outbox::service::announce_updated(app, db.inner(), id);
    }
    Ok(())
}

fn started_dedupe_fragment(event: &ReviewLifecycleDispatch) -> String {
    format!(
        "review-lifecycle:{}:{}:started:",
        event.project_id, event.thread_id
    )
}

fn message(
    cfg: &config::model::AppConfig,
    event: &ReviewLifecycleDispatch,
) -> (String, String, Option<String>) {
    let label = match event.event {
        ReviewLifecycleEvent::Started => "已开始",
        ReviewLifecycleEvent::Completed => "已完成",
        ReviewLifecycleEvent::Failed => "失败",
        ReviewLifecycleEvent::Interrupted => "已中断",
    };
    let title = format!(
        "PR #{} {} {label}",
        event.pr_number,
        crate::model::SkillInvocation::display_label(&event.skill_key)
    );
    let repo = cfg
        .projects
        .iter()
        .find(|project| project.id == event.project_id)
        .map(|project| project.repo.as_str())
        .unwrap_or("");
    let mut body = vec![
        format!("projectId: {}", event.project_id),
        format!("repo: {repo}"),
        format!("threadId: {}", event.thread_id),
    ];
    match event.event {
        ReviewLifecycleEvent::Started => {}
        ReviewLifecycleEvent::Completed => body.push(
            event
                .comment_url
                .clone()
                .unwrap_or_else(|| "本次 review 已完成（无评论链接）".to_string()),
        ),
        ReviewLifecycleEvent::Failed => body.push("本次 review 失败".to_string()),
        ReviewLifecycleEvent::Interrupted => body.push("本次 review 已中断".to_string()),
    }
    (title, body.join("\n"), event.comment_url.clone())
}

fn dedupe_prefix(event: &ReviewLifecycleDispatch, target: &str) -> String {
    format!(
        "review-lifecycle:{}:{}:{}:{}",
        event.project_id,
        event.thread_id,
        event.event.as_wire(),
        target
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::db::Database;
    use crate::model::{ActionKind, ActionStatus, ExternalRequestId, OutboxEntry, ReviewReceiptId};
    use crate::state::AppState;
    use tauri::Manager;

    fn dispatch(event: ReviewLifecycleEvent) -> ReviewLifecycleDispatch {
        ReviewLifecycleDispatch {
            project_id: "p1".to_string(),
            pr_number: 7,
            skill_key: crate::model::SkillInvocation::skill_key("pr-review", ""),
            thread_id: "thread-1".to_string(),
            event,
            comment_url: Some("https://example.com/pr/7#discussion".to_string()),
        }
    }

    #[test]
    fn dedupe_prefix_includes_thread_event_and_target() {
        assert_eq!(
            dedupe_prefix(&dispatch(ReviewLifecycleEvent::Completed), "notification"),
            "review-lifecycle:p1:thread-1:completed:notification"
        );
    }

    #[test]
    fn message_includes_project_repo_thread_and_terminal_detail() {
        let cfg = config::model::AppConfig {
            projects: vec![config::model::Project {
                id: "p1".to_string(),
                repo: "owner/repo".to_string(),
                ..config::model::Project::default()
            }],
            ..config::model::AppConfig::default()
        };
        let (title, body, url) = message(&cfg, &dispatch(ReviewLifecycleEvent::Completed));

        assert_eq!(title, "PR #7 pr-review 已完成");
        assert!(body.contains("projectId: p1"), "{body}");
        assert!(body.contains("repo: owner/repo"), "{body}");
        assert!(body.contains("threadId: thread-1"), "{body}");
        assert!(
            body.contains("https://example.com/pr/7#discussion"),
            "{body}"
        );
        assert_eq!(url.as_deref(), Some("https://example.com/pr/7#discussion"));
    }

    struct CapturingActions {
        call: Mutex<Option<(String, u64)>>,
    }

    impl<R: Runtime> messaging::service::MessagingActions<R> for CapturingActions {
        fn enqueue_reply(
            &self,
            _app: &tauri::AppHandle<R>,
            _integration_id: &str,
            _kind: ActionKind,
            _summary: &str,
            _payload_json: &str,
            _dedupe_key: &str,
        ) -> AppResult<i64> {
            Ok(1)
        }

        fn enqueue_send(
            &self,
            _app: &tauri::AppHandle<R>,
            _kind: ActionKind,
            _summary: &str,
            _payload_json: &str,
            _dedupe_key: &str,
        ) -> AppResult<i64> {
            Ok(2)
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
            _app: &tauri::AppHandle<R>,
            _kind: ActionKind,
            _summary: &str,
            _payload_json: &str,
            dedupe_key: &str,
            delay_secs: u64,
        ) -> AppResult<i64> {
            *self.call.lock().expect("lock") = Some((dedupe_key.to_string(), delay_secs));
            Ok(3)
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
            _extra_args: String,
            _request_id: ExternalRequestId,
        ) -> AppResult<ReviewReceiptId> {
            ReviewReceiptId::new(1).map_err(AppError::new)
        }
    }

    #[test]
    fn enqueue_fans_out_to_notification_and_messaging_targets() {
        let app = tauri::test::mock_app();
        app.manage(AppState::default());
        let db = Database::open_in_memory().expect("open db");
        crate::config::service::persist_db(
            &db,
            &config::model::AppConfig {
                projects: vec![config::model::Project {
                    id: "p1".to_string(),
                    repo: "owner/repo".to_string(),
                    ..config::model::Project::default()
                }],
                active_project_id: "p1".to_string(),
                notifications: config::model::NotificationSettings {
                    channels: vec![config::model::NotificationChannel {
                        id: "desktop".to_string(),
                        name: "Desktop".to_string(),
                        kind: crate::model::NotificationKind::Desktop,
                        enabled: true,
                        ..config::model::NotificationChannel::default()
                    }],
                },
                messaging: config::model::MessagingSettings {
                    integrations: vec![config::model::MessagingIntegration {
                        id: "fs".to_string(),
                        name: "Feishu".to_string(),
                        enabled: true,
                        allowed_conversation_ids: vec!["chat".to_string()],
                        ..config::model::MessagingIntegration::feishu_default()
                    }],
                },
                review_lifecycle_notifications: config::model::ReviewLifecycleNotificationConfig {
                    enabled: true,
                    events: vec![ReviewLifecycleEvent::Completed],
                    targets: vec![
                        config::model::ReviewLifecycleTarget::NotificationChannels {
                            channel_ids: vec!["desktop".to_string()],
                        },
                        config::model::ReviewLifecycleTarget::MessagingConversation {
                            integration_id: "fs".to_string(),
                            conversation_id: "chat".to_string(),
                        },
                    ],
                    start_delay_secs: 0,
                    end_delay_secs: 45,
                },
                ..config::model::AppConfig::default()
            },
        )
        .expect("persist config");
        app.manage(db);
        let actions = CapturingActions {
            call: Mutex::new(None),
        };

        enqueue(
            app.handle(),
            &actions,
            dispatch(ReviewLifecycleEvent::Completed),
        )
        .expect("enqueue");

        let entries =
            crate::outbox::store::list_by_project(app.state::<Database>().inner(), Some("p1"))
                .expect("list outbox");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, ActionKind::Notification);
        assert!(
            entries[0].next_attempt_at >= entries[0].created_at.saturating_add(45),
            "notification target should inherit terminal lifecycle delay"
        );
        let call = actions.call.lock().expect("lock").clone().expect("call");
        assert_eq!(
            call.0,
            "messaging-send:fs:review-lifecycle:p1:thread-1:completed:messaging:fs:chat"
        );
        assert_eq!(call.1, 45);
    }

    #[test]
    fn terminal_event_cancels_pending_delayed_started() {
        let app = tauri::test::mock_app();
        app.manage(AppState::default());
        let db = Database::open_in_memory().expect("open db");
        crate::config::service::persist_db(
            &db,
            &config::model::AppConfig {
                projects: vec![config::model::Project {
                    id: "p1".to_string(),
                    repo: "owner/repo".to_string(),
                    ..config::model::Project::default()
                }],
                active_project_id: "p1".to_string(),
                notifications: config::model::NotificationSettings {
                    channels: vec![config::model::NotificationChannel {
                        id: "desktop".to_string(),
                        name: "Desktop".to_string(),
                        kind: crate::model::NotificationKind::Desktop,
                        enabled: true,
                        ..config::model::NotificationChannel::default()
                    }],
                },
                review_lifecycle_notifications: config::model::ReviewLifecycleNotificationConfig {
                    enabled: true,
                    events: vec![
                        ReviewLifecycleEvent::Started,
                        ReviewLifecycleEvent::Completed,
                    ],
                    targets: vec![config::model::ReviewLifecycleTarget::NotificationChannels {
                        channel_ids: vec!["desktop".to_string()],
                    }],
                    start_delay_secs: 60,
                    end_delay_secs: 0,
                },
                ..config::model::AppConfig::default()
            },
        )
        .expect("persist config");
        app.manage(db);
        let actions = CapturingActions {
            call: Mutex::new(None),
        };

        enqueue(
            app.handle(),
            &actions,
            dispatch(ReviewLifecycleEvent::Started),
        )
        .expect("enqueue started");
        enqueue(
            app.handle(),
            &actions,
            dispatch(ReviewLifecycleEvent::Completed),
        )
        .expect("enqueue completed");

        let entries =
            crate::outbox::store::list_by_project(app.state::<Database>().inner(), Some("p1"))
                .expect("list outbox");
        let started = entries
            .iter()
            .find(|entry| entry.summary.contains("已开始"))
            .expect("started row");
        assert_eq!(
            started.status,
            ActionStatus::Dead,
            "terminal lifecycle event must cancel the delayed Started row before it can be claimed"
        );
    }
}
