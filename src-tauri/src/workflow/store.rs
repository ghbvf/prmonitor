//! `workflow_instance` persistence (#1370).

use rusqlite::{types::Type, OptionalExtension};

use crate::db::{map_err, Database};
use crate::error::AppResult;
use crate::model::{ReviewReceiptId, WorkflowInstance, WorkflowStatus, WorkflowStep, WorkflowType};

const LIST_LIMIT: i64 = 500;
const CLAIM_LIMIT: i64 = 20;
const MAX_LAST_ERROR_LEN: usize = 512;

pub(crate) fn now_epoch() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn clamp_error(error: &str) -> String {
    if error.len() <= MAX_LAST_ERROR_LEN {
        return error.to_string();
    }
    let mut end = MAX_LAST_ERROR_LEN;
    while end > 0 && !error.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &error[..end])
}

pub(crate) fn type_wire(t: WorkflowType) -> &'static str {
    match t {
        WorkflowType::ReviewNotify => "reviewNotify",
    }
}

pub(crate) fn type_from_wire(s: &str) -> AppResult<WorkflowType> {
    match s {
        "reviewNotify" => Ok(WorkflowType::ReviewNotify),
        _ => Err(crate::error::AppError::new(format!(
            "workflow_type 非法：{s}"
        ))),
    }
}

pub(crate) fn status_wire(status: WorkflowStatus) -> &'static str {
    match status {
        WorkflowStatus::Pending => "pending",
        WorkflowStatus::Running => "running",
        WorkflowStatus::Waiting => "waiting",
        WorkflowStatus::Done => "done",
        WorkflowStatus::Failed => "failed",
    }
}

pub(crate) fn status_from_wire(s: &str) -> AppResult<WorkflowStatus> {
    match s {
        "pending" => Ok(WorkflowStatus::Pending),
        "running" => Ok(WorkflowStatus::Running),
        "waiting" => Ok(WorkflowStatus::Waiting),
        "done" => Ok(WorkflowStatus::Done),
        "failed" => Ok(WorkflowStatus::Failed),
        _ => Err(crate::error::AppError::new(format!(
            "workflow status 非法：{s}"
        ))),
    }
}

pub(crate) fn step_wire(step: WorkflowStep) -> &'static str {
    match step {
        WorkflowStep::StartReview => "startReview",
        WorkflowStep::WaitReview => "waitReview",
        WorkflowStep::EnqueueNotify => "enqueueNotify",
        WorkflowStep::Done => "done",
    }
}

pub(crate) fn step_from_wire(s: &str) -> AppResult<WorkflowStep> {
    match s {
        "startReview" => Ok(WorkflowStep::StartReview),
        "waitReview" => Ok(WorkflowStep::WaitReview),
        "enqueueNotify" => Ok(WorkflowStep::EnqueueNotify),
        "done" => Ok(WorkflowStep::Done),
        _ => Err(crate::error::AppError::new(format!(
            "workflow step 非法：{s}"
        ))),
    }
}

pub struct NewWorkflow<'a> {
    pub project_id: &'a str,
    pub workflow_type: WorkflowType,
    pub input: &'a serde_json::Value,
    pub dedupe_key: &'a str,
    pub now: u64,
}

pub fn create_or_get(db: &Database, new: NewWorkflow<'_>) -> AppResult<WorkflowInstance> {
    let input_json = serde_json::to_string(new.input)
        .map_err(|e| crate::error::AppError::new(format!("workflow input 序列化失败：{e}")))?;
    db.with_tx(|tx| {
        tx.execute(
            "INSERT OR IGNORE INTO workflow_instance \
             (project_id, workflow_type, status, current_step, input_json, state_json, \
              attempt_count, next_wake_at, last_error, created_at, updated_at, dedupe_key) \
             VALUES (?1, ?2, ?3, ?4, ?5, '{}', 0, ?6, NULL, ?6, ?6, ?7)",
            rusqlite::params![
                new.project_id,
                type_wire(new.workflow_type),
                status_wire(WorkflowStatus::Pending),
                step_wire(WorkflowStep::StartReview),
                input_json,
                new.now as i64,
                new.dedupe_key,
            ],
        )
        .map_err(map_err)?;
        select_active_by_dedupe_tx(tx, new.project_id, new.workflow_type, new.dedupe_key)
    })
}

