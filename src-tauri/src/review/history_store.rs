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

use serde::{Deserialize, Serialize};

use super::session::{SessionInfo, SessionStatus};
use crate::db::Database;
use crate::error::AppResult;

/// The kind of a persisted history block (pr-review F7). The Rust write side can now ONLY
/// express the two legal kinds, closing the gap where `kind: String` let `append_item`
/// persist an arbitrary string while the TS `StreamItem.kind` was already a union. Serde
/// pins the wire strings (`"message"` / `"reasoning"`) the frontend mirrors; the DB `kind`
/// column stores the SAME strings via [`HistoryItemKind::as_wire`] (one source, locked
/// equal to the serde form by `history_item_kind_wire_matches_serde`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HistoryItemKind {
    Message,
    Reasoning,
}

impl HistoryItemKind {
    /// Pinned wire string stored in the `kind` column (== the serde form the frontend reads).
    fn as_wire(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Reasoning => "reasoning",
        }
    }

    /// Reverse of [`Self::as_wire`] for projecting a stored row back. An unknown / corrupt
    /// value degrades to `Message` (content kept, rendered expanded) rather than dropping
    /// the row — mirrors `status_from_wire`'s lenient default.
    fn from_wire(s: &str) -> Self {
        match s {
            "reasoning" => Self::Reasoning,
            _ => Self::Message,
        }
    }
}

/// One persisted history item — a coalesced message/reasoning block (#70). Same wire
/// shape as the frontend's `StreamItem` (`itemId` / `kind` / `text`), so
/// `get_session_history` can hydrate the review panel directly. Slice-private; mirrored
/// in `src/review/types.ts`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryItem {
    pub item_id: String,
    pub kind: HistoryItemKind,
    pub text: String,
}

/// Wall-clock seconds for the session timestamps. Review-local (the `pr` slice has its
/// own `now_epoch`; keeping one here avoids a cross-slice import — the review slice stays
/// self-contained). Degrades to 0 on a pre-epoch clock rather than panicking. `pub(super)`
/// so `session` stamps a live session's `created_at_epoch` (review F10) from the same clock
/// that stamps the persisted `created_at`, keeping the live and durable order consistent.
pub(super) fn now_epoch() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// [`SessionStatus`] → its pinned camelCase wire string (the form stored in the `status`
/// column), via the same serde contract `list_review_sessions` uses. `SessionStatus` is a
/// unit enum with `#[serde(rename_all = "camelCase")]`, so serialization to a JSON string
/// CANNOT fail — `expect` fail-fast surfaces a serde regression loudly rather than
/// silently writing an empty status that `status_from_wire` would then read as `Failed`
/// (review F3).
fn status_wire(status: SessionStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .expect("SessionStatus serializes to a JSON string (unit enum, known variants)")
}

/// Wire string → [`SessionStatus`] (reverse of [`status_wire`]), for projecting stored
/// rows back into [`SessionInfo`]. An unknown string degrades to `Failed` (a terminal
/// status — never resurrects a dead session as live).
fn status_from_wire(s: &str) -> SessionStatus {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .unwrap_or(SessionStatus::Failed)
}

/// Per-PR cap on persisted review sessions (review F7). Beyond this, [`prune_pr_sessions`]
/// drops the oldest on each upsert so `review_session` / `review_history_item` stay bounded
/// (every dispatch adds a session; nothing else deleted them before this).
const MAX_SESSIONS_PER_PR: i64 = 50;

/// Caps a PR's persisted sessions at [`MAX_SESSIONS_PER_PR`] (newest by `created_at` first),
/// deleting the pruned threads' history rows too (review F7). The FK to `review_session` was
/// dropped (review F2, best-effort history), so the cascade is MANUAL: delete history FIRST
/// (while the prune set's session rows still exist for the subquery to match), then the
/// session rows. The `OFFSET` no-ops cheaply when a PR has ≤ the cap.
fn prune_pr_sessions(
    conn: &rusqlite::Connection,
    project_id: &str,
    pr_number: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM review_history_item WHERE thread_id IN ( \
             SELECT thread_id FROM review_session \
             WHERE project_id = ?1 AND pr_number = ?2 \
             ORDER BY created_at DESC, thread_id LIMIT -1 OFFSET ?3)",
        rusqlite::params![project_id, pr_number, MAX_SESSIONS_PER_PR],
    )?;
    conn.execute(
        "DELETE FROM review_session WHERE thread_id IN ( \
             SELECT thread_id FROM review_session \
             WHERE project_id = ?1 AND pr_number = ?2 \
             ORDER BY created_at DESC, thread_id LIMIT -1 OFFSET ?3)",
        rusqlite::params![project_id, pr_number, MAX_SESSIONS_PER_PR],
    )?;
    Ok(())
}

