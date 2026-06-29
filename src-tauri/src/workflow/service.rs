//! Workflow orchestration logic (#1370).

use sha2::{Digest, Sha256};
use tauri::{Manager, Runtime};

use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::events::{StreamEvent, WorkflowEvent};
use crate::model::{
    NotificationLevel, SendNotificationRequest, WorkflowInstance, WorkflowStatus, WorkflowStep,
    WorkflowType,
};
use crate::stream;
use crate::workflow::manager::{ReviewNotifyRequest, ReviewNotifyState, WorkflowActions};
use crate::workflow::store::{self, NewWorkflow};

const STEP_LEASE_SECS: u64 = 5 * 60;
const WAIT_REVIEW_LEASE_SECS: u64 = 24 * 60 * 60;

pub async fn start_review_notify<R: Runtime>(
    app: &tauri::AppHandle<R>,
    actions: &WorkflowActions,
    request: ReviewNotifyRequest,
) -> AppResult<()> {
    let input = serde_json::to_value(&request)
        .map_err(|e| AppError::new(format!("workflow input 序列化失败：{e}")))?;
    let dedupe_key = review_notify_dedupe_key(&request);
    let db = app.state::<Database>();
    let instance = store::create_or_get(
        db.inner(),
        NewWorkflow {
            project_id: &request.reference,
            workflow_type: WorkflowType::ReviewNotify,
            input: &input,
            dedupe_key: &dedupe_key,
            now: store::now_epoch(),
        },
    )?;
    announce_updated(app, db.inner(), instance.id);
    drive_instance(app, actions, instance).await
}

pub async fn drive_instance<R: Runtime>(
    app: &tauri::AppHandle<R>,
    actions: &WorkflowActions,
    mut instance: WorkflowInstance,
) -> AppResult<()> {
    loop {
        if instance.status == WorkflowStatus::Running {
            return Ok(());
        }
        match instance.current_step {
            WorkflowStep::StartReview => {
                instance = start_review_step(app, actions, instance).await?;
            }
            WorkflowStep::WaitReview => {
                instance = wait_review_step(app, actions, instance).await?;
            }
            WorkflowStep::EnqueueNotify => {
                instance = enqueue_notify_step(app, actions, instance).await?;
            }
            WorkflowStep::Done => return Ok(()),
        }
        if matches!(
            instance.status,
            WorkflowStatus::Waiting | WorkflowStatus::Done | WorkflowStatus::Failed
        ) && instance.current_step == WorkflowStep::WaitReview
        {
            // Waiting can be long-lived. The live deeplink path is handled by the awaited wait step
            // above; this return keeps a recovered non-terminal row from hot-looping.
            return Ok(());
        }
        if matches!(
            instance.status,
            WorkflowStatus::Done | WorkflowStatus::Failed
        ) {
            return Ok(());
        }
    }
}

pub fn announce_updated<R: Runtime>(app: &tauri::AppHandle<R>, db: &Database, id: i64) {
    match store::get(db, id) {
        Ok(Some(instance)) => stream::emit(
            app,
            StreamEvent::Workflow(WorkflowEvent::Updated {
                project_id: instance.project_id.clone(),
                instance,
            }),
        ),
        Ok(None) => {}
        Err(e) => stream::emit(
            app,
            StreamEvent::Workflow(WorkflowEvent::Error {
                operation: "announce".to_string(),
                message: e.message,
            }),
        ),
    }
}

async fn start_review_step<R: Runtime>(
    app: &tauri::AppHandle<R>,
    actions: &WorkflowActions,
    instance: WorkflowInstance,
) -> AppResult<WorkflowInstance> {
    let request: ReviewNotifyRequest = serde_json::from_value(instance.input.clone())
        .map_err(|e| AppError::new(format!("workflow reviewNotify input 无效：{e}")))?;
    let db = app.state::<Database>();
    if !store::claim_step(
        db.inner(),
        instance.id,
        WorkflowStep::StartReview,
        store::now_epoch().saturating_add(STEP_LEASE_SECS),
    )? {
        return store::get(db.inner(), instance.id)?
            .ok_or_else(|| AppError::new(format!("workflow 不存在：{}", instance.id)));
    }
    announce_updated(app, db.inner(), instance.id);
    match (actions.start_review)(request, instance.created_at).await {
        Ok(started) => {
            let mut trace = review_notify_state(&instance)?;
            trace.review_thread_id = Some(started.thread_id);
            let state = review_notify_state_value(&trace)?;
            if !store::update_progress(
                db.inner(),
                store::ProgressUpdate {
                    id: instance.id,
                    expected_step: WorkflowStep::StartReview,
                    project_id: Some(&started.project_id),
                    status: WorkflowStatus::Waiting,
                    step: WorkflowStep::WaitReview,
                    state: &state,
                    next_wake_at: store::now_epoch().saturating_add(15),
                },
            )? {
                return current_instance(db.inner(), instance.id);
            }
            announce_updated(app, db.inner(), instance.id);
            current_instance(db.inner(), instance.id)
        }
        Err(e) => {
            let _ = store::mark_failed_expected(
                db.inner(),
                instance.id,
                WorkflowStep::StartReview,
                &e.message,
            )?;
            announce_updated(app, db.inner(), instance.id);
            Err(e)
        }
    }
}