pub fn create_or_get_for_receipt(
    db: &Database,
    receipt_id: ReviewReceiptId,
    new: NewWorkflow<'_>,
) -> AppResult<WorkflowInstance> {
    let input_json = serde_json::to_string(new.input)
        .map_err(|e| crate::error::AppError::new(format!("workflow input 序列化失败：{e}")))?;
    db.with_tx(|tx| {
        let existing = tx
            .query_row(
                "SELECT workflow_id FROM review_receipt_workflow WHERE receipt_id=?1",
                [receipt_id.get()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(map_err)?;
        if let Some(id) = existing {
            return select_by_id_tx(tx, id);
        }
        tx.execute(
            "INSERT INTO workflow_instance \
             (project_id, workflow_type, status, current_step, input_json, state_json, \
              attempt_count, next_wake_at, last_error, created_at, updated_at, dedupe_key) \
             VALUES (?1, ?2, ?3, ?4, ?5, '{}', 0, ?6, NULL, ?6, ?6, ?7)",
            rusqlite::params![
                new.project_id,
                type_wire(new.workflow_type),
                status_wire(WorkflowStatus::Pending),
                step_wire(WorkflowStep::StartReview),
                input_json,
                new.now as i64,
                new.dedupe_key,
            ],
        )
        .map_err(map_err)?;
        let workflow_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO review_receipt_workflow (receipt_id, workflow_id) VALUES (?1, ?2)",
            rusqlite::params![receipt_id.get(), workflow_id],
        )
        .map_err(map_err)?;
        select_by_id_tx(tx, workflow_id)
    })
}

pub fn list_by_project(
    db: &Database,
    project_id: Option<&str>,
) -> AppResult<Vec<WorkflowInstance>> {
    db.with_conn(|conn| {
        let sql_all = "SELECT id, project_id, workflow_type, status, current_step, input_json, \
                       state_json, attempt_count, next_wake_at, last_error, created_at, updated_at \
                       FROM workflow_instance ORDER BY updated_at DESC, id DESC LIMIT ?1";
        let sql_project = "SELECT id, project_id, workflow_type, status, current_step, input_json, \
                           state_json, attempt_count, next_wake_at, last_error, created_at, updated_at \
                           FROM workflow_instance WHERE project_id = ?1 \
                           ORDER BY updated_at DESC, id DESC LIMIT ?2";
        let mut out = Vec::new();
        if let Some(project_id) = project_id {
            let mut stmt = conn.prepare(sql_project)?;
            let rows = stmt.query_map(rusqlite::params![project_id, LIST_LIMIT], row_to_instance)?;
            for row in rows {
                out.push(row?);
            }
        } else {
            let mut stmt = conn.prepare(sql_all)?;
            let rows = stmt.query_map(rusqlite::params![LIST_LIMIT], row_to_instance)?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    })
}

pub fn get(db: &Database, id: i64) -> AppResult<Option<WorkflowInstance>> {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT id, project_id, workflow_type, status, current_step, input_json, \
             state_json, attempt_count, next_wake_at, last_error, created_at, updated_at \
             FROM workflow_instance WHERE id = ?1",
            rusqlite::params![id],
            row_to_instance,
        )
        .optional()
    })
}

pub fn due_instances(db: &Database, now: u64) -> AppResult<Vec<WorkflowInstance>> {
    db.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, project_id, workflow_type, status, current_step, input_json, \
             state_json, attempt_count, next_wake_at, last_error, created_at, updated_at \
             FROM workflow_instance \
             WHERE status IN ('pending','waiting') AND next_wake_at <= ?1 \
             ORDER BY updated_at ASC, id ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![now as i64, CLAIM_LIMIT], row_to_instance)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    })
}

pub fn claim_step(
    db: &Database,
    id: i64,
    expected_step: WorkflowStep,
    lease_until: u64,
) -> AppResult<bool> {
    let now = now_epoch();
    db.with_conn(|conn| {
        let changed = conn.execute(
            "UPDATE workflow_instance SET status = ?2, next_wake_at = ?3, last_error = NULL, \
             updated_at = ?4 WHERE id = ?1 AND current_step = ?5 AND status IN ('pending','waiting')",
            rusqlite::params![
                id,
                status_wire(WorkflowStatus::Running),
                lease_until as i64,
                now as i64,
                step_wire(expected_step),
            ],
        )?;
        Ok(changed == 1)
    })
}

