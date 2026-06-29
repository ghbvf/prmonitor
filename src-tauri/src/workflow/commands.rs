//! Workflow Tauri commands (#1370).

use tauri::Manager;

use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::model::WorkflowInstance;
use crate::state::AppState;

#[tauri::command]
pub async fn workflow_list<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: Option<String>,
) -> AppResult<Vec<WorkflowInstance>> {
    let db = app.state::<Database>();
    crate::workflow::store::list_by_project(db.inner(), project_id.as_deref())
}

#[tauri::command]
pub async fn workflow_get<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    id: i64,
) -> AppResult<WorkflowInstance> {
    let db = app.state::<Database>();
    crate::workflow::store::get(db.inner(), id)?
        .ok_or_else(|| AppError::new(format!("workflow 不存在（id={id}）")))
}

#[tauri::command]
pub async fn workflow_get_raw<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    id: i64,
) -> AppResult<String> {
    let instance = workflow_get(app, id).await?;
    serde_json::to_string_pretty(&instance)
        .map_err(|e| AppError::new(format!("workflow raw 序列化失败：{e}")))
}

#[tauri::command]
pub async fn workflow_retry<R: tauri::Runtime>(app: tauri::AppHandle<R>, id: i64) -> AppResult<()> {
    let db = app.state::<Database>();
    if !crate::workflow::store::reset_for_retry(db.inner(), id)? {
        return Err(AppError::new(format!(
            "workflow {id} 非 failed 状态，仅失败 workflow 可重试"
        )));
    }
    crate::workflow::service::announce_updated(&app, db.inner(), id);
    app.state::<AppState>().workflow.wake();
    Ok(())
}
