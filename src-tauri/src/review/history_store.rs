//! Review session + history persistence (#70).
//!
//! The in-memory [`super::session::SessionRegistry`] stays the authority for dedup /
//! active-pairs / live status; this module is its DURABLE MIRROR in the unified SQLite
//! store. It persists (1) session metadata (`review_session` — so a PR's sessions
//! survive a restart and are listable per-PR) and (2) session HISTORY content
//! (`review_history_item` — the streamed message/reasoning deltas that were previously
//! emitted-and-discarded, so a history session can be reopened with its prior output).
//!
//! Slice-local: the review slice owns these tables' queries, reaching SQLite only through
//! the horizontal [`crate::db::Database`] handle (not a cross-slice import).

use serde::Serialize;

use super::session::{SessionInfo, SessionStatus};
use crate::db::Database;
use crate::error::AppResult;

/// One persisted history item — a coalesced message/reasoning block (#70). Same wire
/// shape as the frontend's `StreamItem` (`itemId` / `kind` / `text`), so
/// `get_session_history` can hydrate the review panel directly. Slice-private; mirrored
/// in `src/review/types.ts`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryItem {
    pub item_id: String,
    pub kind: String,
    pub text: String,
}

/// Wall-clock seconds for the session timestamps. Review-local (the `pr` slice has its
/// own `now_epoch`; keeping one here avoids a cross-slice import — the review slice stays
/// self-contained). Degrades to 0 on a pre-epoch clock rather than panicking.
fn now_epoch() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// [`SessionStatus`] → its pinned camelCase wire string (the form stored in the `status`
/// column), via the same serde contract `list_review_sessions` uses.
fn status_wire(status: SessionStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Wire string → [`SessionStatus`] (reverse of [`status_wire`]), for projecting stored
/// rows back into [`SessionInfo`]. An unknown string degrades to `Failed` (a terminal
/// status — never resurrects a dead session as live).
fn status_from_wire(s: &str) -> SessionStatus {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .unwrap_or(SessionStatus::Failed)
}

/// Upserts a session row from its in-memory [`SessionInfo`] (#70). First insert stamps
/// `created_at`; a later call (turn/status transition) updates the mutable fields +
/// `updated_at`, preserving `created_at`. Persistence is best-effort — callers log +
/// swallow errors so a DB hiccup never breaks the live session.
pub fn upsert_session(db: &Database, info: &SessionInfo) -> AppResult<()> {
    let now = now_epoch() as i64;
    let status = status_wire(info.status);
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO review_session \
             (thread_id, project_id, pr_number, turn_id, kind, status, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7) \
             ON CONFLICT(thread_id) DO UPDATE SET \
               project_id = excluded.project_id, \
               pr_number  = excluded.pr_number, \
               turn_id    = excluded.turn_id, \
               kind       = excluded.kind, \
               status     = excluded.status, \
               updated_at = excluded.updated_at",
            rusqlite::params![
                info.thread_id,
                info.project_id,
                info.pr_number as i64,
                info.turn_id,
                info.kind,
                status,
                now,
            ],
        )
        .map(|_| ())
    })
}

/// Updates a session's status (#70) without the full [`SessionInfo`] — the pump's
/// terminal branch + `set_status` callsites have only the thread id. A no-op if the row
/// doesn't exist yet (the session insert always precedes any status update in practice).
pub fn set_status(db: &Database, thread_id: &str, status: SessionStatus) -> AppResult<()> {
    let now = now_epoch() as i64;
    let status = status_wire(status);
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE review_session SET status = ?2, updated_at = ?3 WHERE thread_id = ?1",
            rusqlite::params![thread_id, status, now],
        )
        .map(|_| ())
    })
}

/// Appends a streamed delta to a session's history (#70), COALESCING by `(thread_id,
/// item_id)`: the first delta for an item inserts a row; later deltas concatenate onto
/// its `text`. Mirrors the frontend's `appendDelta` so thousands of deltas collapse into
/// a handful of rows. `kind` is `"message"` or `"reasoning"` (constant per item id).
pub fn append_item(
    db: &Database,
    thread_id: &str,
    item_id: &str,
    kind: &str,
    text: &str,
) -> AppResult<()> {
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO review_history_item (thread_id, item_id, kind, text) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(thread_id, item_id) DO UPDATE SET text = text || excluded.text",
            rusqlite::params![thread_id, item_id, kind, text],
        )
        .map(|_| ())
    })
}