pub struct ProgressUpdate<'a> {
    pub id: i64,
    pub expected_step: WorkflowStep,
    pub project_id: Option<&'a str>,
    pub status: WorkflowStatus,
    pub step: WorkflowStep,
    pub state: &'a serde_json::Value,
    pub next_wake_at: u64,
}

pub fn update_progress(db: &Database, update: ProgressUpdate<'_>) -> AppResult<bool> {
    let state_json = serde_json::to_string(update.state)
        .map_err(|e| crate::error::AppError::new(format!("workflow state 序列化失败：{e}")))?;
    let now = now_epoch();
    db.with_conn(|conn| {
        let changed = if let Some(project_id) = update.project_id {
            conn.execute(
                "UPDATE workflow_instance SET project_id = ?2, status = ?3, current_step = ?4, \
                 state_json = ?5, next_wake_at = ?6, last_error = NULL, updated_at = ?7 \
                 WHERE id = ?1 AND current_step = ?8",
                rusqlite::params![
                    update.id,
                    project_id,
                    status_wire(update.status),
                    step_wire(update.step),
                    state_json,
                    update.next_wake_at as i64,
                    now as i64,
                    step_wire(update.expected_step),
                ],
            )?
        } else {
            conn.execute(
                "UPDATE workflow_instance SET status = ?2, current_step = ?3, state_json = ?4, \
                 next_wake_at = ?5, last_error = NULL, updated_at = ?6 \
                 WHERE id = ?1 AND current_step = ?7",
                rusqlite::params![
                    update.id,
                    status_wire(update.status),
                    step_wire(update.step),
                    state_json,
                    update.next_wake_at as i64,
                    now as i64,
                    step_wire(update.expected_step),
                ],
            )?
        };
        Ok(changed == 1)
    })
}

pub fn mark_failed(db: &Database, id: i64, error: &str) -> AppResult<()> {
    mark_failed_inner(db, id, None, error).map(|_| ())
}

pub fn mark_failed_expected(
    db: &Database,
    id: i64,
    expected_step: WorkflowStep,
    error: &str,
) -> AppResult<bool> {
    mark_failed_inner(db, id, Some(expected_step), error)
}

fn mark_failed_inner(
    db: &Database,
    id: i64,
    expected_step: Option<WorkflowStep>,
    error: &str,
) -> AppResult<bool> {
    let now = now_epoch();
    let error = clamp_error(error);
    db.with_conn(|conn| {
        let changed = if let Some(expected_step) = expected_step {
            conn.execute(
                "UPDATE workflow_instance SET status = ?2, last_error = ?3, \
                 attempt_count = attempt_count + 1, updated_at = ?4 \
                 WHERE id = ?1 AND current_step = ?5 AND status = 'running'",
                rusqlite::params![
                    id,
                    status_wire(WorkflowStatus::Failed),
                    error,
                    now as i64,
                    step_wire(expected_step),
                ],
            )?
        } else {
            conn.execute(
                "UPDATE workflow_instance SET status = ?2, last_error = ?3, attempt_count = attempt_count + 1, \
                 updated_at = ?4 WHERE id = ?1",
                rusqlite::params![id, status_wire(WorkflowStatus::Failed), error, now as i64],
            )?
        };
        Ok(changed == 1)
    })
}

pub fn reset_for_retry(db: &Database, id: i64) -> AppResult<bool> {
    let now = now_epoch();
    db.with_conn(|conn| {
        let changed = conn.execute(
            "UPDATE workflow_instance SET status = ?2, next_wake_at = ?3, \
             last_error = NULL, updated_at = ?3 WHERE id = ?1 AND status = 'failed'",
            rusqlite::params![id, status_wire(WorkflowStatus::Pending), now as i64,],
        )?;
        Ok(changed == 1)
    })
}

pub fn reset_running_for_recovery(db: &Database, now: u64) -> AppResult<usize> {
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE workflow_instance SET status = ?1, next_wake_at = ?2, updated_at = ?2 \
             WHERE status = 'running'",
            rusqlite::params![status_wire(WorkflowStatus::Pending), now as i64],
        )
    })
}

