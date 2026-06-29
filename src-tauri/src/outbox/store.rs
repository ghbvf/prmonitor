//! `action_outbox` persistence (AB#1066): the slice-local queries over the outbox table.
//!
//! Slice-local — the outbox slice owns these queries, reaching SQLite only through the horizontal
//! [`crate::db::Database`] handle (not a cross-slice import), exactly as `review::history_store` /
//! `inbox::store` own their tables.
//!
//! [`crate::model::ActionStatus`]'s exhaustive `match` in [`status_as_wire`] and
//! [`crate::model::ActionKind`]'s in [`kind_as_wire`] are the **Hard** carriers for the
//! status/kind ↔ column-string mapping (a new variant is a compile error here, like
//! `inbox::store::status_as_wire`).

use rusqlite::{OptionalExtension, Transaction};

use crate::db::{map_err, Database};
use crate::error::{AppError, AppResult};
use crate::model::{ActionKind, ActionStatus, OutboxEntry};
use crate::outbox::OutboxAction;

/// Bound on a single [`list_by_project`] page (AB#1066): the panel only needs a recent window, and
/// `idx_action_outbox_project` makes the `ORDER BY id DESC LIMIT` a cheap top-N read.
const LIST_LIMIT: i64 = 500;

/// Bound on a single [`claim_due`] batch (AB#1066): the worker drains due rows a page at a time so
/// one cycle can't hydrate an unbounded backlog into memory; the next tick/wake picks up the rest.
const CLAIM_LIMIT: i64 = 100;

/// Global retention cap on TERMINAL (`done` / `dead`) outbox rows (AB#1066). Every produced action
/// inserts a row; nothing else deletes them, so without a cap the table grows unbounded. [`enqueue`]
/// prunes the oldest terminal rows beyond this cap on each insert (mirroring `inbox::store`'s
/// `MAX_INBOX_EVENTS`). Only terminal rows are pruned — a `pending` row is an un-run action and must
/// never be dropped by the cap (the worker drains those), so the bound is a backstop on history, not
/// on the live queue.
const MAX_OUTBOX_TERMINAL: i64 = 5000;

/// Cap on a stored `last_error` (AB#1066, security review): the error is persisted and surfaced to
/// the frontend panel, so an unbounded message (a giant serde error, or a crafted value echoed
/// through `kind_from_wire`) is truncated at this byte budget. Defense-in-depth — the action
/// `payload` (a `Notification` today) must itself carry no secret, since both it and `last_error`
/// are panel-visible.
const MAX_LAST_ERROR_LEN: usize = 512;

/// Truncate a `last_error` to [`MAX_LAST_ERROR_LEN`] on a char boundary (never mid-UTF-8), appending
/// an ellipsis marker when cut. Applied at every `last_error` write so no path can persist an
/// unbounded message. `pub(crate)` so the service layer can apply the SAME bound to the
/// panel-visible cycle-error message (AB#1182), not just persisted row errors.
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

/// Wall-clock seconds for the outbox timestamps. Slice-local (each slice keeps its own `now_epoch`
/// to stay self-contained). Degrades to 0 on a pre-epoch clock rather than panicking.
pub(crate) fn now_epoch() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn sqlite_epoch(field: &str, value: u64) -> AppResult<i64> {
    i64::try_from(value)
        .map_err(|_| AppError::new(format!("{field} 超出 SQLite i64 时间戳存储上限: {value}")))
}

/// [`ActionStatus`] → its pinned wire string (the form stored in the `status` column).
///
/// The **Hard** carrier (exhaustive `match` over the sealed enum) for the status ↔ column mapping:
/// adding a variant without an arm is a compile error. The strings are the SAME serde wire values
/// the frontend mirrors (locked equal to the serde form by `status_wire_matches_serde_and_round_trips`),
/// so the DB column and the `src/types.ts` union can't drift apart.
pub(crate) fn status_as_wire(status: ActionStatus) -> &'static str {
    match status {
        ActionStatus::Pending => "pending",
        ActionStatus::Done => "done",
        ActionStatus::Dead => "dead",
    }
}

/// Wire string → [`ActionStatus`] (reverse of [`status_as_wire`]). An unknown / corrupt stored
/// value degrades to [`ActionStatus::Dead`] (a terminal state — never resurrects a corrupt row as a
/// live `pending`), mirroring `inbox::store::status_from_wire`'s lenient default to `Failed`.
pub(crate) fn status_from_wire(s: &str) -> ActionStatus {
    match s {
        "pending" => ActionStatus::Pending,
        "done" => ActionStatus::Done,
        _ => ActionStatus::Dead,
    }
}

/// [`ActionKind`] → its pinned DB wire string (== the serde form). A unit enum with
/// `rename_all = "camelCase"`, so serialization CANNOT fail — `expect` fail-fast surfaces a serde
/// regression loudly rather than silently storing an empty kind.
pub(crate) fn kind_as_wire(kind: ActionKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("ActionKind serializes to a JSON string (unit enum, known variants)")
}

/// [`ActionKind`] from its DB wire string (reverse of [`kind_as_wire`]), STRICT. The worker routes
/// the side effect on this kind, so it must NOT silently default an unparseable stored value to a
/// concrete kind. An unknown / corrupt value is an explicit [`AppError`]; [`claim_due`] DEAD-LETTERS
/// such a row (quarantine — a corrupt row must not blank the batch or crash the worker, nor linger).
fn kind_from_wire(s: &str) -> AppResult<ActionKind> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| crate::error::AppError::new(format!("outbox 行的 kind 无法识别：{s:?}")))
}

