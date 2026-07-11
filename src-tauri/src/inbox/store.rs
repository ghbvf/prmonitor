//! `inbox_event` persistence (AB#1065): the slice-local queries over the inbox table.
//!
//! Slice-local — the inbox slice owns these queries, reaching SQLite only through the
//! horizontal [`crate::db::Database`] handle (not a cross-slice import), exactly as
//! `review::history_store` owns the review tables.
//!
//! The table's `UNIQUE(dedupe_key)` is the inbox's **Hard** ingress-idempotency carrier (see
//! [`insert_dedup`]); [`crate::model::InboxStatus`]'s exhaustive `match` in [`status_as_wire`]
//! is the **Hard** carrier for the status ↔ column-string mapping (a new variant is a compile
//! error here, like `pr::webhook::StatusOnlyKind`).

use rusqlite::{OptionalExtension, Transaction};

use crate::db::{map_err, Database};
use crate::error::AppResult;
use crate::model::{
    Candidate, EventEnvelope, EventPayload, InboxEntry, InboxStatus, ReviewReceiptId,
    ReviewReceiptSnapshot, ReviewReceiptStatus, SourceKind,
};

/// Bound on a single [`list_by_project`] page (AB#1065): the panel only ever needs a recent
/// window, and the `idx_inbox_event_project` index makes the `ORDER BY id DESC LIMIT` a cheap
/// top-N read. Bounds the rows hydrated into memory regardless of how large the table grows.
const LIST_LIMIT: i64 = 500;

/// Global retention cap on persisted inbox rows (AB#1065). Every webhook delivery inserts a row
/// and nothing else deletes them, so without a cap the `inbox_event` table grows unbounded.
/// [`insert_dedup`] prunes the oldest rows beyond this cap on each new insert (mirroring
/// `review::history_store`'s per-PR `MAX_SESSIONS_PER_PR` backstop). A GLOBAL cap (not per-project)
/// since the inbox is a single audit log across projects; 5000 keeps a long recent window while
/// bounding disk/scan cost.
const MAX_INBOX_EVENTS: i64 = 5000;

/// Wall-clock seconds for the inbox timestamps. Slice-local (the `pr` slice and the `review`
/// slice each keep their own `now_epoch`; keeping one here avoids a cross-slice import — the
/// inbox stays self-contained). Degrades to 0 on a pre-epoch clock rather than panicking.
pub(crate) fn now_epoch() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// [`InboxStatus`] → its pinned wire string (the form stored in the `status` column).
///
/// The **Hard** carrier (exhaustive `match` over the sealed enum) for the status ↔ column
/// mapping: adding a variant without an arm is a compile error — the missing case cannot be
/// expressed. The strings are the SAME serde wire values the frontend mirrors (locked equal to
/// the serde form by `inbox_status_wire_matches_serde_and_round_trips`), so the DB column and
/// the `src/types.ts` union can't drift apart.
pub(crate) fn status_as_wire(status: InboxStatus) -> &'static str {
    match status {
        InboxStatus::Received => "received",
        InboxStatus::Processed => "processed",
        InboxStatus::Failed => "failed",
    }
}

/// Wire string → [`InboxStatus`] (reverse of [`status_as_wire`]). An unknown / corrupt stored
/// value degrades to [`InboxStatus::Failed`] (a terminal, replayable state — never resurrects a
/// corrupt row as `Received`/`Processed`), mirroring `history_store::status_from_wire`'s lenient
/// default.
pub(crate) fn status_from_wire(s: &str) -> InboxStatus {
    match s {
        "received" => InboxStatus::Received,
        "processed" => InboxStatus::Processed,
        _ => InboxStatus::Failed,
    }
}

/// [`SourceKind`] → its pinned DB wire string (== the serde form). A unit enum with
/// `rename_all = "camelCase"`, so serialization CANNOT fail — `expect` fail-fast surfaces a
/// serde regression loudly rather than silently storing an empty source.
fn source_as_wire(source: SourceKind) -> String {
    serde_json::to_value(source)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("SourceKind serializes to a JSON string (unit enum, known variants)")
}