pub fn reset_expired_running(db: &Database, now: u64) -> AppResult<usize> {
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE workflow_instance SET status = ?1, next_wake_at = ?2, updated_at = ?2 \
             WHERE status = 'running' AND next_wake_at <= ?2",
            rusqlite::params![status_wire(WorkflowStatus::Pending), now as i64],
        )
    })
}

fn select_active_by_dedupe_tx(
    tx: &rusqlite::Transaction<'_>,
    project_id: &str,
    workflow_type: WorkflowType,
    dedupe_key: &str,
) -> AppResult<WorkflowInstance> {
    tx.query_row(
        "SELECT id, project_id, workflow_type, status, current_step, input_json, \
         state_json, attempt_count, next_wake_at, last_error, created_at, updated_at \
         FROM workflow_instance \
         WHERE project_id = ?1 AND workflow_type = ?2 AND dedupe_key = ?3 \
           AND status IN ('pending','running','waiting') \
         ORDER BY id DESC LIMIT 1",
        rusqlite::params![project_id, type_wire(workflow_type), dedupe_key],
        row_to_instance,
    )
    .map_err(map_err)
}

fn select_by_id_tx(tx: &rusqlite::Transaction<'_>, id: i64) -> AppResult<WorkflowInstance> {
    tx.query_row(
        "SELECT id, project_id, workflow_type, status, current_step, input_json, \
         state_json, attempt_count, next_wake_at, last_error, created_at, updated_at \
         FROM workflow_instance WHERE id=?1",
        [id],
        row_to_instance,
    )
    .map_err(map_err)
}

