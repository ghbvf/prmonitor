use tauri::Manager;

use crate::db::Database;
use crate::error::AppResult;

#[tauri::command]
pub fn rule_matches_for_inbox<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    inbox_event_id: i64,
) -> AppResult<Vec<super::store::RuleMatchEntry>> {
    let db = app.state::<Database>();
    super::store::list_by_inbox(db.inner(), inbox_event_id)
}