async fn wait_review_step<R: Runtime>(
    app: &tauri::AppHandle<R>,
    actions: &WorkflowActions,
    instance: WorkflowInstance,
) -> AppResult<WorkflowInstance> {
    let db = app.state::<Database>();
    if !store::claim_step(
        db.inner(),
        instance.id,
        WorkflowStep::WaitReview,
        store::now_epoch().saturating_add(WAIT_REVIEW_LEASE_SECS),
    )? {
        return current_instance(db.inner(), instance.id);
    }
    announce_updated(app, db.inner(), instance.id);
    let trace = review_notify_state(&instance)?;
    let Some(thread_id) = trace.review_thread_id.clone() else {
        let e = AppError::new("workflow 缺少 reviewThreadId".to_string());
        let _ = store::mark_failed_expected(
            db.inner(),
            instance.id,
            WorkflowStep::WaitReview,
            &e.message,
        )?;
        announce_updated(app, db.inner(), instance.id);
        return Err(e);
    };
    match (actions.wait_review)(thread_id).await {
        Ok(outcome) => {
            let mut trace = trace;
            trace.review_wire_status = Some(outcome.wire_status);
            trace.comment_url = outcome.comment_url;
            let state = review_notify_state_value(&trace)?;
            if !store::update_progress(
                db.inner(),
                store::ProgressUpdate {
                    id: instance.id,
                    expected_step: WorkflowStep::WaitReview,
                    project_id: None,
                    status: WorkflowStatus::Pending,
                    step: WorkflowStep::EnqueueNotify,
                    state: &state,
                    next_wake_at: store::now_epoch(),
                },
            )? {
                return current_instance(db.inner(), instance.id);
            }
            announce_updated(app, db.inner(), instance.id);
            current_instance(db.inner(), instance.id)
        }
        Err(e) => {
            let _ = store::mark_failed_expected(
                db.inner(),
                instance.id,
                WorkflowStep::WaitReview,
                &e.message,
            )?;
            announce_updated(app, db.inner(), instance.id);
            Err(e)
        }
    }
}

async fn enqueue_notify_step<R: Runtime>(
    app: &tauri::AppHandle<R>,
    actions: &WorkflowActions,
    instance: WorkflowInstance,
) -> AppResult<WorkflowInstance> {
    let request: ReviewNotifyRequest = serde_json::from_value(instance.input.clone())
        .map_err(|e| AppError::new(format!("workflow reviewNotify input 无效：{e}")))?;
    let mut trace = review_notify_state(&instance)?;
    let wire_status = trace.review_wire_status.as_deref().unwrap_or("completed");
    let comment_url = trace.comment_url.clone();
    let send =
        review_completion_notification_request(&request, &instance, wire_status, comment_url);
    let dedupe_prefix = format!("workflow:{}:notify", instance.id);
    let db = app.state::<Database>();
    if !trace.notification_outbox_ids.is_empty() {
        let state = review_notify_state_value(&trace)?;
        if !store::update_progress(
            db.inner(),
            store::ProgressUpdate {
                id: instance.id,
                expected_step: WorkflowStep::EnqueueNotify,
                project_id: None,
                status: WorkflowStatus::Done,
                step: WorkflowStep::Done,
                state: &state,
                next_wake_at: 0,
            },
        )? {
            return current_instance(db.inner(), instance.id);
        }
        announce_updated(app, db.inner(), instance.id);
        return current_instance(db.inner(), instance.id);
    }
    if !store::claim_step(
        db.inner(),
        instance.id,
        WorkflowStep::EnqueueNotify,
        store::now_epoch().saturating_add(STEP_LEASE_SECS),
    )? {
        return store::get(db.inner(), instance.id)?
            .ok_or_else(|| AppError::new(format!("workflow 不存在：{}", instance.id)));
    }
    announce_updated(app, db.inner(), instance.id);
    match (actions.send_notification)(send, dedupe_prefix).await {
        Ok(response) => {
            trace.notification_outbox_ids = response.outbox_ids;
            let state = review_notify_state_value(&trace)?;
            if !store::update_progress(
                db.inner(),
                store::ProgressUpdate {
                    id: instance.id,
                    expected_step: WorkflowStep::EnqueueNotify,
                    project_id: None,
                    status: WorkflowStatus::Done,
                    step: WorkflowStep::Done,
                    state: &state,
                    next_wake_at: 0,
                },
            )? {
                return current_instance(db.inner(), instance.id);
            }
            announce_updated(app, db.inner(), instance.id);
            current_instance(db.inner(), instance.id)
        }
        Err(e) => {
            let _ = store::mark_failed_expected(
                db.inner(),
                instance.id,
                WorkflowStep::EnqueueNotify,
                &e.message,
            )?;
            announce_updated(app, db.inner(), instance.id);
            Err(e)
        }
    }
}

