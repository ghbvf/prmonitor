//! Outbox slice Tauri commands (AB#1066): list entries, read a raw payload, retry an entry.
//!
//! Thin shells over [`super::store`] / [`super::service`]: they resolve the `Database` (and, for
//! retry, the worker handle on [`crate::state::AppState`]) and delegate. The wire contract
//! (`OutboxEntry` newest-first; `outbox_get_raw` errs on an unknown id; `outbox_retry` re-queues +
//! wakes the worker) matches the frontend mirror exactly — the symmetric counterpart of the inbox
//! commands.

use tauri::Manager;

use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::model::OutboxEntry;
use crate::outbox::store;
use crate::state::AppState;

/// List outbox entries, NEWEST FIRST (AB#1066). `project_id: None` lists across all projects (the
/// panel's "all" view); `Some(pid)` scopes to one project.
#[tauri::command]
pub async fn outbox_list<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: Option<String>,
) -> AppResult<Vec<OutboxEntry>> {
    let db = app.state::<Database>();
    store::list_by_project(db.inner(), project_id.as_deref())
}

/// The verbatim serialized payload of an outbox entry by id (AB#1066) — the action body (a
/// `Notification` JSON today), for the "View raw" disclosure. Errors when the id is unknown (the
/// store returns `None`; this maps it to an `AppError` so the frontend distinguishes "no such
/// entry" from an empty body), mirroring `inbox_get_raw`.
#[tauri::command]
pub async fn outbox_get_raw<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    id: i64,
) -> AppResult<String> {
    let db = app.state::<Database>();
    store::get_raw(db.inner(), id)?
        .ok_or_else(|| AppError::new(format!("outbox 条目不存在（id={id}）")))
}

/// Re-queue a DEAD-LETTERED outbox entry for another run (AB#1066): reset it to `pending` with a
/// fresh retry budget due now, emit `outbox:updated` for the `pending` flip (so the panel reflects it
/// immediately, symmetric with `service::enqueue`), and WAKE the worker so it re-executes promptly
/// (the worker re-emits again as it transitions the row to done/dead). Only a `dead` row is
/// retryable: an unknown id errors ("不存在"), and a non-`dead` row errors ("仅 dead 行可重试") rather
/// than re-running an already-succeeded / still-queued action (the backend invariant behind the
/// UI's dead-only retry button). The store's `refresh()` after this resolves is the belt-and-
/// suspenders deterministic reconcile.
#[tauri::command]
pub async fn outbox_retry<R: tauri::Runtime>(app: tauri::AppHandle<R>, id: i64) -> AppResult<()> {
    let db = app.state::<Database>();
    let now = store::now_epoch();
    match store::reset_for_retry(db.inner(), id, now)? {
        store::RetryReset::Requeued => {}
        store::RetryReset::Unknown => {
            return Err(AppError::new(format!("outbox 条目不存在（id={id}）")))
        }
        store::RetryReset::NotDead => {
            return Err(AppError::new(format!(
                "outbox 条目 {id} 非 dead 状态，仅 dead（最终失败）行可重试"
            )))
        }
    }
    super::service::announce_updated(&app, db.inner(), id);
    app.state::<AppState>().outbox.wake();
    Ok(())
}