/// Insert one normalized [`Event`] + its raw payload + (optional) parsed `WebhookEvent` JSON,
/// deduping on [`Event::dedupe_key`] (AB#1065). Returns `Some(new_id)` when a NEW row was
/// inserted, `None` when the delivery was a duplicate (the same `dedupe_key` already exists).
///
/// **Hard ingress-idempotency carrier.** The `INSERT … ON CONFLICT(dedupe_key) DO NOTHING`
/// relies on the table's `UNIQUE(dedupe_key)` (see `SCHEMA_V4`): a re-delivered webhook can't
/// produce a second row. `conn.changes() == 1` distinguishes a real insert (process it) from a
/// no-op conflict (a duplicate — do NOT re-process). The new status is always `Received`.
///
/// On a NEW insert this also prunes rows beyond [`MAX_INBOX_EVENTS`] (oldest by `id` first) so the
/// table stays bounded. Insert + prune run in ONE `with_tx` (mirroring
/// `review::history_store::upsert_session`'s insert+prune) so a prune failure can't leave the new
/// row with a half-applied prune, and both commit/roll back atomically.
pub fn insert_dedup(
    db: &Database,
    event: &EventEnvelope,
    raw: &str,
    webhook_event_json: Option<&str>,
    candidate: Option<&Candidate>,
) -> AppResult<Option<i64>> {
    let event_json = serde_json::to_string(event)
        .map_err(|e| crate::error::AppError::new(format!("inbox 事件序列化失败: {e}")))?;
    let candidate_json = candidate
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| crate::error::AppError::new(format!("inbox candidate 序列化失败: {e}")))?;
    db.with_tx(|tx| {
        tx.execute(
            "INSERT INTO inbox_event \
             (dedupe_key, source, event_type, project_id, repo, number, event_json, \
              raw_payload, webhook_event_json, candidate_json, status, received_at_epoch) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
             ON CONFLICT(dedupe_key) DO NOTHING",
            rusqlite::params![
                event.dedupe_key().as_str(),
                source_as_wire(event.source()),
                // The event-class column mirrors the normalized `Event`'s serde wire form, kept
                // in lockstep with `event_json`'s `eventType` (both come from the same `Event`).
                event_type_as_wire(event),
                event.project_id(),
                event.repo(),
                event
                    .as_observation()
                    .and_then(|o| o.subject.number)
                    .or_else(|| event.as_review_request().map(|r| r.pr_number))
                    .map(|n| n as i64),
                event_json,
                raw,
                webhook_event_json,
                candidate_json.as_deref(),
                status_as_wire(InboxStatus::Received),
                event.received_at_epoch() as i64,
            ],
        )
        .map_err(map_err)?;
        // A conflict (duplicate `dedupe_key`) is a no-op → `changes() == 0` → `None`. A real
        // insert → `changes() == 1` → the new rowid.
        if tx.changes() != 1 {
            return Ok(None);
        }
        let id = tx.last_insert_rowid();
        // Cap the table after a real insert: drop the oldest rows beyond MAX_INBOX_EVENTS (the
        // `LIMIT -1 OFFSET ?` no-ops cheaply when the table is under the cap).
        tx.execute(
            "DELETE FROM inbox_event WHERE id IN ( \
                 SELECT id FROM inbox_event \
                 WHERE status IN ('processed','failed') \
                   AND COALESCE(json_extract(event_json, '$.payload.kind'), 'observation') <> 'reviewRequest' \
                 ORDER BY id ASC LIMIT max((SELECT COUNT(*) FROM inbox_event) - ?1, 0))",
            rusqlite::params![MAX_INBOX_EVENTS],
        )
        .map_err(map_err)?;
        Ok(Some(id))
    })
}

/// Whether live rows forced the audit table above its history target. Live work is never pruned.
pub fn retention_pressure(db: &Database) -> AppResult<bool> {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT COUNT(*) > ?1 FROM inbox_event",
            [MAX_INBOX_EVENTS],
            |row| row.get(0),
        )
    })
}

/// Oldest durable work awaiting the single inbox consumer.
pub fn received_ids(db: &Database) -> AppResult<Vec<i64>> {
    db.with_conn(|conn| {
        let mut stmt = conn
            .prepare("SELECT id FROM inbox_event WHERE status='received' ORDER BY id LIMIT 100")?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    })
}

/// Replay is the only legal transition back to live work.
pub fn requeue_failed(db: &Database, id: i64) -> AppResult<bool> {
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE inbox_event SET status='received', processed_at_epoch=NULL, error=NULL \
             WHERE id=?1 AND status='failed'",
            [id],
        )
        .map(|changed| changed == 1)
    })
}

/// The inbox entries for a project (AB#1065), NEWEST FIRST (`ORDER BY id DESC`), bounded to
/// [`LIST_LIMIT`]. `project_id: None` lists ACROSS projects (the panel's "all projects" view);
/// `Some(pid)` scopes to one project (the `idx_inbox_event_project` index). Each row hydrates its
/// [`Event`] back from `event_json`; a corrupt `event_json` row is SKIPPED (best-effort — one
/// unreadable row must not blank the whole list), not surfaced as an error.
pub fn list_by_project(db: &Database, project_id: Option<&str>) -> AppResult<Vec<InboxEntry>> {
    // Map one queried row to the pre-hydrate `RawRow` (shared by both query shapes below).
    fn read_row(r: &rusqlite::Row) -> rusqlite::Result<RawRow> {
        Ok(RawRow {
            id: r.get(0)?,
            event_json: r.get(1)?,
            status: r.get(2)?,
            processed_at_epoch: r.get::<_, Option<i64>>(3)?,
            error: r.get(4)?,
        })
    }
    db.with_conn(|conn| {
        // Two query shapes (scoped vs all-projects) so the parameter sets are statically typed,
        // avoiding a `Vec<&dyn ToSql>` whose borrows of a matched `Some(..)` payload don't
        // outlive it. `LIMIT` bounds the page either way; `idx_inbox_event_project` serves both.
        let raw_rows: Vec<RawRow> = match project_id {
            Some(pid) => {
                let mut stmt = conn.prepare(
                    "SELECT id, event_json, status, processed_at_epoch, error FROM inbox_event \
                     WHERE project_id = ?1 ORDER BY id DESC LIMIT ?2",
                )?;
                // Bind the collected rows to a local so `stmt` drops before the arm's value.
                let rows = stmt
                    .query_map(rusqlite::params![pid, LIST_LIMIT], read_row)?
                    .collect::<rusqlite::Result<_>>()?;
                rows
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT id, event_json, status, processed_at_epoch, error FROM inbox_event \
                     ORDER BY id DESC LIMIT ?1",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![LIST_LIMIT], read_row)?
                    .collect::<rusqlite::Result<_>>()?;
                rows
            }
        };
        let mut out = Vec::with_capacity(raw_rows.len());
        for row in raw_rows {
            // Best-effort hydrate: skip a row whose `event_json` no longer parses rather than
            // failing the whole list (a forward-incompatible / corrupt blob).
            if let Ok(event) = serde_json::from_str::<EventEnvelope>(&row.event_json) {
                out.push(InboxEntry {
                    id: row.id,
                    event,
                    status: status_from_wire(&row.status),
                    processed_at_epoch: row.processed_at_epoch.map(|n| n as u64),
                    error: row.error,
                });
            }
        }
        Ok(out)
    })
}

