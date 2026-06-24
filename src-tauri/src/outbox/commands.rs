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

/// Re-queue a (typically dead-lettered) outbox entry for another run (AB#1066): reset it to
/// `pending` with a fresh retry budget due now and WAKE the worker so it re-executes promptly (the
/// worker then re-emits `outbox:updated` as it transitions the row). Errors when the id is unknown
/// (the analog of `inbox_replay`'s "err if unknown"). The frontend reconciles deterministically via
/// the store's `refresh()` after this command resolves, so the row flips to `pending` immediately in
/// the panel even before the worker runs.
#[tauri::command]
pub async fn outbox_retry<R: tauri::Runtime>(app: tauri::AppHandle<R>, id: i64) -> AppResult<()> {
    let db = app.state::<Database>();
    let now = store::now_epoch();
    if !store::reset_for_retry(db.inner(), id, now)? {
        return Err(AppError::new(format!("outbox 条目不存在（id={id}）")));
    }
    app.state::<AppState>().outbox.wake();
    Ok(())
}
