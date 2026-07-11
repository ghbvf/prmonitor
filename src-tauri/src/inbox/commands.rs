//! Inbox slice Tauri commands (AB#1065): list entries, read a raw payload, replay an entry.
//!
//! Thin shells over [`super::store`] / [`super::service`]: they resolve the `Database` (and, for
//! replay, the composition-installed hooks on [`crate::state::AppState`]) and delegate. The wire
//! contract (`InboxEntry` newest-first; `inbox_get_raw` errs on an unknown id; `inbox_replay`
//! only requeues a failed entry for the single worker) matches the frontend mirror exactly.

use tauri::Manager;

use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::model::InboxEntry;

/// List inbox entries, NEWEST FIRST (AB#1065). `project_id: None` lists across all projects (the
/// panel's "all" view); `Some(pid)` scopes to one project.
#[tauri::command]
pub async fn inbox_list<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: Option<String>,
) -> AppResult<Vec<InboxEntry>> {
    let db = app.state::<Database>();
    super::store::list_by_project(db.inner(), project_id.as_deref())
}

/// The verbatim raw payload of an inbox entry by id (AB#1065). Errors when the id is unknown
/// (the command-level "err if unknown" decision — the store returns `None`, this maps it to an
/// `AppError` so the frontend distinguishes "no such entry" from an empty body).
#[tauri::command]
pub async fn inbox_get_raw<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    id: i64,
) -> AppResult<String> {
    let db = app.state::<Database>();
    super::store::get_raw(db.inner(), id)?
        .ok_or_else(|| AppError::new(format!("inbox 条目不存在（id={id}）")))
}

/// Requeue a failed inbox entry (AB#1065/#1379). This command performs only the guarded
/// `Failed → Received` transition and wakes the single consumer worker; it never executes the
/// entry inline. Unknown ids and every other status fail closed.
#[tauri::command]
pub async fn inbox_replay(app: tauri::AppHandle, id: i64) -> AppResult<()> {
    let db = app.state::<Database>();
    super::service::requeue_failed(&app, db.inner(), id)
}