fn review_completion_notification_request(
    request: &ReviewNotifyRequest,
    instance: &WorkflowInstance,
    wire_status: &str,
    comment_url: Option<String>,
) -> SendNotificationRequest {
    let (status_label, fallback) = match wire_status {
        "completed" => ("完成", "本次 review 完成（无评论链接）"),
        "interrupted" => ("已中断", "本次 review 已中断（无评论链接）"),
        "failed" => ("失败", "本次 review 失败（无评论链接）"),
        _ => ("结束", "本次 review 结束（无评论链接）"),
    };
    SendNotificationRequest {
        level: Some(NotificationLevel::Info),
        title: format!("PR #{} review {status_label}", request.pr_number),
        body: Some(comment_url.clone().unwrap_or_else(|| fallback.to_string())),
        url: comment_url,
        project_id: Some(instance.project_id.clone()).filter(|s| !s.is_empty()),
        channel_ids: Vec::new(),
    }
}

fn current_instance(db: &Database, id: i64) -> AppResult<WorkflowInstance> {
    store::get(db, id)?.ok_or_else(|| AppError::new(format!("workflow 不存在：{id}")))
}

fn review_notify_state(instance: &WorkflowInstance) -> AppResult<ReviewNotifyState> {
    serde_json::from_value(instance.state.clone())
        .map_err(|e| AppError::new(format!("workflow reviewNotify state 无效：{e}")))
}

fn review_notify_state_value(state: &ReviewNotifyState) -> AppResult<serde_json::Value> {
    serde_json::to_value(state)
        .map_err(|e| AppError::new(format!("workflow reviewNotify state 序列化失败：{e}")))
}