/// Upserts a session row from its in-memory [`SessionInfo`] (#70). First insert stamps
/// `created_at`; a later call (turn/status transition) updates the mutable fields +
/// `updated_at`, preserving `created_at`. Persistence is best-effort — callers log +
/// swallow errors so a DB hiccup never breaks the live session.
pub fn upsert_session(db: &Database, info: &SessionInfo) -> AppResult<()> {
    let now = now_epoch() as i64;
    let status = status_wire(info.status);
    // upsert + prune are ONE lifecycle write: run them in a transaction so a prune failure
    // can't leave the new row with a half-applied prune, and the multi-statement prune
    // commits/rolls back atomically (pr-review F5).
    db.with_tx(|tx| {
        tx.execute(
            "INSERT INTO review_session \
             (thread_id, project_id, pr_number, turn_id, kind, status, created_at, updated_at, comment_url) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8) \
             ON CONFLICT(thread_id) DO UPDATE SET \
               project_id = excluded.project_id, \
               pr_number  = excluded.pr_number, \
               turn_id    = excluded.turn_id, \
               kind       = excluded.kind, \
               status     = excluded.status, \
               updated_at = excluded.updated_at, \
               comment_url = COALESCE(excluded.comment_url, comment_url)",
            rusqlite::params![
                info.thread_id,
                info.project_id,
                info.pr_number as i64,
                info.turn_id,
                info.kind,
                status,
                now,
                // AB#1042: `comment_url` is None during start (Starting/Running upserts); the
                // terminal URL is written by `set_status_and_comment_url`. COALESCE on conflict
                // means a None upsert never clobbers an already-resolved URL.
                info.comment_url,
            ],
        )
        .map_err(crate::db::map_err)?;
        // Cap this PR's persisted sessions after the insert so `review_session` /
        // `review_history_item` don't grow unbounded (review F7).
        prune_pr_sessions(tx, &info.project_id, info.pr_number as i64)
            .map_err(crate::db::map_err)?;
        Ok(())
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

/// Atomically writes the TERMINAL status AND the resolved `comment_url` for a session
/// (AB#1042) — the durable half of [`super::session::finalize_turn`]'s terminal write, so a
/// woken completion subscriber reading the DB sees the status and URL land together. A
/// `None` `comment_url` writes SQL NULL (the terminal had no comment — interrupted / failed
/// / Bitbucket). A no-op if the row doesn't exist (the session insert always precedes a
/// terminal in practice). Best-effort, like [`set_status`].
pub fn set_status_and_comment_url(
    db: &Database,
    thread_id: &str,
    status: SessionStatus,
    comment_url: Option<&str>,
) -> AppResult<()> {
    let now = now_epoch() as i64;
    let status = status_wire(status);
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE review_session SET status = ?2, comment_url = ?3, updated_at = ?4 \
             WHERE thread_id = ?1",
            rusqlite::params![thread_id, status, comment_url, now],
        )
        .map(|_| ())
    })
}

/// Appends a streamed delta to a session's history (#70), COALESCING by `(thread_id,
/// item_id)`: the first delta for an item inserts a row; later deltas concatenate onto
/// its `text`. Mirrors the frontend's `appendDelta` so thousands of deltas collapse into
/// a handful of rows. `kind` is the typed [`HistoryItemKind`] (pr-review F7), stored as its
/// pinned wire string so the write side cannot persist an illegal kind.
pub fn append_item(
    db: &Database,
    thread_id: &str,
    item_id: &str,
    kind: HistoryItemKind,
    text: &str,
) -> AppResult<()> {
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO review_history_item (thread_id, item_id, kind, text) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(thread_id, item_id) DO UPDATE SET text = text || excluded.text",
            rusqlite::params![thread_id, item_id, kind.as_wire(), text],
        )
        .map(|_| ())
    })
}