/// One inbox entry by id (AB#1065), or `None` when unknown — the post-transition row the
/// `inbox:updated` emit carries. Hydrates the [`Event`] from `event_json`; a corrupt blob is an
/// error here (unlike the list's skip) because the caller asked for THIS specific row.
pub fn get_entry(db: &Database, id: i64) -> AppResult<Option<InboxEntry>> {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT id, event_json, status, processed_at_epoch, error FROM inbox_event \
             WHERE id = ?1",
            [id],
            |r| {
                Ok(RawRow {
                    id: r.get(0)?,
                    event_json: r.get(1)?,
                    status: r.get(2)?,
                    processed_at_epoch: r.get::<_, Option<i64>>(3)?,
                    error: r.get(4)?,
                })
            },
        )
        .optional()
    })
    .and_then(|maybe| match maybe {
        None => Ok(None),
        Some(row) => {
            let event: EventEnvelope = serde_json::from_str(&row.event_json).map_err(|e| {
                crate::error::AppError::new(format!("inbox 行 {id} 的事件反序列化失败: {e}"))
            })?;
            Ok(Some(InboxEntry {
                id: row.id,
                event,
                status: status_from_wire(&row.status),
                processed_at_epoch: row.processed_at_epoch.map(|n| n as u64),
                error: row.error,
            }))
        }
    })
}

/// The verbatim raw payload of an inbox entry (AB#1065), or `None` when the id is unknown —
/// the `inbox_get_raw` audit source.
pub fn get_raw(db: &Database, id: i64) -> AppResult<Option<String>> {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT raw_payload FROM inbox_event WHERE id = ?1",
            [id],
            |r| r.get::<_, String>(0),
        )
        .optional()
    })
}

pub fn id_by_dedupe_key(db: &Database, key: &str) -> AppResult<Option<i64>> {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT id FROM inbox_event WHERE dedupe_key=?1",
            [key],
            |row| row.get(0),
        )
        .optional()
    })
}

pub fn get_review_receipt(
    db: &Database,
    receipt_id: ReviewReceiptId,
) -> AppResult<ReviewReceiptSnapshot> {
    let row = db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT i.event_json, i.status, i.error, o.status, o.last_error, o.review_thread_id, \
                    s.status, s.comment_url, s.terminal_outcome, s.terminal_error \
             FROM inbox_event i \
             LEFT JOIN rule_match rm ON rm.inbox_event_id=i.id \
             LEFT JOIN rule_match_action rma ON rma.rule_match_id=rm.id \
             LEFT JOIN action_outbox o ON o.id=rma.action_outbox_id AND o.kind IN ('review','check') \
             LEFT JOIN review_session s ON s.thread_id=o.review_thread_id \
             WHERE i.id=?1 ORDER BY o.id LIMIT 1",
                [receipt_id.get()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, Option<String>>(9)?,
                    ))
                },
            )
            .optional()
        })?
        .ok_or_else(|| {
            crate::error::AppError::new(format!("review receipt 不存在：{}", receipt_id.get()))
        })?;

    let (
        event_json,
        inbox_status,
        inbox_error,
        action_status,
        action_error,
        thread_id,
        session_status,
        comment_url,
        terminal_outcome,
        terminal_error,
    ) = row;
    let event: EventEnvelope = serde_json::from_str(&event_json).map_err(|error| {
        crate::error::AppError::new(format!(
            "review receipt {} 的事件损坏: {error}",
            receipt_id.get()
        ))
    })?;
    if event.as_review_request().is_none() {
        return Err(crate::error::AppError::new(format!(
            "review receipt 不存在：{}",
            receipt_id.get()
        )));
    }
    let status = if inbox_status == "failed" {
        ReviewReceiptStatus::Failed
    } else {
        match action_status.as_deref() {
            None => ReviewReceiptStatus::Received,
            Some("pending") => ReviewReceiptStatus::Queued,
            Some("blocked") => ReviewReceiptStatus::Blocked,
            Some("dead") => ReviewReceiptStatus::Failed,
            Some("done") => match session_status.as_deref() {
                Some("starting") => ReviewReceiptStatus::Starting,
                Some("running") => ReviewReceiptStatus::Running,
                Some("interrupting") => ReviewReceiptStatus::Interrupting,
                Some("failed") => ReviewReceiptStatus::Failed,
                Some("done") | None => ReviewReceiptStatus::Done,
                Some(_) => ReviewReceiptStatus::Failed,
            },
            Some(_) => ReviewReceiptStatus::Failed,
        }
    };
    Ok(ReviewReceiptSnapshot {
        receipt_id,
        status,
        thread_id,
        comment_url,
        outcome: terminal_outcome,
        error: terminal_error.or(inbox_error).or(action_error),
    })
}