fn review_notify_dedupe_key(request: &ReviewNotifyRequest) -> String {
    let bytes = serde_json::to_vec(request).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    format!("deeplink-review:{}", hex::encode(digest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use crate::model::SendNotificationResponse;

    fn test_app() -> tauri::App<tauri::test::MockRuntime> {
        let app = tauri::test::mock_app();
        app.manage(Database::open_in_memory().expect("open db"));
        app.manage(crate::state::AppState::default());
        app
    }

    fn request() -> ReviewNotifyRequest {
        ReviewNotifyRequest {
            reference: "repo".to_string(),
            pr_number: 7,
            kind: "review".to_string(),
        }
    }

    fn happy_actions(
        start_calls: Arc<AtomicUsize>,
        wait_calls: Arc<AtomicUsize>,
        send_calls: Arc<AtomicUsize>,
    ) -> WorkflowActions {
        WorkflowActions {
            start_review: Arc::new(move |_request, _created_at| {
                let start_calls = Arc::clone(&start_calls);
                Box::pin(async move {
                    start_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(crate::workflow::manager::StartedReview {
                        thread_id: "t1".to_string(),
                        project_id: "p1".to_string(),
                    })
                })
            }),
            wait_review: Arc::new(move |thread_id| {
                let wait_calls = Arc::clone(&wait_calls);
                Box::pin(async move {
                    wait_calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(thread_id, "t1");
                    Ok(crate::workflow::manager::ReviewCompletion {
                        wire_status: "completed".to_string(),
                        comment_url: Some("https://example.com/pr/7#comment".to_string()),
                    })
                })
            }),
            send_notification: Arc::new(move |_request, dedupe_prefix| {
                let send_calls = Arc::clone(&send_calls);
                Box::pin(async move {
                    send_calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(dedupe_prefix, "workflow:1:notify");
                    Ok(SendNotificationResponse {
                        outbox_ids: vec![9],
                    })
                })
            }),
        }
    }

    #[tokio::test]
    async fn review_notify_state_machine_records_success_trace() {
        let app = test_app();
        let handle = app.handle().clone();
        let start_calls = Arc::new(AtomicUsize::new(0));
        let wait_calls = Arc::new(AtomicUsize::new(0));
        let send_calls = Arc::new(AtomicUsize::new(0));
        let actions = happy_actions(
            Arc::clone(&start_calls),
            Arc::clone(&wait_calls),
            Arc::clone(&send_calls),
        );

        start_review_notify(&handle, &actions, request())
            .await
            .expect("start");
        let db = handle.state::<Database>();
        let waiting = crate::workflow::store::list_by_project(db.inner(), Some("p1"))
            .expect("list")
            .remove(0);
        assert_eq!(waiting.status, WorkflowStatus::Waiting);
        assert_eq!(waiting.current_step, WorkflowStep::WaitReview);
        assert_eq!(waiting.state["reviewThreadId"], "t1");
        assert_eq!(start_calls.load(Ordering::SeqCst), 1);
        assert_eq!(wait_calls.load(Ordering::SeqCst), 0);
        assert_eq!(send_calls.load(Ordering::SeqCst), 0);

        drive_instance(&handle, &actions, waiting)
            .await
            .expect("finish");
        let done = crate::workflow::store::list_by_project(db.inner(), Some("p1"))
            .expect("list")
            .remove(0);
        assert_eq!(done.status, WorkflowStatus::Done);
        assert_eq!(done.current_step, WorkflowStep::Done);
        assert_eq!(done.state["reviewWireStatus"], "completed");
        assert_eq!(done.state["commentUrl"], "https://example.com/pr/7#comment");
        assert_eq!(done.state["notificationOutboxIds"], serde_json::json!([9]));
        assert_eq!(start_calls.load(Ordering::SeqCst), 1);
        assert_eq!(wait_calls.load(Ordering::SeqCst), 1);
        assert_eq!(send_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn start_review_failure_is_visible_on_instance() {
        let app = test_app();
        let handle = app.handle().clone();
        let actions = WorkflowActions {
            start_review: Arc::new(move |_request, _created_at| {
                Box::pin(async move { Err(AppError::new("start boom".to_string())) })
            }),
            wait_review: Arc::new(move |_thread_id| {
                Box::pin(async move { Err(AppError::new("should not wait".to_string())) })
            }),
            send_notification: Arc::new(move |_request, _dedupe_prefix| {
                Box::pin(async move { Err(AppError::new("should not send".to_string())) })
            }),
        };

        let err = start_review_notify(&handle, &actions, request())
            .await
            .expect_err("start fails");
        assert_eq!(err.message, "start boom");
        let db = handle.state::<Database>();
        let failed = crate::workflow::store::list_by_project(db.inner(), Some("repo"))
            .expect("list")
            .remove(0);
        assert_eq!(failed.status, WorkflowStatus::Failed);
        assert_eq!(failed.current_step, WorkflowStep::StartReview);
        assert_eq!(failed.attempt_count, 1);
        assert_eq!(failed.last_error.as_deref(), Some("start boom"));
    }

    #[tokio::test]
    async fn enqueue_notify_skips_side_effect_when_outbox_ids_exist() {
        let app = test_app();
        let handle = app.handle().clone();
        let db = handle.state::<Database>();
        let input = serde_json::to_value(request()).expect("input");
        let instance = crate::workflow::store::create_or_get(
            db.inner(),
            NewWorkflow {
                project_id: "p1",
                workflow_type: WorkflowType::ReviewNotify,
                input: &input,
                dedupe_key: "k1",
                now: 10,
            },
        )
        .expect("create");
        let state = serde_json::json!({
            "reviewThreadId": "t1",
            "reviewWireStatus": "completed",
            "commentUrl": "https://example.com/pr/7#comment",
            "notificationOutboxIds": [9]
        });
        crate::workflow::store::update_progress(
            db.inner(),
            crate::workflow::store::ProgressUpdate {
                id: instance.id,
                expected_step: WorkflowStep::StartReview,
                project_id: None,
                status: WorkflowStatus::Pending,
                step: WorkflowStep::EnqueueNotify,
                state: &state,
                next_wake_at: 10,
            },
        )
        .expect("advance");
        let current = current_instance(db.inner(), instance.id).expect("current");
        let send_calls = Arc::new(AtomicUsize::new(0));
        let actions = happy_actions(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::clone(&send_calls),
        );

        drive_instance(&handle, &actions, current)
            .await
            .expect("drive");
        let done = current_instance(db.inner(), instance.id).expect("done");
        assert_eq!(done.status, WorkflowStatus::Done);
        assert_eq!(done.current_step, WorkflowStep::Done);
        assert_eq!(done.state["notificationOutboxIds"], serde_json::json!([9]));
        assert_eq!(send_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn claim_owned_by_another_runner_exits_without_side_effect() {
        let app = test_app();
        let handle = app.handle().clone();
        let db = handle.state::<Database>();
        let input = serde_json::to_value(request()).expect("input");
        let stale = crate::workflow::store::create_or_get(
            db.inner(),
            NewWorkflow {
                project_id: "repo",
                workflow_type: WorkflowType::ReviewNotify,
                input: &input,
                dedupe_key: "k1",
                now: 10,
            },
        )
        .expect("create");
        assert!(crate::workflow::store::claim_step(
            db.inner(),
            stale.id,
            WorkflowStep::StartReview,
            99,
        )
        .expect("claim elsewhere"));
        let start_calls = Arc::new(AtomicUsize::new(0));
        let actions = happy_actions(
            Arc::clone(&start_calls),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        );

        drive_instance(&handle, &actions, stale)
            .await
            .expect("stale driver exits");
        assert_eq!(start_calls.load(Ordering::SeqCst), 0);
        let current = current_instance(db.inner(), 1).expect("current");
        assert_eq!(current.status, WorkflowStatus::Running);
        assert_eq!(current.current_step, WorkflowStep::StartReview);
    }

    #[test]
    fn review_notify_dedupe_key_is_stable() {
        let req = ReviewNotifyRequest {
            reference: "repo".to_string(),
            pr_number: 7,
            kind: "review".to_string(),
        };
        assert_eq!(
            review_notify_dedupe_key(&req),
            review_notify_dedupe_key(&req)
        );
    }

    #[test]
    fn review_completion_notification_request_maps_terminal_payload() {
        let req = ReviewNotifyRequest {
            reference: "repo".to_string(),
            pr_number: 7,
            kind: "review".to_string(),
        };
        let instance = WorkflowInstance {
            id: 1,
            project_id: "p1".to_string(),
            workflow_type: WorkflowType::ReviewNotify,
            status: WorkflowStatus::Running,
            current_step: WorkflowStep::EnqueueNotify,
            input: serde_json::Value::Null,
            state: serde_json::Value::Null,
            attempt_count: 0,
            next_wake_at: 0,
            last_error: None,
            created_at: 0,
            updated_at: 0,
        };
        let request = review_completion_notification_request(
            &req,
            &instance,
            "completed",
            Some("https://example.com/pr/7#comment".to_string()),
        );
        assert_eq!(request.title, "PR #7 review 完成");
        assert_eq!(
            request.body.as_deref(),
            Some("https://example.com/pr/7#comment")
        );
        assert_eq!(
            request.url.as_deref(),
            Some("https://example.com/pr/7#comment")
        );
        assert_eq!(request.project_id.as_deref(), Some("p1"));

        let failed = review_completion_notification_request(&req, &instance, "failed", None);
        assert_eq!(failed.title, "PR #7 review 失败");
        assert_eq!(
            failed.body.as_deref(),
            Some("本次 review 失败（无评论链接）")
        );
    }

    #[test]
    fn review_notify_state_wire_keys_are_pinned() {
        let input = serde_json::to_value(request()).expect("input");
        assert_eq!(input["reference"], "repo");
        assert_eq!(input["prNumber"], 7);
        assert_eq!(input["kind"], "review");

        let state = ReviewNotifyState {
            review_thread_id: Some("t1".to_string()),
            review_wire_status: Some("completed".to_string()),
            comment_url: Some("https://example.com/pr/7#comment".to_string()),
            notification_outbox_ids: vec![9],
        };
        let value = review_notify_state_value(&state).expect("state");
        assert_eq!(value["reviewThreadId"], "t1");
        assert_eq!(value["reviewWireStatus"], "completed");
        assert_eq!(value["commentUrl"], "https://example.com/pr/7#comment");
        assert_eq!(value["notificationOutboxIds"], serde_json::json!([9]));
        assert!(value.get("review_thread_id").is_none());
        assert!(value.get("notification_outbox_ids").is_none());
    }
}