/// A session's stored history items in stream order (#70), SCOPED to its owning
/// `(project_id, pr_number)` (pr-review F6): the JOIN to `review_session` means a
/// `thread_id` that doesn't belong to the claimed PR returns nothing — the command can't be
/// used to read another PR's history by raw id. An orphan history (no session row) is
/// likewise not returned, since ownership can't be verified.
pub fn get_history(
    db: &Database,
    project_id: &str,
    pr_number: u64,
    thread_id: &str,
) -> AppResult<Vec<HistoryItem>> {
    db.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT h.item_id, h.kind, h.text FROM review_history_item h \
             JOIN review_session s ON s.thread_id = h.thread_id \
             WHERE h.thread_id = ?1 AND s.project_id = ?2 AND s.pr_number = ?3 \
             ORDER BY h.id",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![thread_id, project_id, pr_number as i64],
            |r| {
                let kind: String = r.get(1)?;
                Ok(HistoryItem {
                    item_id: r.get(0)?,
                    kind: HistoryItemKind::from_wire(&kind),
                    text: r.get(2)?,
                })
            },
        )?;
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
            "SELECT thread_id, project_id, pr_number, turn_id, kind, status, created_at, comment_url \
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
                created_at_epoch: r.get::<_, i64>(6)? as u64,
                // AB#1042: NULL (no comment) → None; a resolved terminal URL → Some.
                comment_url: r.get::<_, Option<String>>(7)?,
            })
        })?;
        rows.collect()
    })
}