/// Enqueue one produced action as a NEW `pending` row due now (AB#1066), returning its row id. The
/// row starts at `attempt_count = 0`, `next_attempt_at = now`, `last_error = NULL`. Insert + prune
/// run in ONE `with_tx` (mirroring `inbox::store::insert_dedup`) so a prune failure can't leave the
/// new row half-pruned; both commit / roll back atomically.
pub fn enqueue(
    db: &Database,
    project_id: &str,
    kind: ActionKind,
    summary: &str,
    payload: &str,
    now: u64,
) -> AppResult<i64> {
    enqueue_inner(db, project_id, kind, summary, payload, None, now)
}

pub(crate) struct EnqueueInput<'a> {
    pub(crate) project_id: &'a str,
    pub(crate) kind: ActionKind,
    pub(crate) summary: &'a str,
    pub(crate) payload: &'a str,
    pub(crate) dedupe_key: Option<&'a str>,
    pub(crate) next_attempt_at: Option<u64>,
}

/// Enqueue one produced action with a live-pending dedupe key (#1379). If the same project already
/// has a pending row for `dedupe_key`, return that row id instead of inserting another pending
/// action. Once the row reaches `done`/`dead`, the partial unique index no longer applies and a new
/// action for a new attempt can be queued intentionally.
pub fn enqueue_deduped(
    db: &Database,
    project_id: &str,
    kind: ActionKind,
    summary: &str,
    payload: &str,
    dedupe_key: &str,
    now: u64,
) -> AppResult<i64> {
    enqueue_inner(
        db,
        project_id,
        kind,
        summary,
        payload,
        Some(dedupe_key),
        now,
    )
}

pub(crate) fn id_by_dedupe_key_any_status(
    db: &Database,
    project_id: &str,
    dedupe_key: &str,
) -> AppResult<Option<i64>> {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT id FROM action_outbox \
             WHERE project_id = ?1 AND dedupe_key = ?2 \
             ORDER BY id LIMIT 1",
            rusqlite::params![project_id, dedupe_key],
            |r| r.get::<_, i64>(0),
        )
        .optional()
    })
}

pub(crate) fn id_by_dedupe_key_any_status_in_tx(
    tx: &Transaction<'_>,
    project_id: &str,
    dedupe_key: &str,
) -> AppResult<Option<i64>> {
    tx.query_row(
        "SELECT id FROM action_outbox \
         WHERE project_id = ?1 AND dedupe_key = ?2 \
         ORDER BY id LIMIT 1",
        rusqlite::params![project_id, dedupe_key],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .map_err(map_err)
}

pub(crate) fn enqueue_in_tx(
    tx: &Transaction<'_>,
    row: &EnqueueInput<'_>,
    now: u64,
) -> AppResult<i64> {
    enqueue_inner_tx(tx, row, now)
}

fn enqueue_inner(
    db: &Database,
    project_id: &str,
    kind: ActionKind,
    summary: &str,
    payload: &str,
    dedupe_key: Option<&str>,
    now: u64,
) -> AppResult<i64> {
    db.with_tx(|tx| {
        let row = EnqueueInput {
            project_id,
            kind,
            summary,
            payload,
            dedupe_key,
            next_attempt_at: Some(now),
        };
        enqueue_inner_tx(tx, &row, now)
    })
}

fn enqueue_inner_tx(tx: &Transaction<'_>, row: &EnqueueInput<'_>, now: u64) -> AppResult<i64> {
    let next_attempt_at = row.next_attempt_at.unwrap_or(now);
    let next_attempt_at = sqlite_epoch("next_attempt_at", next_attempt_at)?;
    let now = sqlite_epoch("now", now)?;
    tx.execute(
        "INSERT OR IGNORE INTO action_outbox \
         (project_id, kind, summary, payload, status, attempt_count, next_attempt_at, \
          last_error, created_at, updated_at, dedupe_key) \
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, NULL, ?7, ?7, ?8)",
        rusqlite::params![
            row.project_id,
            kind_as_wire(row.kind),
            row.summary,
            row.payload,
            status_as_wire(ActionStatus::Pending),
            next_attempt_at,
            now,
            row.dedupe_key,
        ],
    )
    .map_err(map_err)?;
    let id = if tx.changes() == 1 {
        tx.last_insert_rowid()
    } else {
        tx.query_row(
            "SELECT id FROM action_outbox \
             WHERE project_id = ?1 AND dedupe_key = ?2 AND status = 'pending' \
             ORDER BY id LIMIT 1",
            rusqlite::params![row.project_id, row.dedupe_key],
            |r| r.get::<_, i64>(0),
        )
        .map_err(map_err)?
    };
    // Cap stored history: drop the oldest TERMINAL rows beyond MAX_OUTBOX_TERMINAL (never a
    // `pending` row — that is an un-run action). The `LIMIT -1 OFFSET ?` no-ops cheaply when
    // under the cap.
    tx.execute(
        "DELETE FROM action_outbox WHERE id IN ( \
             SELECT id FROM action_outbox WHERE status IN ('done', 'dead') \
             ORDER BY id DESC LIMIT -1 OFFSET ?1)",
        rusqlite::params![MAX_OUTBOX_TERMINAL],
    )
    .map_err(map_err)?;
    Ok(id)
}

/// Claim the due actions (AB#1066): `pending` rows whose `next_attempt_at <= now`, OLDEST FIRST
/// (`ORDER BY id`), bounded to [`CLAIM_LIMIT`]. The `idx_action_outbox_due` index serves the
/// predicate.
///
/// A row whose stored `kind` is unrecognized is QUARANTINED — dead-lettered in place (not just
/// skipped). The schema-version forward-compat guard ([`crate::db`]) means an older binary refuses
/// to open a newer DB, so a `kind` this binary can't parse is genuine corruption/tampering, never a
/// legitimate future kind — dead-lettering it is correct (it becomes terminal `dead`, visible in the
/// panel, and stops being re-selected + re-logged every cycle).
///
/// Returns `(actions, quarantined_ids)`: the well-formed actions to execute, plus the ids of rows
/// just dead-lettered as corrupt. The caller ([`super::service::run_due_once`]) emits
/// `outbox:updated` for the quarantined ids too, so an open panel sees the `dead` transition (the
/// store has no `AppHandle` to emit itself — all persisted transitions surface through the service).
pub fn claim_due(db: &Database, now: u64) -> AppResult<(Vec<OutboxAction>, Vec<i64>)> {
    let now_sql = sqlite_epoch("now", now)?;
    // The 6th column `created_at` (AB#1182) lets the caller dead-letter a row that has sat
    // `pending` past its kind's staleness TTL instead of executing a stale action.
    let rows: Vec<(i64, String, String, String, i64, i64)> = db.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, project_id, kind, payload, attempt_count, created_at FROM action_outbox \
             WHERE status = 'pending' AND next_attempt_at <= ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![now_sql, CLAIM_LIMIT], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    })?;

    // Observability (AB#1066): a full page means the pending queue depth is ≥ CLAIM_LIMIT — the
    // worker is draining a backlog a page at a time. Surface it so an unexpectedly deep queue (a
    // producer outpacing the worker) is diagnosable rather than silent.
    if rows.len() as i64 >= CLAIM_LIMIT {
        eprintln!("outbox: claim 批次已达上限 {CLAIM_LIMIT}，pending 队列可能积压（下个周期续清）");
    }

    let mut actions = Vec::with_capacity(rows.len());
    let mut quarantined = Vec::new();
    for (id, project_id, kind, payload, attempt_count, created_at) in rows {
        let attempt_count = attempt_count.max(0) as u32;
        match kind_from_wire(&kind) {
            Ok(kind) => actions.push(OutboxAction {
                id,
                project_id,
                kind,
                payload,
                attempt_count,
                created_at: created_at.max(0) as u64,
            }),
            Err(e) => {
                // Quarantine: dead-letter the corrupt row so it terminalizes (panel-visible) instead
                // of being re-skipped + re-logged forever. Best-effort — a write failure here just
                // leaves it `pending` to retry the quarantine next cycle, never a false `done`.
                match mark_dead(db, id, attempt_count, &e.message, now) {
                    Ok(()) => quarantined.push(id),
                    Err(mark_err) => eprintln!(
                        "outbox: 死信 kind 损坏的行失败（id={id}）：{}",
                        mark_err.message
                    ),
                }
            }
        }
    }
    Ok((actions, quarantined))
}