fn row_to_instance(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkflowInstance> {
    let workflow_type: String = row.get(2)?;
    let status: String = row.get(3)?;
    let current_step: String = row.get(4)?;
    let input_json: String = row.get(5)?;
    let state_json: String = row.get(6)?;
    let decode_err = |message: String| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            Type::Text,
            Box::new(std::io::Error::other(message)),
        )
    };
    Ok(WorkflowInstance {
        id: row.get(0)?,
        project_id: row.get(1)?,
        workflow_type: type_from_wire(&workflow_type).map_err(|e| decode_err(e.message))?,
        status: status_from_wire(&status).map_err(|e| decode_err(e.message))?,
        current_step: step_from_wire(&current_step).map_err(|e| decode_err(e.message))?,
        input: serde_json::from_str(&input_json)
            .map_err(|e| decode_err(format!("workflow input_json 非法：{e}")))?,
        state: serde_json::from_str(&state_json)
            .map_err(|e| decode_err(format!("workflow state_json 非法：{e}")))?,
        attempt_count: row.get::<_, i64>(7)? as u32,
        next_wake_at: row.get::<_, i64>(8)? as u64,
        last_error: row.get(9)?,
        created_at: row.get::<_, i64>(10)? as u64,
        updated_at: row.get::<_, i64>(11)? as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_wire_strings_are_pinned() {
        assert_eq!(type_wire(WorkflowType::ReviewNotify), "reviewNotify");
        assert_eq!(status_wire(WorkflowStatus::Pending), "pending");
        assert_eq!(status_wire(WorkflowStatus::Running), "running");
        assert_eq!(status_wire(WorkflowStatus::Waiting), "waiting");
        assert_eq!(status_wire(WorkflowStatus::Done), "done");
        assert_eq!(status_wire(WorkflowStatus::Failed), "failed");
        assert_eq!(step_wire(WorkflowStep::StartReview), "startReview");
        assert_eq!(step_wire(WorkflowStep::WaitReview), "waitReview");
        assert_eq!(step_wire(WorkflowStep::EnqueueNotify), "enqueueNotify");
        assert_eq!(step_wire(WorkflowStep::Done), "done");
        assert_eq!(
            type_from_wire("reviewNotify").expect("type"),
            WorkflowType::ReviewNotify
        );
        assert!(type_from_wire("unknown").is_err());
        assert!(status_from_wire("unknown").is_err());
        assert!(step_from_wire("unknown").is_err());
    }

    #[test]
    fn create_is_idempotent_and_progress_is_cas() {
        let db = Database::open_in_memory().expect("db");
        let input = serde_json::json!({"reference":"repo","prNumber":7,"skill_key":"review"});
        let first = create_or_get(
            &db,
            NewWorkflow {
                project_id: "repo",
                workflow_type: WorkflowType::ReviewNotify,
                input: &input,
                dedupe_key: "k1",
                now: 10,
            },
        )
        .expect("first");
        let second = create_or_get(
            &db,
            NewWorkflow {
                project_id: "repo",
                workflow_type: WorkflowType::ReviewNotify,
                input: &input,
                dedupe_key: "k1",
                now: 20,
            },
        )
        .expect("second");
        assert_eq!(first.id, second.id);

        let state = serde_json::json!({"reviewThreadId":"t1"});
        assert!(update_progress(
            &db,
            ProgressUpdate {
                id: first.id,
                expected_step: WorkflowStep::StartReview,
                project_id: Some("p1"),
                status: WorkflowStatus::Waiting,
                step: WorkflowStep::WaitReview,
                state: &state,
                next_wake_at: 11,
            },
        )
        .expect("advance"));
        assert!(!update_progress(
            &db,
            ProgressUpdate {
                id: first.id,
                expected_step: WorkflowStep::StartReview,
                project_id: None,
                status: WorkflowStatus::Waiting,
                step: WorkflowStep::WaitReview,
                state: &state,
                next_wake_at: 11,
            },
        )
        .expect("stale advance"));
        mark_failed(&db, first.id, "boom").expect("fail");
        assert!(reset_for_retry(&db, first.id).expect("retry"));

        let got = get(&db, first.id).expect("get").expect("row");
        assert_eq!(got.project_id, "p1");
        assert_eq!(got.current_step, WorkflowStep::WaitReview);
        assert_eq!(got.status, WorkflowStatus::Pending);
        assert_eq!(got.last_error, None);
        assert_eq!(got.state["reviewThreadId"], "t1");
    }

    #[test]
    fn terminal_workflow_does_not_block_new_dedupe_instance() {
        let db = Database::open_in_memory().expect("db");
        let input = serde_json::json!({"reference":"repo","prNumber":7,"skill_key":"review"});
        let first = create_or_get(
            &db,
            NewWorkflow {
                project_id: "repo",
                workflow_type: WorkflowType::ReviewNotify,
                input: &input,
                dedupe_key: "k1",
                now: 10,
            },
        )
        .expect("first");
        let state = serde_json::json!({"notificationOutboxIds":[9]});
        assert!(update_progress(
            &db,
            ProgressUpdate {
                id: first.id,
                expected_step: WorkflowStep::StartReview,
                project_id: None,
                status: WorkflowStatus::Done,
                step: WorkflowStep::Done,
                state: &state,
                next_wake_at: 0,
            },
        )
        .expect("complete"));

        let second = create_or_get(
            &db,
            NewWorkflow {
                project_id: "repo",
                workflow_type: WorkflowType::ReviewNotify,
                input: &input,
                dedupe_key: "k1",
                now: 20,
            },
        )
        .expect("second");
        assert_ne!(
            first.id, second.id,
            "terminal rows must not permanently suppress a later deeplink"
        );
        let third = create_or_get(
            &db,
            NewWorkflow {
                project_id: "repo",
                workflow_type: WorkflowType::ReviewNotify,
                input: &input,
                dedupe_key: "k1",
                now: 30,
            },
        )
        .expect("third");
        assert_eq!(
            second.id, third.id,
            "active rows still dedupe repeated deeplink clicks"
        );
    }

    #[test]
    fn receipt_identity_survives_terminal_workflow_and_prevents_duplicate_notification_saga() {
        let db = Database::open_in_memory().expect("db");
        let receipt_id = ReviewReceiptId::new(17).expect("receipt");
        let input = serde_json::json!({
            "receiptId": 17, "reference": "repo", "prNumber": 7, "skill_key": "review"
        });
        let create = |now| NewWorkflow {
            project_id: "repo",
            workflow_type: WorkflowType::ReviewNotify,
            input: &input,
            dedupe_key: "receipt:17",
            now,
        };
        let first = create_or_get_for_receipt(&db, receipt_id, create(10)).expect("first");
        assert!(update_progress(
            &db,
            ProgressUpdate {
                id: first.id,
                expected_step: WorkflowStep::StartReview,
                project_id: None,
                status: WorkflowStatus::Done,
                step: WorkflowStep::Done,
                state: &serde_json::json!({"notificationOutboxIds":[9]}),
                next_wake_at: 0,
            },
        )
        .expect("terminalize"));

        let replay = create_or_get_for_receipt(&db, receipt_id, create(20)).expect("replay");
        assert_eq!(replay.id, first.id);
        let count: i64 = db
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM workflow_instance", [], |r| r.get(0))
            })
            .expect("count");
        assert_eq!(count, 1);
    }

    #[test]
    fn claim_step_is_status_cas_and_running_recovers_on_restart() {
        let db = Database::open_in_memory().expect("db");
        let input = serde_json::json!({});
        let row = create_or_get(
            &db,
            NewWorkflow {
                project_id: "p1",
                workflow_type: WorkflowType::ReviewNotify,
                input: &input,
                dedupe_key: "k1",
                now: 10,
            },
        )
        .expect("create");

        assert!(claim_step(&db, row.id, WorkflowStep::StartReview, 99).expect("claim"));
        assert!(
            !claim_step(&db, row.id, WorkflowStep::StartReview, 99).expect("duplicate claim"),
            "status CAS prevents duplicate side-effect runners for the same step"
        );
        assert!(due_instances(&db, 99).expect("due").is_empty());

        assert_eq!(
            reset_running_for_recovery(&db, 100).expect("recover"),
            1,
            "startup recovery releases stale running rows"
        );
        let recovered = get(&db, row.id).expect("get").expect("row");
        assert_eq!(recovered.status, WorkflowStatus::Pending);
        assert_eq!(
            due_instances(&db, 100).expect("due").len(),
            1,
            "recovered row is due again"
        );
    }

    #[test]
    fn expired_running_lease_recovers_without_restart() {
        let db = Database::open_in_memory().expect("db");
        let input = serde_json::json!({});
        let row = create_or_get(
            &db,
            NewWorkflow {
                project_id: "p1",
                workflow_type: WorkflowType::ReviewNotify,
                input: &input,
                dedupe_key: "k1",
                now: 10,
            },
        )
        .expect("create");
        assert!(claim_step(&db, row.id, WorkflowStep::StartReview, 50).expect("claim"));
        assert_eq!(reset_expired_running(&db, 49).expect("not expired"), 0);
        assert!(due_instances(&db, 50).expect("due").is_empty());

        assert_eq!(reset_expired_running(&db, 50).expect("expired"), 1);
        let recovered = get(&db, row.id).expect("get").expect("row");
        assert_eq!(recovered.status, WorkflowStatus::Pending);
        assert_eq!(due_instances(&db, 50).expect("due").len(), 1);
    }

    #[test]
    fn corrupt_wire_values_are_read_errors_not_normalized() {
        let db = Database::open_in_memory().expect("db");
        let input = serde_json::json!({});
        let row = create_or_get(
            &db,
            NewWorkflow {
                project_id: "p1",
                workflow_type: WorkflowType::ReviewNotify,
                input: &input,
                dedupe_key: "k1",
                now: 10,
            },
        )
        .expect("create");

        db.with_conn(|conn| {
            conn.execute(
                "UPDATE workflow_instance SET current_step = 'futureStep' WHERE id = ?1",
                rusqlite::params![row.id],
            )
            .map(|_| ())
        })
        .expect("corrupt step");
        assert!(
            get(&db, row.id).is_err(),
            "unknown step must surface as a read error"
        );

        db.with_conn(|conn| {
            conn.execute(
                "UPDATE workflow_instance SET current_step = 'startReview', state_json = '{' WHERE id = ?1",
                rusqlite::params![row.id],
            )
            .map(|_| ())
        })
        .expect("corrupt json");
        assert!(
            get(&db, row.id).is_err(),
            "invalid JSON must surface as a read error"
        );
    }

    #[test]
    fn failed_workflow_is_not_recovered_until_retry() {
        let db = Database::open_in_memory().expect("db");
        let input = serde_json::json!({});
        let row = create_or_get(
            &db,
            NewWorkflow {
                project_id: "p1",
                workflow_type: WorkflowType::ReviewNotify,
                input: &input,
                dedupe_key: "k1",
                now: 10,
            },
        )
        .expect("create");
        mark_failed(&db, row.id, "boom").expect("fail");
        assert!(due_instances(&db, 99).expect("due").is_empty());
        assert!(reset_for_retry(&db, row.id).expect("retry"));
        assert_eq!(
            due_instances(&db, now_epoch().saturating_add(60))
                .expect("due")
                .len(),
            1
        );
    }
}