/// A session's stored history items in stream order (#70) — what `get_session_history`
/// returns so the UI can show content produced before the user opened the session.
pub fn get_history(db: &Database, thread_id: &str) -> AppResult<Vec<HistoryItem>> {
    db.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT item_id, kind, text FROM review_history_item \
             WHERE thread_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([thread_id], |r| {
            Ok(HistoryItem {
                item_id: r.get(0)?,
                kind: r.get(1)?,
                text: r.get(2)?,
            })
        })?;
        rows.collect()
    })
}

/// A PR's persisted sessions (#70), newest first — what `get_pr_sessions` returns so each
/// PR can restore its session list after a restart. Reads the DURABLE `review_session`
/// table (vs `list_review_sessions`'s in-memory snapshot, which is empty after restart).
pub fn get_pr_sessions(
    db: &Database,
    project_id: &str,
    pr_number: u64,
) -> AppResult<Vec<SessionInfo>> {
    db.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT thread_id, project_id, pr_number, turn_id, kind, status \
             FROM review_session \
             WHERE project_id = ?1 AND pr_number = ?2 ORDER BY created_at DESC, thread_id",
        )?;
        let rows = stmt.query_map(rusqlite::params![project_id, pr_number as i64], |r| {
            let status: String = r.get(5)?;
            Ok(SessionInfo {
                thread_id: r.get(0)?,
                project_id: r.get(1)?,
                pr_number: r.get::<_, i64>(2)? as u64,
                turn_id: r.get(3)?,
                kind: r.get(4)?,
                status: status_from_wire(&status),
            })
        })?;
        rows.collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(thread: &str, pr: u64, status: SessionStatus) -> SessionInfo {
        SessionInfo {
            project_id: "alpha".to_string(),
            thread_id: thread.to_string(),
            turn_id: "t1".to_string(),
            pr_number: pr,
            kind: "review".to_string(),
            status,
        }
    }

    // Session metadata round-trip (#70, Medium carrier): upsert then `get_pr_sessions`
    // must surface the row with its status, and a status transition must update (not
    // duplicate) it. Restart durability rests on this read path.
    #[test]
    fn session_upsert_and_status_transition_round_trip() {
        let db = Database::open_in_memory().expect("open db");
        upsert_session(&db, &info("th-1", 12, SessionStatus::Starting)).expect("insert");
        set_status(&db, "th-1", SessionStatus::Done).expect("transition");

        let sessions = get_pr_sessions(&db, "alpha", 12).expect("list");
        assert_eq!(sessions.len(), 1, "transition updates, not duplicates");
        assert_eq!(sessions[0].thread_id, "th-1");
        assert_eq!(sessions[0].status, SessionStatus::Done);

        // Scoped per (project, PR): a different PR sees nothing.
        assert!(get_pr_sessions(&db, "alpha", 99).expect("list").is_empty());
    }

    // History capture (#70): deltas for one item id COALESCE into a single concatenated
    // row, distinct item ids are separate rows, and the read is in stream (`id`) order.
    #[test]
    fn append_item_coalesces_by_item_id_and_reads_in_order() {
        let db = Database::open_in_memory().expect("open db");
        upsert_session(&db, &info("th-1", 12, SessionStatus::Running)).expect("session");

        append_item(&db, "th-1", "i1", "reasoning", "Plan").expect("a");
        append_item(&db, "th-1", "i2", "message", "Hello ").expect("b");
        append_item(&db, "th-1", "i1", "reasoning", "ning done").expect("c");
        append_item(&db, "th-1", "i2", "message", "world").expect("d");

        let items = get_history(&db, "th-1").expect("history");
        assert_eq!(items.len(), 2, "two item ids → two coalesced rows");
        // i1 inserted first → comes first; deltas concatenated in arrival order.
        assert_eq!(items[0].item_id, "i1");
        assert_eq!(items[0].kind, "reasoning");
        assert_eq!(items[0].text, "Planning done");
        assert_eq!(items[1].item_id, "i2");
        assert_eq!(items[1].text, "Hello world");
    }

    // `HistoryItem` wire-shape lock (#70, Medium carrier): camelCase `itemId` present,
    // snake_case absent — keeps the Rust↔`src/review/types.ts` (`StreamItem`) contract.
    #[test]
    fn history_item_wire_shape_is_camel_case() {
        let item = HistoryItem {
            item_id: "i1".to_string(),
            kind: "message".to_string(),
            text: "hi".to_string(),
        };
        let v = serde_json::to_value(&item).expect("serializes");
        assert!(v.get("itemId").is_some());
        assert!(v.get("kind").is_some());
        assert!(v.get("text").is_some());
        assert!(v.get("item_id").is_none());
    }
}