/// Mark an outbox row `done` (AB#1066), recording the FINAL `attempt_count` (the attempt that
/// succeeded — so a done row reflects its real execution count, consistent with the failure paths
/// that also write `attempt_count`), stamping `updated_at`, and clearing any prior `last_error`. A
/// no-op if the id doesn't exist.
pub fn mark_done(db: &Database, id: i64, attempt_count: u32, now: u64) -> AppResult<()> {
    let now = sqlite_epoch("now", now)?;
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE action_outbox SET status = ?2, attempt_count = ?3, last_error = NULL, \
             updated_at = ?4 WHERE id = ?1",
            rusqlite::params![
                id,
                status_as_wire(ActionStatus::Done),
                attempt_count as i64,
                now
            ],
        )
        .map(|_| ())
    })
}

/// Record a transient failure (AB#1066): bump `attempt_count`, reschedule `next_attempt_at`, store
/// `last_error`, stamp `updated_at`. Status stays `pending` (the worker retries it when due). A
/// no-op if the id doesn't exist.
pub fn mark_retry(
    db: &Database,
    id: i64,
    attempt_count: u32,
    next_attempt_at: u64,
    error: &str,
    now: u64,
) -> AppResult<()> {
    let next_attempt_at = sqlite_epoch("next_attempt_at", next_attempt_at)?;
    let now = sqlite_epoch("now", now)?;
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE action_outbox SET attempt_count = ?2, next_attempt_at = ?3, last_error = ?4, \
             updated_at = ?5 WHERE id = ?1",
            rusqlite::params![
                id,
                attempt_count as i64,
                next_attempt_at,
                clamp_error(error),
                now
            ],
        )
        .map(|_| ())
    })
}

/// Dead-letter an outbox row (AB#1066): flip to the terminal `dead`, record the final
/// `attempt_count` + `last_error`, stamp `updated_at`. A no-op if the id doesn't exist.
pub fn mark_dead(
    db: &Database,
    id: i64,
    attempt_count: u32,
    error: &str,
    now: u64,
) -> AppResult<()> {
    let now = sqlite_epoch("now", now)?;
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE action_outbox SET status = ?2, attempt_count = ?3, last_error = ?4, \
             updated_at = ?5 WHERE id = ?1",
            rusqlite::params![
                id,
                status_as_wire(ActionStatus::Dead),
                attempt_count as i64,
                clamp_error(error),
                now
            ],
        )
        .map(|_| ())
    })
}

