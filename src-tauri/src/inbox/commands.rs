//! Inbox slice Tauri commands (AB#1065): list entries, read a raw payload, replay an entry.
//!
//! Thin shells over [`super::store`] / [`super::service`]: they resolve the `Database` (and, for
//! replay, the composition-installed hooks on [`crate::state::AppState`]) and delegate. The wire
//! contract (`InboxEntry` newest-first; `inbox_get_raw` errs on an unknown id; `inbox_replay`
//! re-processes + re-emits) matches the frontend mirror exactly.

use tauri::Manager;

use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::model::InboxEntry;
use crate::state::AppState;

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

/// Re-process a stored inbox entry by id (AB#1065/#1379): re-feed a GitHub delivery through the
/// vetted dispatch path, re-invoke the Azure refresh, or re-produce a candidate-backed outbox action;
/// then mark the entry Processed/Failed and re-emit `inbox:updated`. Works on ANY entry. Fails
/// closed if the composition root hasn't installed the replay hooks yet.
#[tauri::command]
pub async fn inbox_replay(app: tauri::AppHandle, id: i64) -> AppResult<()> {
    let github_refeed = app
        .state::<AppState>()
        .inbox
        .github_refeed()
        .ok_or_else(|| AppError::new("inbox 重放未初始化（github_refeed 未安装）".to_string()))?;
    let refresher = app
        .state::<AppState>()
        .inbox
        .refresher()
        .ok_or_else(|| AppError::new("inbox 重放未初始化（refresher 未安装）".to_string()))?;
    let candidate_dispatch = app
        .state::<AppState>()
        .inbox
        .candidate_dispatch()
        .ok_or_else(|| {
            AppError::new("inbox 重放未初始化（candidate_dispatch 未安装）".to_string())
        })?;
    // Resolve the DB handle, then delegate. Cloning the handle out of `State` is not needed — the
    // service borrows `&Database` for the duration of the await (the connection mutex is internal).
    let db = app.state::<Database>();
    super::service::replay(
        &app,
        db.inner(),
        &github_refeed,
        &refresher,
        &candidate_dispatch,
        id,
    )
    .await
}