/// Reconciles sessions left non-terminal by a previous process (pr-review F1). A persisted
/// `starting` / `running` / `interrupting` status means the prior run died mid-session: its
/// live pump is gone, so the session is NOT actually running. Flip such rows to `failed` at
/// startup so the UI doesn't restore a dead session as active. Returns the count flipped.
pub fn fail_orphaned_sessions(db: &Database) -> AppResult<usize> {
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE review_session SET status = 'failed' \
             WHERE status IN ('starting', 'running', 'interrupting')",
            [],
        )
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
            created_at_epoch: 0,
            comment_url: None,
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

    // AB#1042: the terminal write (`set_status_and_comment_url`) persists status + URL
    // together, and `get_pr_sessions` reads the URL back into `SessionInfo.comment_url`. A
    // None terminal leaves it NULL (read back as None). The Starting/Running upserts carry
    // None and must NOT clobber a later-written URL (the COALESCE on conflict).
    #[test]
    fn terminal_comment_url_persists_and_reads_back() {
        let db = Database::open_in_memory().expect("open db");
        // Starting then Running upserts (both None comment_url).
        upsert_session(&db, &info("th-1", 12, SessionStatus::Starting)).expect("starting");
        upsert_session(&db, &info("th-1", 12, SessionStatus::Running)).expect("running");

        // Terminal: write Done + the resolved URL atomically.
        set_status_and_comment_url(&db, "th-1", SessionStatus::Done, Some("https://x/c"))
            .expect("terminal write");

        let sessions = get_pr_sessions(&db, "alpha", 12).expect("list");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Done);
        assert_eq!(sessions[0].comment_url.as_deref(), Some("https://x/c"));

        // A later None terminal (e.g. a re-run that interrupted) overwrites with NULL — the
        // dedicated terminal write is explicit, not a COALESCE (only the start upsert preserves).
        set_status_and_comment_url(&db, "th-1", SessionStatus::Failed, None).expect("none write");
        let after = get_pr_sessions(&db, "alpha", 12).expect("list");
        assert!(
            after[0].comment_url.is_none(),
            "explicit None terminal clears the URL"
        );
    }

    // History capture (#70): deltas for one item id COALESCE into a single concatenated
    // row, distinct item ids are separate rows, and the read is in stream (`id`) order.
    #[test]
    fn append_item_coalesces_by_item_id_and_reads_in_order() {
        let db = Database::open_in_memory().expect("open db");
        upsert_session(&db, &info("th-1", 12, SessionStatus::Running)).expect("session");

        append_item(&db, "th-1", "i1", HistoryItemKind::Reasoning, "Plan").expect("a");
        append_item(&db, "th-1", "i2", HistoryItemKind::Message, "Hello ").expect("b");
        append_item(&db, "th-1", "i1", HistoryItemKind::Reasoning, "ning done").expect("c");
        append_item(&db, "th-1", "i2", HistoryItemKind::Message, "world").expect("d");

        let items = get_history(&db, "alpha", 12, "th-1").expect("history");
        assert_eq!(items.len(), 2, "two item ids → two coalesced rows");
        // i1 inserted first → comes first; deltas concatenated in arrival order.
        assert_eq!(items[0].item_id, "i1");
        assert_eq!(items[0].kind, HistoryItemKind::Reasoning);
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
            kind: HistoryItemKind::Message,
            text: "hi".to_string(),
        };
        let v = serde_json::to_value(&item).expect("serializes");
        assert!(v.get("itemId").is_some());
        assert_eq!(v["kind"], "message");
        assert!(v.get("text").is_some());
        assert!(v.get("item_id").is_none());
    }

    // `HistoryItemKind` lock (pr-review F7, Medium carrier): the DB-stored `as_wire` string
    // MUST equal the serde form the frontend mirrors, and `from_wire` round-trips it — so a
    // drift between the column value and the `src/review/types.ts` `StreamItem.kind` union
    // (or between the two Rust sources) fails here.
    #[test]
    fn history_item_kind_wire_matches_serde_and_round_trips() {
        for kind in [HistoryItemKind::Message, HistoryItemKind::Reasoning] {
            let serde_wire = serde_json::to_value(kind).expect("serializes");
            assert_eq!(
                serde_wire,
                kind.as_wire(),
                "as_wire must equal the serde form"
            );
            assert_eq!(
                HistoryItemKind::from_wire(kind.as_wire()),
                kind,
                "round-trips"
            );
        }
        // Unknown / corrupt stored value degrades to Message (content kept, not dropped).
        assert_eq!(HistoryItemKind::from_wire("???"), HistoryItemKind::Message);
    }

    // `get_pr_sessions` orders newest-first (`created_at DESC`) — the user-facing order
    // (review F5). Insert two rows with explicit timestamps (upsert stamps `now`, which a
    // fast test can't distinguish) and assert the newer thread comes first.
    #[test]
    fn get_pr_sessions_orders_newest_first() {
        let db = Database::open_in_memory().expect("open db");
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO review_session \
                 (thread_id, project_id, pr_number, turn_id, kind, status, created_at, updated_at) \
                 VALUES ('old', 'alpha', 12, '', 'review', 'done', 100, 100), \
                        ('new', 'alpha', 12, '', 'review', 'running', 200, 200)",
                [],
            )?;
            Ok(())
        })
        .expect("seed");

        let sessions = get_pr_sessions(&db, "alpha", 12).expect("list");
        let order: Vec<String> = sessions.iter().map(|s| s.thread_id.clone()).collect();
        assert_eq!(order, vec!["new".to_string(), "old".to_string()]);
        // The persisted `created_at` flows into the wire `created_at_epoch` (review F10) —
        // the frontend's newest-first sort key, replacing the old threadId scramble.
        assert_eq!(sessions[0].created_at_epoch, 200);
        assert_eq!(sessions[1].created_at_epoch, 100);
    }

    // Every `SessionStatus` survives the store → wire-string → enum round-trip (review
    // F5): closes the `status_from_wire` Deserialize funnel (the existing pinned-wire test
    // only locks Serialize). A drift in either direction fails here.
    #[test]
    fn session_status_round_trips_through_storage() {
        let db = Database::open_in_memory().expect("open db");
        let cases = [
            ("th-a", SessionStatus::Starting),
            ("th-b", SessionStatus::Running),
            ("th-c", SessionStatus::Interrupting),
            ("th-d", SessionStatus::Done),
            ("th-e", SessionStatus::Failed),
        ];
        for (thread, status) in cases {
            upsert_session(&db, &info(thread, 12, status)).expect("upsert");
        }
        let by_thread: std::collections::HashMap<String, SessionStatus> =
            get_pr_sessions(&db, "alpha", 12)
                .expect("list")
                .into_iter()
                .map(|s| (s.thread_id, s.status))
                .collect();
        for (thread, status) in cases {
            assert_eq!(by_thread.get(thread), Some(&status), "status for {thread}");
        }
    }

    // FK to `review_session` was dropped (review F2): a best-effort `append_item` must
    // succeed even when the session row is missing (an orphan row, not an error). The scoped
    // read (pr-review F6) does NOT return that orphan — ownership can't be verified without a
    // session row — but the row is still persisted (content not lost), confirmed directly.
    #[test]
    fn append_item_without_session_row_is_orphan_not_error() {
        let db = Database::open_in_memory().expect("open db");
        append_item(&db, "orphan", "i1", HistoryItemKind::Message, "hi").expect("append");
        // Scoped read can't verify ownership (no session row) → empty.
        assert!(get_history(&db, "alpha", 7, "orphan")
            .expect("history")
            .is_empty());
        // But the row was persisted, not dropped.
        let rows: i64 = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM review_history_item WHERE thread_id = 'orphan'",
                    [],
                    |r| r.get(0),
                )
            })
            .expect("count");
        assert_eq!(
            rows, 1,
            "orphan row persisted (F2), just unreadable via scoped get (F6)"
        );
    }

    // Unbounded-growth backstop (review F7): an upsert caps a PR's persisted sessions at
    // MAX_SESSIONS_PER_PR (newest first) AND cascade-deletes the pruned threads' history
    // (no FK, so the cascade is manual). Seed cap+2 older rows, upsert one newer → the two
    // oldest sessions and their history are gone; the cap holds.
    #[test]
    fn upsert_prunes_oldest_sessions_and_their_history_to_cap() {
        let db = Database::open_in_memory().expect("open db");
        let seeded = (MAX_SESSIONS_PER_PR + 2) as usize;
        db.with_conn(|conn| {
            for i in 0..seeded {
                let t = format!("t{i:03}");
                conn.execute(
                    "INSERT INTO review_session \
                     (thread_id, project_id, pr_number, turn_id, kind, status, created_at, updated_at) \
                     VALUES (?1, 'alpha', 7, '', 'review', 'done', ?2, ?2)",
                    rusqlite::params![t, i as i64],
                )?;
                conn.execute(
                    "INSERT INTO review_history_item (thread_id, item_id, kind, text) \
                     VALUES (?1, 'i1', 'message', 'x')",
                    rusqlite::params![t],
                )?;
            }
            Ok(())
        })
        .expect("seed");

        // `upsert_session` stamps `created_at = now` (> every seeded stamp), so the new
        // session is newest and its insert triggers the prune.
        upsert_session(&db, &info("t-new", 7, SessionStatus::Running)).expect("upsert");

        let sessions = get_pr_sessions(&db, "alpha", 7).expect("list");
        assert_eq!(
            sessions.len(),
            MAX_SESSIONS_PER_PR as usize,
            "capped at the per-PR max"
        );
        assert_eq!(sessions[0].thread_id, "t-new", "newest kept first");
        // The two oldest seeded sessions were pruned, and their history cascade-deleted.
        // Count the rows directly (the scoped `get_history` would read empty anyway once the
        // session row is gone, so it can't prove the history rows themselves were removed).
        assert!(!sessions.iter().any(|s| s.thread_id == "t000"));
        let orphan_history: i64 = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM review_history_item WHERE thread_id IN ('t000', 't001')",
                    [],
                    |r| r.get(0),
                )
            })
            .expect("count");
        assert_eq!(
            orphan_history, 0,
            "pruned sessions' history cascade-deleted (no orphans)"
        );
    }

    // Startup reconciliation (pr-review F1): non-terminal statuses left by a dead process
    // flip to `failed`; terminal ones (done / already-failed) are untouched.
    #[test]
    fn fail_orphaned_sessions_flips_only_non_terminal() {
        let db = Database::open_in_memory().expect("open db");
        for (thread, status) in [
            ("run", SessionStatus::Running),
            ("start", SessionStatus::Starting),
            ("intr", SessionStatus::Interrupting),
            ("done", SessionStatus::Done),
        ] {
            upsert_session(&db, &info(thread, 7, status)).expect("seed");
        }
        let flipped = fail_orphaned_sessions(&db).expect("reconcile");
        assert_eq!(
            flipped, 3,
            "the three non-terminal sessions flip, done does not"
        );
        let by_thread: std::collections::HashMap<String, SessionStatus> =
            get_pr_sessions(&db, "alpha", 7)
                .expect("list")
                .into_iter()
                .map(|s| (s.thread_id, s.status))
                .collect();
        assert_eq!(by_thread["run"], SessionStatus::Failed);
        assert_eq!(by_thread["start"], SessionStatus::Failed);
        assert_eq!(by_thread["intr"], SessionStatus::Failed);
        assert_eq!(by_thread["done"], SessionStatus::Done, "terminal untouched");
    }
}