pub(crate) fn mark_pending_by_dedupe_fragment_dead(
    db: &Database,
    dedupe_fragment: &str,
    error: &str,
    now: u64,
) -> AppResult<Vec<i64>> {
    if dedupe_fragment.is_empty() {
        return Ok(Vec::new());
    }
    let now = sqlite_epoch("now", now)?;
    let error = clamp_error(error);
    db.with_tx(|tx| {
        let ids = {
            let mut stmt = tx
                .prepare(
                    "SELECT id FROM action_outbox \
                     WHERE status = ?1 AND dedupe_key IS NOT NULL AND instr(dedupe_key, ?2) > 0 \
                     ORDER BY id",
                )
                .map_err(map_err)?;
            let ids = stmt
                .query_map(
                    rusqlite::params![status_as_wire(ActionStatus::Pending), dedupe_fragment],
                    |r| r.get::<_, i64>(0),
                )
                .map_err(map_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(map_err)?;
            ids
        };
        for id in &ids {
            tx.execute(
                "UPDATE action_outbox SET status = ?2, last_error = ?3, updated_at = ?4 \
                 WHERE id = ?1 AND status = ?5",
                rusqlite::params![
                    id,
                    status_as_wire(ActionStatus::Dead),
                    &error,
                    now,
                    status_as_wire(ActionStatus::Pending),
                ],
            )
            .map_err(map_err)?;
        }
        Ok(ids)
    })
}

/// The outcome of a [`reset_for_retry`] attempt (AB#1066): the row was re-queued, the id is unknown,
/// or the row exists but is NOT in a retryable terminal state. The command maps each to a distinct
/// response (a precise error for the latter two).
#[derive(Debug, PartialEq, Eq)]
pub enum RetryReset {
    /// A `dead` row was reset to `pending` for another run.
    Requeued,
    /// No row with this id exists.
    Unknown,
    /// The row exists but is not `dead` (only a dead-lettered action may be manually retried).
    NotDead,
}

/// Re-queue a DEAD-LETTERED row for another run (AB#1066) — the `outbox_retry` command's write.
/// Flips a `dead` row back to `pending` with a FRESH retry budget (`attempt_count = 0`) due now, so a
/// manual retry gets the full attempt allowance again. `last_error` is KEPT (the user still sees why
/// it last failed until the next attempt clears it on success).
///
/// **Resets `created_at = now` too (AB#1182).** A manual retry is an explicit "re-send it now", so it
/// gets a FRESH staleness window — otherwise a row dead-lettered BY the staleness sweep (its
/// `created_at` already past the TTL) would be re-claimed and immediately re-expired, a futile
/// retry → re-dead loop. Resetting `created_at` makes the re-queued action fresh, consistent with
/// the fresh retry budget.
///
/// **Guards to `status = 'dead'` (backend invariant, not UI-only).** Only a dead-lettered action may
/// be manually retried — re-queuing a `done` row would re-run an already-succeeded side effect, and a
/// `pending` row is already queued. The UI only renders the retry button on `dead` rows, but this SQL
/// guard makes the invariant hold at the command boundary regardless of caller. Distinguishes
/// [`RetryReset::Unknown`] (no such id) from [`RetryReset::NotDead`] (wrong state).
pub fn reset_for_retry(db: &Database, id: i64, now: u64) -> AppResult<RetryReset> {
    let now = sqlite_epoch("now", now)?;
    db.with_conn(|conn| {
        let updated = conn.execute(
            "UPDATE action_outbox SET status = ?2, attempt_count = 0, next_attempt_at = ?3, \
             created_at = ?3, updated_at = ?3 WHERE id = ?1 AND status = ?4",
            rusqlite::params![
                id,
                status_as_wire(ActionStatus::Pending),
                now,
                status_as_wire(ActionStatus::Dead)
            ],
        )?;
        if updated == 1 {
            return Ok(RetryReset::Requeued);
        }
        // No dead row updated — distinguish a missing id from a wrong-state (non-dead) row so the
        // command can return a precise error.
        let exists = conn
            .query_row(
                "SELECT 1 FROM action_outbox WHERE id = ?1",
                [id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(if exists {
            RetryReset::NotDead
        } else {
            RetryReset::Unknown
        })
    })
}

/// The outbox entries for a project (AB#1066), NEWEST FIRST (`ORDER BY id DESC`), bounded to
/// [`LIST_LIMIT`]. `project_id: None` lists ACROSS projects (the panel's "all" view); `Some(pid)`
/// scopes to one project (the `idx_action_outbox_project` index serves both).
pub fn list_by_project(db: &Database, project_id: Option<&str>) -> AppResult<Vec<OutboxEntry>> {
    db.with_conn(|conn| {
        let rows: Vec<OutboxEntry> = match project_id {
            Some(pid) => {
                let mut stmt = conn.prepare(
                    "SELECT id, project_id, kind, summary, status, attempt_count, next_attempt_at, \
                     last_error, created_at, updated_at FROM action_outbox \
                     WHERE project_id = ?1 ORDER BY id DESC LIMIT ?2",
                )?;
                // Bind to a local so `stmt` drops before the arm's value (the `MappedRows` borrows
                // it); returning the collect expression directly would outlive `stmt`.
                let rows = stmt
                    .query_map(rusqlite::params![pid, LIST_LIMIT], hydrate_entry)?
                    .collect::<rusqlite::Result<_>>()?;
                rows
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT id, project_id, kind, summary, status, attempt_count, next_attempt_at, \
                     last_error, created_at, updated_at FROM action_outbox \
                     ORDER BY id DESC LIMIT ?1",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![LIST_LIMIT], hydrate_entry)?
                    .collect::<rusqlite::Result<_>>()?;
                rows
            }
        };
        Ok(rows)
    })
}

/// One outbox entry by id (AB#1066), or `None` when unknown — the post-transition row the
/// `outbox:updated` emit carries.
pub fn get_entry(db: &Database, id: i64) -> AppResult<Option<OutboxEntry>> {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT id, project_id, kind, summary, status, attempt_count, next_attempt_at, \
             last_error, created_at, updated_at FROM action_outbox WHERE id = ?1",
            [id],
            hydrate_entry,
        )
        .optional()
    })
}

