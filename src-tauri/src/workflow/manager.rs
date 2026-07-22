//! Workflow worker lifecycle and composition-root action seams (#1370).

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::async_runtime::{spawn, JoinHandle};
use tauri::Manager;
use tokio::sync::Notify;
use tokio::time::{interval, MissedTickBehavior};

use crate::db::Database;
use crate::error::AppResult;
use crate::events::{StreamEvent, WorkflowEvent};
use crate::model::{ReviewReceiptId, ReviewReceiptStatus, SendNotificationResponse};
use crate::stream;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewNotifyRequest {
    pub receipt_id: ReviewReceiptId,
    pub reference: String,
    pub pr_number: u64,
    pub skill_key: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewNotifyState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_wire_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notification_outbox_ids: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewCompletion {
    pub thread_id: Option<String>,
    pub project_id: String,
    pub status: ReviewReceiptStatus,
    pub comment_url: Option<String>,
}

pub type WaitReceiptFn = Arc<
    dyn Fn(ReviewReceiptId) -> Pin<Box<dyn Future<Output = AppResult<ReviewCompletion>> + Send>>
        + Send
        + Sync,
>;

pub type SendNotificationFn = Arc<
    dyn Fn(
            crate::model::SendNotificationRequest,
            String,
        ) -> Pin<Box<dyn Future<Output = AppResult<SendNotificationResponse>> + Send>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct WorkflowActions {
    pub wait_receipt: WaitReceiptFn,
    pub send_notification: SendNotificationFn,
}

const WORKER_TICK_SECS: u64 = 15;
static NEXT_TRACKED_TASK_ID: AtomicU64 = AtomicU64::new(1);

struct TrackedHandle {
    id: u64,
    handle: JoinHandle<()>,
}

#[derive(Default)]
struct TrackedTasks {
    handles: Vec<TrackedHandle>,
    completed: HashSet<u64>,
}

#[derive(Default)]
pub struct WorkflowManager {
    wake: Arc<Notify>,
    stop: Arc<Notify>,
    task: StdMutex<Option<JoinHandle<()>>>,
    in_flight: Arc<StdMutex<TrackedTasks>>,
    actions: StdMutex<Option<WorkflowActions>>,
}

impl WorkflowManager {
    pub fn set_actions(&self, actions: WorkflowActions) {
        *self.actions.lock().unwrap_or_else(|p| p.into_inner()) = Some(actions);
    }

    pub fn start(&self, app: tauri::AppHandle) {
        let mut task = self.task.lock().unwrap_or_else(|p| p.into_inner());
        if task.is_some() {
            return;
        }
        let db = app.state::<Database>();
        if let Err(e) = crate::workflow::store::reset_running_for_recovery(
            db.inner(),
            crate::workflow::store::now_epoch(),
        ) {
            emit_error(&app, "reset_running_for_recovery", e.message);
        }
        let wake = Arc::clone(&self.wake);
        let stop = Arc::clone(&self.stop);
        let in_flight = Arc::clone(&self.in_flight);
        *task = Some(spawn(worker_loop(app, wake, stop, in_flight)));
    }

    pub fn wake(&self) {
        self.wake.notify_one();
    }

    pub fn shutdown(&self) {
        self.stop.notify_one();
        if let Some(handle) = self.task.lock().unwrap_or_else(|p| p.into_inner()).take() {
            handle.abort();
        }
        for handle in self
            .in_flight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .handles
            .drain(..)
        {
            handle.handle.abort();
        }
    }

    pub fn start_receipt_notify(
        &self,
        app: tauri::AppHandle,
        receipt_id: ReviewReceiptId,
        reference: String,
        pr_number: u64,
        skill_key: String,
    ) -> AppResult<()> {
        let request = ReviewNotifyRequest {
            receipt_id,
            reference,
            pr_number,
            skill_key,
        };
        crate::workflow::service::ensure_receipt_notify(&app, request)?;
        self.wake.notify_one();
        Ok(())
    }
}

fn spawn_tracked<F>(in_flight: &Arc<StdMutex<TrackedTasks>>, future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let id = NEXT_TRACKED_TASK_ID.fetch_add(1, Ordering::Relaxed);
    let cleanup = Arc::clone(in_flight);
    let handle = spawn(async move {
        future.await;
        complete_tracked_handle(&cleanup, id);
    });
    register_tracked_handle(in_flight, TrackedHandle { id, handle });
}

fn register_tracked_handle(in_flight: &Arc<StdMutex<TrackedTasks>>, handle: TrackedHandle) {
    let mut tasks = in_flight.lock().unwrap_or_else(|p| p.into_inner());
    if tasks.completed.remove(&handle.id) {
        return;
    }
    tasks.handles.push(handle);
}

fn complete_tracked_handle(in_flight: &Arc<StdMutex<TrackedTasks>>, id: u64) {
    let mut tasks = in_flight.lock().unwrap_or_else(|p| p.into_inner());
    let before = tasks.handles.len();
    tasks.handles.retain(|h| h.id != id);
    if tasks.handles.len() == before {
        tasks.completed.insert(id);
    }
}

fn emit_error<R: tauri::Runtime>(app: &tauri::AppHandle<R>, operation: &str, message: String) {
    stream::emit(
        app,
        StreamEvent::Workflow(WorkflowEvent::Error {
            operation: operation.to_string(),
            message,
        }),
    );
}

async fn worker_loop(
    app: tauri::AppHandle,
    wake: Arc<Notify>,
    stop: Arc<Notify>,
    in_flight: Arc<StdMutex<TrackedTasks>>,
) {
    let mut ticker = interval(Duration::from_secs(WORKER_TICK_SECS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = wake.notified() => {}
            _ = stop.notified() => return,
        }
        let actions = {
            let state = app.state::<crate::state::AppState>();
            let actions = state
                .workflow
                .actions
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            actions
        };
        let Some(actions) = actions else {
            continue;
        };
        let db = app.state::<Database>();
        if let Err(e) = crate::workflow::store::reset_expired_running(
            db.inner(),
            crate::workflow::store::now_epoch(),
        ) {
            emit_error(&app, "reset_expired_running", e.message);
            continue;
        }
        let due = match crate::workflow::store::due_instances(
            db.inner(),
            crate::workflow::store::now_epoch(),
        ) {
            Ok(due) => due,
            Err(e) => {
                emit_error(&app, "due_instances", e.message);
                continue;
            }
        };
        for instance in due {
            let app = app.clone();
            let actions = actions.clone();
            spawn_tracked(&in_flight, async move {
                if let Err(e) =
                    crate::workflow::service::drive_instance(&app, &actions, instance).await
                {
                    emit_error(&app, "drive_instance", e.message);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tracked_tasks_remove_themselves_on_completion() {
        let in_flight = Arc::new(StdMutex::new(TrackedTasks::default()));
        spawn_tracked(&in_flight, async {});
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            in_flight
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .handles
                .is_empty(),
            "completed tasks remove their own handle"
        );

        spawn_tracked(&in_flight, async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        assert_eq!(
            in_flight
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .handles
                .len(),
            1
        );
        for handle in in_flight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .handles
            .drain(..)
        {
            handle.handle.abort();
        }
    }

    #[tokio::test]
    async fn completion_before_registration_does_not_leave_finished_handle() {
        let in_flight = Arc::new(StdMutex::new(TrackedTasks::default()));
        let handle = spawn(async {});
        complete_tracked_handle(&in_flight, 42);
        register_tracked_handle(&in_flight, TrackedHandle { id: 42, handle });

        let tasks = in_flight.lock().unwrap_or_else(|p| p.into_inner());
        assert!(tasks.handles.is_empty());
        assert!(tasks.completed.is_empty());
    }
}