/// The stored facts a replay needs for one inbox entry (AB#1065): the normalized [`Event`], the
/// [`SourceKind`] (which decides the replay path — GitHub re-feed vs Azure refresh), the current
/// [`InboxStatus`], and the parsed `WebhookEvent` JSON (`Some` for a GitHub entry, `None` for an
/// Azure audit row). A named struct rather than a wide tuple (clippy::type_complexity).
#[derive(Debug)]
pub struct ProcessableInboxRow {
    pub event: EventEnvelope,
    pub source: SourceKind,
    pub status: InboxStatus,
    pub webhook_event_json: Option<String>,
    pub candidate: Option<Candidate>,
}

/// The [`ProcessableInboxRow`] facts of an inbox entry by id (AB#1065), or `None` when unknown. A corrupt
/// `event_json` is an error (the caller asked for THIS row).
pub fn get_replayable(db: &Database, id: i64) -> AppResult<Option<ProcessableInboxRow>> {
    let row = db.with_conn(|conn| {
        conn.query_row(
            "SELECT event_json, source, status, webhook_event_json, candidate_json FROM inbox_event WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()
    })?;
    match row {
        None => Ok(None),
        Some((event_json, source, status, webhook_event_json, candidate_json)) => {
            let event: EventEnvelope = serde_json::from_str(&event_json).map_err(|e| {
                crate::error::AppError::new(format!("inbox 行 {id} 的事件反序列化失败: {e}"))
            })?;
            let candidate = candidate_json
                .map(|json| {
                    serde_json::from_str::<Candidate>(&json).map_err(|e| {
                        crate::error::AppError::new(format!(
                            "inbox 行 {id} 的 candidate 反序列化失败: {e}"
                        ))
                    })
                })
                .transpose()?;
            Ok(Some(ProcessableInboxRow {
                event,
                source: source_from_wire(&source)?,
                status: status_from_wire(&status),
                webhook_event_json,
                candidate,
            }))
        }
    }
}

/// Mark an inbox entry `Processed` (AB#1065), stamping `processed_at_epoch` and clearing any
/// prior `error`. A no-op if the id doesn't exist.
pub fn mark_processed(db: &Database, id: i64) -> AppResult<()> {
    let now = now_epoch() as i64;
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE inbox_event SET status = ?2, processed_at_epoch = ?3, error = NULL \
             WHERE id = ?1",
            rusqlite::params![id, status_as_wire(InboxStatus::Processed), now],
        )
        .map(|_| ())
    })
}

pub(crate) fn mark_processed_in_tx(tx: &Transaction<'_>, id: i64, now: u64) -> AppResult<()> {
    let changed = tx
        .execute(
            "UPDATE inbox_event SET status=?2, processed_at_epoch=?3, error=NULL \
         WHERE id=?1 AND status='received'",
            rusqlite::params![id, status_as_wire(InboxStatus::Processed), now as i64],
        )
        .map_err(map_err)?;
    if changed != 1 {
        return Err(crate::error::AppError::new(format!(
            "inbox 行 {id} 非 Received，拒绝提交规则动作"
        )));
    }
    Ok(())
}

/// Mark an inbox entry `Failed` (AB#1065) with an error message, stamping `processed_at_epoch`
/// (the attempt's finish time). A no-op if the id doesn't exist.
pub fn mark_failed(db: &Database, id: i64, error: &str) -> AppResult<()> {
    let now = now_epoch() as i64;
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE inbox_event SET status = ?2, processed_at_epoch = ?3, error = ?4 \
             WHERE id = ?1",
            rusqlite::params![id, status_as_wire(InboxStatus::Failed), now, error],
        )
        .map(|_| ())
    })
}

/// A raw `inbox_event` row before its `Event` is hydrated — the shared shape of the list / get
/// reads, kept here so the row→`InboxEntry` mapping lives in one place.
struct RawRow {
    id: i64,
    event_json: String,
    status: String,
    processed_at_epoch: Option<i64>,
    error: Option<String>,
}

/// [`SourceKind`] from its DB wire string (reverse of [`source_as_wire`]), STRICT. Replay is a
/// side-effectful path (it re-feeds GitHub through `ingest_webhook` / re-invokes the Azure
/// refresher), so it must NOT silently default an unparseable stored `source` to a concrete kind
/// (the old lenient `unwrap_or(Github)` would route a corrupt Azure/Bitbucket row down the GitHub
/// re-feed). An unknown / corrupt value is an explicit [`AppError`] the replay surfaces and
/// records as `Failed`. (Listing stays lenient by NOT reading `source` — `list_by_project` never
/// calls this, so a corrupt source row still lists fine.)
fn source_from_wire(s: &str) -> AppResult<SourceKind> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| crate::error::AppError::new(format!("inbox 行的 source 无法识别：{s:?}")))
}