/// The verbatim serialized `payload` of an outbox entry (AB#1066), or `None` when the id is unknown
/// — the `outbox_get_raw` audit source.
pub fn get_raw(db: &Database, id: i64) -> AppResult<Option<String>> {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT payload FROM action_outbox WHERE id = ?1",
            [id],
            |r| r.get::<_, String>(0),
        )
        .optional()
    })
}

/// Map one queried row to an [`OutboxEntry`]. The `kind` / `status` columns degrade leniently so a
/// tampered row still LISTS rather than failing the whole page: `status_from_wire` defaults a
/// corrupt value to `Dead`, and `kind` falls back to `ActionKind::default()` (`Notification`) — a
/// diagnostic lie for an UNRECOGNIZED kind, but acceptable for a read-only list (the strict
/// `kind_from_wire` on the side-effectful [`claim_due`] path instead DEAD-LETTERS such a row, so it
/// surfaces as terminal `dead` and stops re-appearing). Every KNOWN kind
/// (`notification`/`review`/`check`/`stopReview`, AB#1069) hydrates correctly — only a genuinely
/// unrecognized string falls back to `Notification`, and the schema-version guard rules out a
/// legitimate future kind reaching an older binary. (`hydrate_entry_round_trips_all_known_kinds`
/// locks this so a new kind can't silently list as `Notification`.)
fn hydrate_entry(r: &rusqlite::Row) -> rusqlite::Result<OutboxEntry> {
    let kind_wire: String = r.get(2)?;
    let status_wire: String = r.get(4)?;
    let attempt_count: i64 = r.get(5)?;
    let next_attempt_at: i64 = r.get(6)?;
    let created_at: i64 = r.get(8)?;
    let updated_at: i64 = r.get(9)?;
    Ok(OutboxEntry {
        id: r.get(0)?,
        project_id: r.get(1)?,
        kind: kind_from_wire(&kind_wire).unwrap_or_default(),
        summary: r.get(3)?,
        status: status_from_wire(&status_wire),
        attempt_count: attempt_count.max(0) as u32,
        next_attempt_at: next_attempt_at.max(0) as u64,
        last_error: r.get(7)?,
        created_at: created_at.max(0) as u64,
        updated_at: updated_at.max(0) as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enqueue_notif(db: &Database, project_id: &str, summary: &str, now: u64) -> i64 {
        enqueue(
            db,
            project_id,
            ActionKind::Notification,
            summary,
            "{\"title\":\"x\"}",
            now,
        )
        .expect("enqueue")
    }

    // A fresh enqueue is a `pending` row due now, attempt_count 0, no error (AB#1066).
    #[test]
    fn enqueue_inserts_pending_due_now() {
        let db = Database::open_in_memory().expect("open db");
        let id = enqueue_notif(&db, "p1", "PR #7 review 完成", 1_000);

        let entry = get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(entry.status, ActionStatus::Pending);
        assert_eq!(entry.kind, ActionKind::Notification);
        assert_eq!(entry.summary, "PR #7 review 完成");
        assert_eq!(entry.attempt_count, 0);
        assert_eq!(entry.next_attempt_at, 1_000);
        assert_eq!(entry.created_at, 1_000);
        assert_eq!(entry.last_error, None);
    }

    #[test]
    fn enqueue_deduped_reuses_live_pending_row_only() {
        let db = Database::open_in_memory().expect("open db");
        let first = enqueue_deduped(
            &db,
            "p1",
            ActionKind::Review,
            "PR #7 review",
            "{}",
            "7@sha:review",
            100,
        )
        .expect("first enqueue");
        let second = enqueue_deduped(
            &db,
            "p1",
            ActionKind::Review,
            "PR #7 review",
            "{}",
            "7@sha:review",
            101,
        )
        .expect("second enqueue");
        assert_eq!(second, first, "same live pending action is reused");

        mark_done(&db, first, 1, 200).expect("done");
        let after_done = enqueue_deduped(
            &db,
            "p1",
            ActionKind::Review,
            "PR #7 review",
            "{}",
            "7@sha:review",
            201,
        )
        .expect("enqueue after done");
        assert_ne!(
            after_done, first,
            "terminal row no longer blocks a deliberate future enqueue"
        );
    }

    // `claim_due` returns ONLY pending rows whose next_attempt_at <= now, oldest first; it excludes
    // future-scheduled, done, and dead rows (AB#1066) — the worker's claim contract.
    #[test]
    fn claim_due_filters_pending_due_oldest_first() {
        let db = Database::open_in_memory().expect("open db");
        let a = enqueue_notif(&db, "p1", "a", 100); // due
        let b = enqueue_notif(&db, "p2", "b", 100); // due (newer id)
        let future = enqueue_notif(&db, "p1", "future", 100);
        mark_retry(&db, future, 1, 10_000, "later", 100).expect("reschedule future");
        let done = enqueue_notif(&db, "p1", "done", 100);
        mark_done(&db, done, 1, 100).expect("done");
        // A terminal `dead` row (due now) must also be excluded — the claim predicate is
        // `status = 'pending'`, so a dead-lettered row is never re-claimed.
        let dead = enqueue_notif(&db, "p1", "dead", 100);
        mark_dead(&db, dead, 5, "final boom", 100).expect("dead");

        let (claimed, quarantined) = claim_due(&db, 5_000).expect("claim");
        let ids: Vec<i64> = claimed.iter().map(|x| x.id).collect();
        assert_eq!(
            ids,
            vec![a, b],
            "only due pending rows, oldest id first (done/dead excluded)"
        );
        assert!(claimed.iter().all(|x| x.attempt_count == 0));
        // AB#1182: the claimed action carries `created_at` (the enqueue epoch), so the worker can
        // evaluate the staleness TTL. `a`/`b` were enqueued at 100 → that is their created_at.
        assert!(
            claimed.iter().all(|x| x.created_at == 100),
            "created_at hydrated from the enqueue epoch"
        );
        assert!(quarantined.is_empty(), "no corrupt rows here");

        // Advancing now past the future row's schedule makes it claimable too.
        let (later, _q) = claim_due(&db, 10_000).expect("claim later");
        assert!(later.iter().any(|x| x.id == future));
    }

    #[test]
    fn claim_due_respects_delayed_next_attempt_at() {
        let db = Database::open_in_memory().expect("open db");
        let now = 1_000;
        let id = db
            .with_tx(|tx| {
                enqueue_in_tx(
                    tx,
                    &EnqueueInput {
                        project_id: "p1",
                        kind: ActionKind::Notification,
                        summary: "delayed",
                        payload: "{}",
                        dedupe_key: Some("delayed-key"),
                        next_attempt_at: Some(now + 60),
                    },
                    now,
                )
            })
            .expect("enqueue delayed");

        let (early, _) = claim_due(&db, now + 59).expect("early claim");
        assert!(
            early.iter().all(|action| action.id != id),
            "delayed action must not be claimable before next_attempt_at"
        );

        let (due, _) = claim_due(&db, now + 60).expect("due claim");
        assert!(
            due.iter().any(|action| action.id == id),
            "delayed action becomes claimable at next_attempt_at"
        );
    }

    #[test]
    fn enqueue_rejects_timestamps_outside_sqlite_i64_range() {
        let db = Database::open_in_memory().expect("open db");
        let err = db
            .with_tx(|tx| {
                enqueue_in_tx(
                    tx,
                    &EnqueueInput {
                        project_id: "p1",
                        kind: ActionKind::Notification,
                        summary: "too far",
                        payload: "{}",
                        dedupe_key: Some("too-far"),
                        next_attempt_at: Some(i64::MAX as u64 + 1),
                    },
                    100,
                )
            })
            .expect_err("next_attempt_at beyond SQLite i64 must fail");
        assert!(
            err.message.contains("next_attempt_at"),
            "error should name the invalid timestamp field: {}",
            err.message
        );
    }

    #[test]
    fn mark_retry_rejects_timestamps_outside_sqlite_i64_range() {
        let db = Database::open_in_memory().expect("open db");
        let id = enqueue_notif(&db, "p1", "a", 100);
        let err = mark_retry(&db, id, 1, i64::MAX as u64 + 1, "later", 100)
            .expect_err("retry schedule beyond SQLite i64 must fail");
        assert!(
            err.message.contains("next_attempt_at"),
            "error should name the invalid timestamp field: {}",
            err.message
        );
    }

    // Status transitions (AB#1066): mark_done → done + clears error; mark_retry bumps attempt +
    // reschedules + keeps pending + stores error; mark_dead → terminal dead with the message.
    #[test]
    fn status_transitions() {
        let db = Database::open_in_memory().expect("open db");
        let id = enqueue_notif(&db, "p1", "a", 100);

        mark_retry(&db, id, 1, 130, "boom", 100).expect("retry");
        let r = get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(r.status, ActionStatus::Pending);
        assert_eq!(r.attempt_count, 1);
        assert_eq!(r.next_attempt_at, 130);
        assert_eq!(r.last_error.as_deref(), Some("boom"));

        mark_dead(&db, id, 5, "final boom", 200).expect("dead");
        let d = get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(d.status, ActionStatus::Dead);
        assert_eq!(d.attempt_count, 5);
        assert_eq!(d.last_error.as_deref(), Some("final boom"));

        mark_done(&db, id, 6, 300).expect("done");
        let done = get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(done.status, ActionStatus::Done);
        assert_eq!(
            done.attempt_count, 6,
            "mark_done records the succeeding attempt count"
        );
        assert_eq!(done.last_error, None, "mark_done clears the error");
    }

    // `reset_for_retry` re-queues a DEAD row with a fresh budget (AB#1066 F2): status → pending,
    // attempt_count → 0, due now → Requeued. An unknown id → Unknown; a non-dead row → NotDead
    // (the backend invariant: only a dead-lettered action may be manually retried — a `done` row
    // must NOT be re-runnable from the command boundary, regardless of UI).
    #[test]
    fn reset_for_retry_only_requeues_dead_rows() {
        let db = Database::open_in_memory().expect("open db");
        let id = enqueue_notif(&db, "p1", "a", 100);
        mark_dead(&db, id, 5, "final boom", 200).expect("dead");

        assert_eq!(
            reset_for_retry(&db, id, 500).expect("reset"),
            RetryReset::Requeued
        );
        let r = get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(r.status, ActionStatus::Pending);
        assert_eq!(r.attempt_count, 0, "fresh retry budget");
        assert_eq!(r.next_attempt_at, 500);
        // AB#1182: a manual retry resets `created_at` too (was 100 at enqueue) so the re-queued row
        // gets a FRESH staleness window — a row dead-lettered by the TTL sweep won't instantly
        // re-expire on retry.
        assert_eq!(r.created_at, 500, "retry resets the staleness window");
        let (due, _q) = claim_due(&db, 500).expect("claim");
        assert!(due.iter().any(|x| x.id == id));

        // Unknown id → Unknown.
        assert_eq!(
            reset_for_retry(&db, 99_999, 500).expect("unknown"),
            RetryReset::Unknown
        );

        // A `done` (non-dead) row must NOT be re-queued — guard rejects it as NotDead, leaving it done.
        let done_id = enqueue_notif(&db, "p1", "done", 100);
        mark_done(&db, done_id, 1, 100).expect("done");
        assert_eq!(
            reset_for_retry(&db, done_id, 600).expect("non-dead"),
            RetryReset::NotDead,
            "a done row is not retryable from the command boundary"
        );
        assert_eq!(
            get_entry(&db, done_id)
                .expect("get")
                .expect("exists")
                .status,
            ActionStatus::Done,
            "done row left untouched"
        );
    }

    // List scope + order (AB#1066): newest first (`id DESC`); a `project_id` filter scopes the
    // page; `None` lists across projects.
    #[test]
    fn list_by_project_scopes_and_orders_newest_first() {
        let db = Database::open_in_memory().expect("open db");
        let a = enqueue_notif(&db, "p1", "a", 100);
        let b = enqueue_notif(&db, "p2", "b", 100);
        let c = enqueue_notif(&db, "p1", "c", 100);

        let p1: Vec<i64> = list_by_project(&db, Some("p1"))
            .expect("list p1")
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(p1, vec![c, a], "p1 newest first");

        let all: Vec<i64> = list_by_project(&db, None)
            .expect("list all")
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(all, vec![c, b, a], "all projects, id desc");

        assert!(list_by_project(&db, Some("nope"))
            .expect("list nope")
            .is_empty());
    }

    // Raw payload round-trip (AB#1066): the verbatim payload reads back by id; an unknown id is None.
    #[test]
    fn get_raw_round_trips_and_unknown_is_none() {
        let db = Database::open_in_memory().expect("open db");
        let id =
            enqueue(&db, "p1", ActionKind::Notification, "s", "{\"a\":1}", 100).expect("enqueue");
        assert_eq!(get_raw(&db, id).expect("raw").as_deref(), Some("{\"a\":1}"));
        assert!(get_raw(&db, 99_999).expect("unknown").is_none());
    }

    // `ActionStatus` lock (AB#1066, Medium carrier): the DB-stored `status_as_wire` string MUST
    // equal the serde form the frontend mirrors, and `status_from_wire` round-trips it — so a drift
    // between the column value and the `src/types.ts` union (or between the two Rust sources) fails
    // here. An unknown stored value degrades to `Dead` (terminal).
    #[test]
    fn status_wire_matches_serde_and_round_trips() {
        for status in [
            ActionStatus::Pending,
            ActionStatus::Done,
            ActionStatus::Dead,
        ] {
            let serde_wire = serde_json::to_value(status).expect("serializes");
            assert_eq!(serde_wire, status_as_wire(status), "as_wire == serde form");
            assert_eq!(
                status_from_wire(status_as_wire(status)),
                status,
                "round-trips"
            );
        }
        assert_eq!(status_from_wire("???"), ActionStatus::Dead);
    }

    // `ActionKind` wire is strict on the side-effectful path (AB#1066/AB#1069): every known kind
    // round-trips (a `"review"` row is parsed + executed, NOT quarantined); an unknown value is an
    // explicit error (claim_due DEAD-LETTERS such a row rather than mis-routing). `"email"` stays
    // unknown by design — email/IM are NotificationKind channels, not ActionKinds (AB#1069).
    #[test]
    fn kind_wire_round_trips_known_and_errors_on_unknown() {
        for (kind, wire) in [
            (ActionKind::Notification, "notification"),
            (ActionKind::Review, "review"),
            (ActionKind::Check, "check"),
            (ActionKind::StopReview, "stopReview"),
            (ActionKind::MessagingReply, "messagingReply"),
            (ActionKind::MessagingSend, "messagingSend"),
        ] {
            assert_eq!(kind_as_wire(kind), wire, "{kind:?} → {wire}");
            assert_eq!(
                kind_from_wire(wire).expect("known kind parses (not quarantined)"),
                kind,
                "{wire} → {kind:?}"
            );
        }
        assert!(
            kind_from_wire("email").is_err(),
            "email is a NotificationKind channel, not an ActionKind — stays an explicit error"
        );
    }

    // The read-only LIST path (`hydrate_entry`) must surface every KNOWN kind correctly — the lenient
    // `kind_from_wire(...).unwrap_or_default()` fallback (→ Notification) is ONLY for an unrecognized
    // string, never for a legitimate AB#1069 kind. This locks that a `review`/`check`/`stopReview` row
    // does NOT silently list as `Notification` in the panel (the "diagnostic lie" must not bite a
    // real kind).
    #[test]
    fn hydrate_entry_round_trips_all_known_kinds() {
        let db = Database::open_in_memory().expect("open db");
        for (i, kind) in [
            ActionKind::Notification,
            ActionKind::Review,
            ActionKind::Check,
            ActionKind::StopReview,
            ActionKind::MessagingReply,
            ActionKind::MessagingSend,
        ]
        .into_iter()
        .enumerate()
        {
            let id = enqueue(&db, "p1", kind, "s", "{}", 100 + i as u64).expect("enqueue");
            let entry = get_entry(&db, id).expect("get").expect("exists");
            assert_eq!(
                entry.kind, kind,
                "row {id} hydrates to {kind:?}, not a fallback"
            );
        }
    }

    // claim_due QUARANTINES (dead-letters) a corrupt-kind row (AB#1066): a tampered `kind` the
    // worker can't route must not crash the batch NOR linger `pending` forever — it is flipped to
    // terminal `dead` (panel-visible, no longer re-selected), while a good row alongside still
    // claims. A second claim returns only the good row (the dead row is excluded), proving the
    // corrupt row stops re-appearing.
    #[test]
    fn claim_due_dead_letters_corrupt_kind_row() {
        let db = Database::open_in_memory().expect("open db");
        let good = enqueue_notif(&db, "p1", "good", 100);
        let corrupt = db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO action_outbox \
                     (project_id, kind, summary, payload, status, attempt_count, next_attempt_at, \
                      created_at, updated_at) \
                     VALUES ('p1', 'gitlab-bot', 's', '{}', 'pending', 0, 100, 100, 100)",
                    [],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .expect("insert corrupt-kind row");

        let (claimed, quarantined) = claim_due(&db, 500).expect("claim does not crash");
        let ids: Vec<i64> = claimed.iter().map(|x| x.id).collect();
        assert_eq!(
            ids,
            vec![good],
            "corrupt-kind row excluded, good row claimed"
        );
        // The corrupt row's id is returned as quarantined so the service emits its `dead` transition.
        assert_eq!(
            quarantined,
            vec![corrupt],
            "corrupt-kind id reported as quarantined"
        );

        // The corrupt row was dead-lettered (terminal), carrying the parse error, and is not
        // re-claimed on the next cycle.
        let entry = get_entry(&db, corrupt).expect("get").expect("exists");
        assert_eq!(
            entry.status,
            ActionStatus::Dead,
            "corrupt-kind row dead-lettered"
        );
        assert!(
            entry.last_error.is_some(),
            "carries the unrecognized-kind error"
        );
        let (reclaimed, requarantined) = claim_due(&db, 500).expect("re-claim");
        assert!(
            reclaimed.iter().all(|x| x.id != corrupt) && requarantined.is_empty(),
            "dead-lettered corrupt row is neither re-claimed nor re-quarantined"
        );
    }

    // `last_error` is clamped on write (AB#1066, security review): a short message is stored
    // verbatim; an over-budget one is truncated on a char boundary with an ellipsis, so a giant /
    // crafted error can't bloat the panel-visible column. Multi-byte input must not split a char.
    #[test]
    fn last_error_is_clamped_on_write() {
        let db = Database::open_in_memory().expect("open db");
        let id = enqueue_notif(&db, "p1", "a", 100);

        let short = "boom";
        mark_dead(&db, id, 1, short, 100).expect("dead short");
        assert_eq!(
            get_entry(&db, id)
                .expect("get")
                .expect("exists")
                .last_error
                .as_deref(),
            Some(short),
            "short error stored verbatim"
        );

        // A multi-byte string longer than the cap truncates on a char boundary (never panics).
        let huge = "字".repeat(MAX_LAST_ERROR_LEN); // 3 bytes each → well over the byte budget
        mark_retry(&db, id, 2, 200, &huge, 100).expect("retry huge");
        let stored = get_entry(&db, id)
            .expect("get")
            .expect("exists")
            .last_error
            .expect("has error");
        assert!(
            stored.len() <= MAX_LAST_ERROR_LEN + 4,
            "clamped to the byte budget (+ellipsis)"
        );
        assert!(stored.ends_with('…'), "truncation marker appended");
    }

    // Retention cap prunes oldest TERMINAL rows but NEVER a pending one (AB#1066): a pending row is
    // an un-run action. Seed cap+overflow terminal rows + one pending, enqueue one more to trigger
    // the prune, assert the count holds and the pending row survives.
    #[test]
    fn enqueue_prunes_oldest_terminal_but_keeps_pending() {
        let db = Database::open_in_memory().expect("open db");
        // A pending row with the LOWEST id (oldest) — must survive the prune.
        let pending_old = enqueue_notif(&db, "p1", "pending-old", 50);
        // Seed exactly MAX_OUTBOX_TERMINAL terminal (done) rows directly (cheap raw inserts).
        db.with_conn(|conn| {
            for i in 0..MAX_OUTBOX_TERMINAL {
                conn.execute(
                    "INSERT INTO action_outbox \
                     (project_id, kind, summary, payload, status, attempt_count, next_attempt_at, \
                      created_at, updated_at) \
                     VALUES ('p1', 'notification', ?1, '{}', 'done', 0, 100, 100, 100)",
                    rusqlite::params![format!("seed-{i}")],
                )?;
            }
            Ok(())
        })
        .expect("seed terminal rows");

        // One more enqueue triggers the terminal-row prune (cap + 1 → cap, among terminal rows).
        enqueue_notif(&db, "p1", "newest", 100);

        let terminal: i64 = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM action_outbox WHERE status IN ('done','dead')",
                    [],
                    |r| r.get(0),
                )
            })
            .expect("count terminal");
        assert_eq!(terminal, MAX_OUTBOX_TERMINAL, "terminal rows capped");
        // The pending row (oldest id of all) was NOT pruned — only terminal rows are.
        assert!(
            get_entry(&db, pending_old).expect("get").is_some(),
            "a pending row is never pruned by the cap"
        );
    }
}