/// The event-class column value for an [`Event`]: the same serde wire string the frontend
/// mirrors (kept in lockstep with the `eventType` inside `event_json`). A unit enum, so
/// serialization cannot fail — `expect` surfaces a serde regression loudly.
fn event_type_as_wire(event: &EventEnvelope) -> String {
    let value = match event.payload() {
        EventPayload::Observation { .. } => {
            event
                .as_observation()
                .expect("observation payload")
                .event_type
        }
        EventPayload::ReviewRequest { .. } => crate::model::EventType::PullRequest,
    };
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("EventType serializes to a JSON string (unit enum, known variants)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EventSubject, EventType, InboxDedupeKey};

    fn event(dedupe_key: &str, project_id: &str, number: Option<u64>) -> EventEnvelope {
        EventEnvelope::observation(
            InboxDedupeKey::new(dedupe_key).unwrap(),
            SourceKind::Github,
            project_id,
            "owner/repo",
            EventType::PullRequest,
            EventSubject {
                number,
                title: "Add feature".to_string(),
                body: String::new(),
                labels: vec!["pr-review".to_string()],
                url: "https://example.com/pr/7".to_string(),
            },
            1_700_000_000,
        )
        .unwrap()
    }

    fn candidate(number: u64, kind: &str) -> Candidate {
        Candidate {
            number,
            head_sha: "sha".to_string(),
            head_ref: "main".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            kind: kind.parse().unwrap(),
        }
    }

    // Hard ingress-idempotency carrier (AB#1065): the FIRST insert of a `dedupe_key` returns
    // `Some(id)` (process it); a SECOND insert of the SAME key returns `None` (a duplicate — do
    // NOT re-process) and does not add a row. This is what makes a webhook retry exactly-once.
    #[test]
    fn insert_dedup_returns_new_then_none_for_duplicate() {
        let db = Database::open_in_memory().expect("open db");
        let ev = event("github:abc-123", "p1", Some(7));

        let first = insert_dedup(&db, &ev, "{\"raw\":1}", None, None).expect("first insert");
        assert!(first.is_some(), "first delivery inserts a new row");

        let second = insert_dedup(&db, &ev, "{\"raw\":1}", None, None).expect("second insert");
        assert!(second.is_none(), "duplicate dedupe_key is a no-op (None)");

        // Exactly one row exists for the key.
        let count: i64 = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM inbox_event WHERE dedupe_key = 'github:abc-123'",
                    [],
                    |r| r.get(0),
                )
            })
            .expect("count");
        assert_eq!(count, 1, "duplicate did not add a second row");
    }

    // List scope + order (AB#1065): newest first (`id DESC`), and a `project_id` filter scopes
    // the page; `None` lists across projects.
    #[test]
    fn list_by_project_scopes_and_orders_newest_first() {
        let db = Database::open_in_memory().expect("open db");
        insert_dedup(&db, &event("k1", "p1", Some(1)), "r1", None, None).expect("k1");
        insert_dedup(&db, &event("k2", "p2", Some(2)), "r2", None, None).expect("k2");
        insert_dedup(&db, &event("k3", "p1", Some(3)), "r3", None, None).expect("k3");

        // Scoped to p1: k3 (newest) then k1.
        let p1 = list_by_project(&db, Some("p1")).expect("list p1");
        let p1_keys: Vec<String> = p1
            .iter()
            .map(|e| e.event.dedupe_key().to_string())
            .collect();
        assert_eq!(p1_keys, vec!["k3".to_string(), "k1".to_string()]);

        // Across projects: k3, k2, k1 (id DESC).
        let all = list_by_project(&db, None).expect("list all");
        let all_keys: Vec<String> = all
            .iter()
            .map(|e| e.event.dedupe_key().to_string())
            .collect();
        assert_eq!(
            all_keys,
            vec!["k3".to_string(), "k2".to_string(), "k1".to_string()]
        );

        // A scope with no rows is empty, not an error.
        assert!(list_by_project(&db, Some("nope"))
            .expect("list nope")
            .is_empty());
    }

    // Raw round-trip (AB#1065): the verbatim payload reads back by id, and an unknown id is
    // `None` (the `inbox_get_raw` "err if unknown" decision is the command's, not the store's).
    #[test]
    fn get_raw_round_trips_and_unknown_is_none() {
        let db = Database::open_in_memory().expect("open db");
        let raw = "{\"delivery\":\"verbatim body\"}";
        let id = insert_dedup(&db, &event("k1", "p1", Some(1)), raw, None, None)
            .expect("insert")
            .expect("new id");

        assert_eq!(get_raw(&db, id).expect("get raw").as_deref(), Some(raw));
        assert!(get_raw(&db, 99_999).expect("unknown").is_none());
    }

    #[test]
    fn observation_inbox_id_is_not_a_review_receipt() {
        let db = Database::open_in_memory().expect("open db");
        let id = insert_dedup(&db, &event("observation", "p1", Some(7)), "raw", None, None)
            .expect("insert")
            .expect("new id");
        let receipt = ReviewReceiptId::new(id).expect("positive id");
        let error =
            get_review_receipt(&db, receipt).expect_err("observation must not be enumerable");
        assert!(error.message.contains("不存在"));
    }

    #[test]
    fn review_receipt_projects_durable_terminal_outcome_and_engine_error() {
        let db = Database::open_in_memory().expect("open db");
        let request_id =
            crate::model::ExternalRequestId::parse("0123456789abcdef0123456789abcdef").unwrap();
        let event = EventEnvelope::review_request(
            crate::model::InboxDedupeKey::new(format!("external:{request_id}")).unwrap(),
            SourceKind::Github,
            "p1",
            "owner/repo",
            7,
            crate::model::ReviewKind::Review,
            request_id,
            crate::model::ExternalTriggerOrigin::Http,
            false,
            1,
        )
        .unwrap();
        let inbox_id = insert_dedup(&db, &event, "external", None, None)
            .expect("insert")
            .expect("new");
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO review_session (thread_id, project_id, pr_number, turn_id, kind, status, created_at, updated_at, comment_url, engine_kind, terminal_outcome, terminal_error) VALUES ('th-1','p1',7,'turn-1','review','failed',1,2,NULL,'codex','interrupted','engine stopped')",
                [],
            )?;
            conn.execute(
                "INSERT INTO rule_match (rule_id, rule_name, inbox_event_id, project_id, action_count, created_at) VALUES ('system','external',?1,'p1',1,1)",
                [inbox_id],
            )?;
            conn.execute(
                "INSERT INTO action_outbox (project_id, kind, summary, payload, status, attempt_count, next_attempt_at, created_at, updated_at, producer_key, review_thread_id) VALUES ('p1','review','review','{}','done',1,1,1,2,'receipt-test','th-1')",
                [],
            )?;
            let action_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO rule_match_action (rule_match_id, action_outbox_id) VALUES ((SELECT id FROM rule_match WHERE inbox_event_id=?1), ?2)",
                rusqlite::params![inbox_id, action_id],
            )?;
            Ok(())
        })
        .expect("session, action, and match");

        let receipt =
            get_review_receipt(&db, ReviewReceiptId::new(inbox_id).unwrap()).expect("receipt");
        assert_eq!(receipt.outcome.as_deref(), Some("interrupted"));
        assert_eq!(receipt.error.as_deref(), Some("engine stopped"));
    }

    // Event round-trips through `event_json` (AB#1065): the hydrated `Event` matches what went
    // in, and an absent `number` (a generic event) round-trips as `None`.
    #[test]
    fn event_json_round_trips_through_list_and_get_entry() {
        let db = Database::open_in_memory().expect("open db");
        let ev = event("k1", "p1", None); // no number
        let id = insert_dedup(&db, &ev, "raw", None, None)
            .expect("insert")
            .expect("new id");

        let entry = get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(entry.event.dedupe_key().as_str(), "k1");
        assert_eq!(entry.event.as_observation().unwrap().subject.number, None);
        assert_eq!(
            entry.event.as_observation().unwrap().subject.labels,
            vec!["pr-review".to_string()]
        );
        assert_eq!(entry.status, InboxStatus::Received);
        assert_eq!(entry.processed_at_epoch, None);

        assert!(get_entry(&db, 99_999).expect("unknown").is_none());
    }

    // Status transitions (AB#1065): a new row is `Received` (no processed_at, no error);
    // `mark_processed` flips it to `Processed` + stamps `processed_at_epoch` + clears error;
    // `mark_failed` flips it to `Failed` + records the message.
    #[test]
    fn mark_processed_and_failed_transition_status() {
        let db = Database::open_in_memory().expect("open db");
        let id = insert_dedup(&db, &event("k1", "p1", Some(1)), "raw", None, None)
            .expect("insert")
            .expect("new id");

        let received = get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(received.status, InboxStatus::Received);
        assert_eq!(received.error, None);

        mark_failed(&db, id, "boom").expect("mark failed");
        let failed = get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(failed.status, InboxStatus::Failed);
        assert_eq!(failed.error.as_deref(), Some("boom"));
        assert!(failed.processed_at_epoch.is_some());

        mark_processed(&db, id).expect("mark processed");
        let processed = get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(processed.status, InboxStatus::Processed);
        assert_eq!(processed.error, None, "mark_processed clears the error");
        assert!(processed.processed_at_epoch.is_some());
    }

    #[test]
    fn requeue_failed_is_the_only_transition_back_to_received() {
        let db = Database::open_in_memory().expect("open db");

        let failed_id = insert_dedup(&db, &event("failed", "p1", Some(1)), "raw", None, None)
            .expect("insert failed row")
            .expect("new id");
        mark_failed(&db, failed_id, "boom").expect("mark failed");
        assert!(requeue_failed(&db, failed_id).expect("requeue failed"));
        let requeued = get_entry(&db, failed_id).expect("get").expect("exists");
        assert_eq!(requeued.status, InboxStatus::Received);
        assert_eq!(requeued.processed_at_epoch, None);
        assert_eq!(requeued.error, None);

        let received_id = insert_dedup(&db, &event("received", "p1", Some(2)), "raw", None, None)
            .expect("insert received row")
            .expect("new id");
        assert!(!requeue_failed(&db, received_id).expect("reject received"));

        let processed_id = insert_dedup(&db, &event("processed", "p1", Some(3)), "raw", None, None)
            .expect("insert processed row")
            .expect("new id");
        mark_processed(&db, processed_id).expect("mark processed");
        assert!(!requeue_failed(&db, processed_id).expect("reject processed"));
        assert!(!requeue_failed(&db, i64::MAX).expect("reject unknown"));
    }

    // `get_replayable` (AB#1065): returns the hydrated event, the source kind, the status, and
    // the stored `webhook_event_json`; an unknown id is `None`.
    #[test]
    fn get_replayable_returns_event_source_and_webhook_json() {
        let db = Database::open_in_memory().expect("open db");
        let id = insert_dedup(
            &db,
            &event("k1", "p1", Some(1)),
            "raw",
            Some("{\"projectId\":\"p1\"}"),
            None,
        )
        .expect("insert")
        .expect("new id");

        let r = get_replayable(&db, id).expect("get").expect("exists");
        assert_eq!(r.event.dedupe_key().as_str(), "k1");
        assert_eq!(r.source, SourceKind::Github);
        assert_eq!(r.status, InboxStatus::Received);
        assert_eq!(
            r.webhook_event_json.as_deref(),
            Some("{\"projectId\":\"p1\"}")
        );
        assert_eq!(r.candidate, None);

        assert!(get_replayable(&db, 99_999).expect("unknown").is_none());
    }

    #[test]
    fn get_replayable_round_trips_candidate_json() {
        let db = Database::open_in_memory().expect("open db");
        let cand = candidate(7, "review");
        let id = insert_dedup(
            &db,
            &event("k-candidate", "p1", Some(7)),
            "raw",
            Some("{\"projectId\":\"p1\"}"),
            Some(&cand),
        )
        .expect("insert")
        .expect("new id");

        let r = get_replayable(&db, id).expect("get").expect("exists");
        assert_eq!(r.candidate, Some(cand));
    }

    // `InboxStatus` lock (AB#1065, Medium carrier): the DB-stored `status_as_wire` string MUST
    // equal the serde form the frontend mirrors, and `status_from_wire` round-trips it — so a
    // drift between the column value and the `src/types.ts` `InboxStatus` union (or between the
    // two Rust sources) fails here. An unknown stored value degrades to `Failed` (replayable).
    #[test]
    fn inbox_status_wire_matches_serde_and_round_trips() {
        for status in [
            InboxStatus::Received,
            InboxStatus::Processed,
            InboxStatus::Failed,
        ] {
            let serde_wire = serde_json::to_value(status).expect("serializes");
            assert_eq!(
                serde_wire,
                status_as_wire(status),
                "as_wire must equal the serde form"
            );
            assert_eq!(
                status_from_wire(status_as_wire(status)),
                status,
                "round-trips"
            );
        }
        // Unknown / corrupt stored value degrades to Failed (terminal, replayable).
        assert_eq!(status_from_wire("???"), InboxStatus::Failed);
    }

    // `source_from_wire` is STRICT (AB#1065 review fix): every real SourceKind round-trips through
    // its serde wire string, and an UNKNOWN / corrupt value is an explicit error (NOT a silent
    // default to GitHub) — so a corrupt-source replay fails loudly instead of routing down the
    // wrong side-effectful path.
    #[test]
    fn source_from_wire_round_trips_known_and_errors_on_unknown() {
        for source in [SourceKind::Github, SourceKind::Azure, SourceKind::Bitbucket] {
            let wire = source_as_wire(source);
            assert_eq!(
                source_from_wire(&wire).expect("known source parses"),
                source,
                "round-trips"
            );
        }
        assert!(
            source_from_wire("gitlab").is_err(),
            "unknown source is an explicit error, not a silent GitHub default"
        );
    }

    // Replay path strictness (AB#1065 review fix): a row whose stored `source` is corrupt makes
    // `get_replayable` return an error (the side-effectful replay must not silently default to a
    // concrete source). Insert a valid row, corrupt its `source` column directly, then assert
    // `get_replayable` errors — while `list_by_project` (which never reads `source`) still lists it.
    #[test]
    fn get_replayable_errors_on_corrupt_source_but_list_stays_lenient() {
        let db = Database::open_in_memory().expect("open db");
        let id = insert_dedup(&db, &event("k1", "p1", Some(1)), "raw", None, None)
            .expect("insert")
            .expect("new id");
        db.with_conn(|conn| {
            conn.pragma_update(None, "ignore_check_constraints", true)?;
            conn.execute(
                "UPDATE inbox_event SET source = 'bogus-source' WHERE id = ?1",
                [id],
            )?;
            conn.pragma_update(None, "ignore_check_constraints", false)
        })
        .expect("corrupt source");

        assert!(
            get_replayable(&db, id).is_err(),
            "replay must error on a corrupt source, not default it"
        );
        // Listing does NOT read `source`, so a corrupt-source row still lists fine (lenient).
        let listed = list_by_project(&db, Some("p1")).expect("list");
        assert_eq!(listed.len(), 1, "corrupt source row still lists");
    }

    // Retention cap (AB#1065 review fix): a new insert prunes the oldest rows beyond
    // MAX_INBOX_EVENTS so the table stays bounded. Drive insert_dedup past the cap and assert the
    // row count holds at the cap and the OLDEST keys were dropped (newest kept). Uses a tiny seed
    // count by checking the cap value directly (MAX_INBOX_EVENTS is large, so seed cap+overflow
    // rows via raw inserts then one more through insert_dedup to trigger the prune).
    #[test]
    fn insert_dedup_prunes_oldest_terminal_rows_beyond_cap() {
        let db = Database::open_in_memory().expect("open db");
        // Seed exactly MAX_INBOX_EVENTS rows directly (cheap raw inserts), oldest id first.
        db.with_conn(|conn| {
            for i in 0..MAX_INBOX_EVENTS {
                conn.execute(
                    "INSERT INTO inbox_event \
                     (dedupe_key, source, event_type, project_id, repo, event_json, raw_payload, \
                      status, received_at_epoch) \
                     VALUES (?1, 'github', 'pullRequest', 'p1', 'owner/repo', '{}', 'raw', \
                             'processed', 1)",
                    rusqlite::params![format!("seed-{i}")],
                )?;
            }
            conn.execute(
                "INSERT INTO inbox_event \
                 (dedupe_key, source, event_type, project_id, repo, event_json, raw_payload, \
                  status, received_at_epoch, processed_at_epoch) \
                 VALUES ('external:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'github', 'pullRequest', \
                         'p1', 'owner/repo', \
                         '{\"payload\":{\"kind\":\"reviewRequest\"}}', 'raw', \
                         'processed', 1, 2)",
                [],
            )?;
            Ok(())
        })
        .expect("seed cap rows");

        // One more real insert through the capped path triggers the prune (cap + 1 → cap).
        insert_dedup(&db, &event("overflow", "p1", Some(1)), "raw", None, None)
            .expect("insert")
            .expect("new id");

        let count: i64 = db
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM inbox_event", [], |r| r.get(0)))
            .expect("count");
        assert_eq!(count, MAX_INBOX_EVENTS, "table capped at MAX_INBOX_EVENTS");
        // The newest row survived; the oldest seed row (`seed-0`, lowest id) was pruned.
        let oldest_gone: i64 = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM inbox_event WHERE dedupe_key = 'seed-0'",
                    [],
                    |r| r.get(0),
                )
            })
            .expect("count oldest");
        assert_eq!(oldest_gone, 0, "oldest row pruned");
        let newest_kept: i64 = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM inbox_event WHERE dedupe_key = 'overflow'",
                    [],
                    |r| r.get(0),
                )
            })
            .expect("count newest");
        assert_eq!(newest_kept, 1, "newest row kept");
        let receipt_kept: i64 = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM inbox_event WHERE dedupe_key LIKE 'external:%'",
                    [],
                    |r| r.get(0),
                )
            })
            .expect("count durable receipt");
        assert_eq!(
            receipt_kept, 1,
            "external receipts keep permanent idempotency identity"
        );
    }

    #[test]
    fn live_rows_are_never_pruned_and_report_retention_pressure() {
        let db = Database::open_in_memory().expect("open db");
        db.with_conn(|conn| {
            for i in 0..=MAX_INBOX_EVENTS {
                conn.execute(
                    "INSERT INTO inbox_event \
                     (dedupe_key, source, event_type, project_id, repo, event_json, raw_payload, status, received_at_epoch) \
                     VALUES (?1, 'github', 'pullRequest', 'p1', 'owner/repo', '{}', 'raw', 'received', 1)",
                    [format!("live-{i}")],
                )?;
            }
            Ok(())
        }).expect("seed live rows");
        assert!(retention_pressure(&db).expect("pressure"));
        assert_eq!(received_ids(&db).expect("live page").len(), 100);
        let count: i64 = db
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM inbox_event", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(count, MAX_INBOX_EVENTS + 1);
    }

    // Listing leniency (AB#1065 review fix): a row whose `event_json` is corrupt is SKIPPED by
    // `list_by_project` (best-effort — one bad blob must not blank the list or error). Insert a
    // good row + a bad-JSON row directly, assert only the good one lists and there's no error.
    #[test]
    fn list_by_project_skips_corrupt_event_json_row() {
        let db = Database::open_in_memory().expect("open db");
        insert_dedup(&db, &event("good", "p1", Some(1)), "raw", None, None).expect("good");
        db.with_conn(|conn| {
            conn.pragma_update(None, "ignore_check_constraints", true)?;
            conn.execute(
                "INSERT INTO inbox_event \
                 (dedupe_key, source, event_type, project_id, repo, event_json, raw_payload, \
                  status, received_at_epoch) \
                 VALUES ('bad', 'github', 'pullRequest', 'p1', 'owner/repo', 'NOT JSON', 'raw', \
                         'received', 2)",
                [],
            )?;
            conn.pragma_update(None, "ignore_check_constraints", false)
        })
        .expect("insert corrupt row");

        let listed = list_by_project(&db, Some("p1")).expect("list does not error");
        let keys: Vec<String> = listed
            .iter()
            .map(|e| e.event.dedupe_key().to_string())
            .collect();
        assert_eq!(keys, vec!["good".to_string()], "corrupt-json row skipped");
    }
}
