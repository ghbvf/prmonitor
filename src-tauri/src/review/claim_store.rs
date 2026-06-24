//! Outbox review-execution claim persistence (AB#1204).
//!
//! Closes the cross-restart duplicate-review window. The AB#1069 outbox executor is
//! at-least-once: a crash AFTER an outbox `review`/`check` action started a review but BEFORE the
//! row was marked `done` re-runs that action next boot, and the purely-in-memory
//! [`super::session::SessionRegistry::try_reserve_pair`] reserves freely after a restart — so the
//! replay would launch a DUPLICATE review (a second `pm:` comment).
//!
//! This table is a **write-ahead claim keyed by the OUTBOX ROW id** (`outbox_review_claim`,
//! schema v6 in [`crate::db`]). The outbox row is the unit of at-least-once replay, so keying the
//! claim on its id lets a replay distinguish *"this same action already started a review"*
//! (resolve its outcome, don't duplicate) from *"a new review need"* (a new commit ⇒ a new outbox
//! row ⇒ a fresh claim). Keying on `(project, pr, kind)` instead would wrongly suppress a
//! legitimate re-review of a new commit — the trap this design avoids.
//!
//! [`try_reserve_pair`] is deliberately left untouched (it stays the pure in-memory test-and-set
//! whose "terminal sessions don't block" semantics make new-commit re-review work); the durable
//! guard lives one layer up, at the outbox executor entry ([`super::commands::start_for_outbox`]).
//!
//! Slice-local: the review slice owns this table's queries, reaching SQLite only through the
//! horizontal [`crate::db::Database`] handle (not a cross-slice import). DDL lives in `db.rs`.

use crate::db::Database;
use crate::error::AppResult;

/// Write-ahead claim for an outbox row that is about to start a review (AB#1204): insert a row
/// keyed by `outbox_id` (`ON CONFLICT(outbox_id) DO NOTHING`, the **Hard** idempotency carrier —
/// a double-claim for the same row is unexpressible), then read back its `thread_id`.
///
/// Returns the claim's `thread_id` **only when this is a replay of a row that already reached
/// `thread/start`** (an existing claim with a non-NULL `thread_id`); returns `None` for a fresh
/// claim OR an existing claim that never attached a thread (a crash before `thread/start`) — both
/// mean no review was launched yet, so the caller proceeds to start (at-least-once). A `Some` value
/// is resolved by the caller against the durable `review_session` row to decide suppress-vs-rerun.
///
/// **Propagating error (NOT best-effort)** — unlike the surrounding `persist_session`: if this
/// write-ahead fails, [`super::commands::start_for_outbox`] returns `Err`, the worker retries the
/// row, and no review started yet (safe). Swallowing it would re-open the very window this closes.
pub fn begin_claim(
    db: &Database,
    outbox_id: i64,
    project_id: &str,
    pr_number: u64,
    kind: &str,
) -> AppResult<Option<String>> {
    let now = super::history_store::now_epoch() as i64;
    // **Hard-ized atomicity (AB#1204):** the INSERT and the read-back SELECT are wrapped in ONE
    // [`Database::with_tx`] transaction, so "claim this `outbox_id` once AND read back its
    // `thread_id`" is a single atomic unit committed/rolled back together — the atomicity now rests
    // on the SQL transaction, NOT on an external "only one `Mutex<Connection>`" invariant.
    //   - Upstream carrier (**Hard**): `outbox_review_claim.outbox_id PRIMARY KEY` (`db.rs` SCHEMA_V6)
    //     makes a duplicate claim for the same row UNEXPRESSIBLE — `ON CONFLICT(outbox_id) DO NOTHING`
    //     leaves the existing row untouched, so the SELECT always reads the ONE authoritative claim.
    //   - Why a transaction, not two `with_conn` calls: splitting the INSERT and the SELECT into two
    //     separate connection acquisitions would re-open a window where another claim path could
    //     interleave between them; the single `with_tx` closes that seam at the SQL layer.
    db.with_tx(|tx| {
        tx.execute(
            "INSERT INTO outbox_review_claim \
             (outbox_id, project_id, pr_number, kind, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(outbox_id) DO NOTHING",
            rusqlite::params![outbox_id, project_id, pr_number as i64, kind, now],
        )
        .map_err(crate::db::map_err)?;
        // The row always exists now (just inserted, or pre-existing). A NULL `thread_id` (fresh
        // claim, or a crash before `attach_thread`) maps to `None` → the caller starts the review.
        tx.query_row(
            "SELECT thread_id FROM outbox_review_claim WHERE outbox_id = ?1",
            rusqlite::params![outbox_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .map_err(crate::db::map_err)
    })
}

/// Record the `thread_id` a claim's review started under (AB#1204), called right after
/// `thread/start` succeeds and BEFORE the turn posts its `pm:` comment — so a later crash-replay
/// finds the breadcrumb and resolves the prior review instead of duplicating it. Best-effort: a
/// failure here only narrows the window back toward the pre-AB#1204 behavior, never a regression.
pub fn attach_thread(db: &Database, outbox_id: i64, thread_id: &str) -> AppResult<()> {
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE outbox_review_claim SET thread_id = ?2 WHERE outbox_id = ?1",
            rusqlite::params![outbox_id, thread_id],
        )
        .map(|_| ())
    })
}

/// Drop a claim once its outbox row terminalizes (`done`/`dead`) — table hygiene only (AB#1204).
/// Correctness does NOT depend on release: a terminal row is never re-claimed by the worker, so a
/// stale claim is never read again; this just bounds the table. Idempotent (a missing row no-ops).
pub fn release_claim(db: &Database, outbox_id: i64) -> AppResult<()> {
    db.with_conn(|conn| {
        conn.execute(
            "DELETE FROM outbox_review_claim WHERE outbox_id = ?1",
            rusqlite::params![outbox_id],
        )
        .map(|_| ())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Raw read of a claim's stored `thread_id` (test-only): `Some(None)` = row present with NULL
    /// thread, `Some(Some(_))` = attached, `None` = no row (released / never claimed).
    fn peek(db: &Database, outbox_id: i64) -> Option<Option<String>> {
        db.with_conn(|conn| {
            conn.query_row(
                "SELECT thread_id FROM outbox_review_claim WHERE outbox_id = ?1",
                rusqlite::params![outbox_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .map_or(Ok(None), |t| Ok(Some(t)))
        })
        .unwrap_or_else(|e| panic!("peek(outbox_id={outbox_id}): {}", e.message))
    }

    /// Test-only row count for an `outbox_id` (0 or 1 — the PK makes >1 unexpressible). Lets the
    /// engine-failure / double-attach tests assert "no duplicate row inserted" directly.
    fn row_count(db: &Database, outbox_id: i64) -> i64 {
        db.with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM outbox_review_claim WHERE outbox_id = ?1",
                rusqlite::params![outbox_id],
                |r| r.get::<_, i64>(0),
            )
        })
        .unwrap_or_else(|e| panic!("row_count(outbox_id={outbox_id}): {}", e.message))
    }

    /// Open a fresh in-memory DB and seed the PARENT `action_outbox` rows for `ids` so the v7 FK
    /// (`outbox_id → action_outbox(id) ON DELETE CASCADE`, AB#1204 F4) is satisfied when a test
    /// `begin_claim`s those ids. In production the parent row ALWAYS exists first: the executor only
    /// reaches `start_for_outbox` for a row `claim_due` already returned, so a claim never references
    /// a missing outbox row. These tests must mirror that ordering now that the FK enforces it.
    fn db_with_outbox_rows(ids: &[i64]) -> Database {
        let db = Database::open_in_memory().expect("open");
        db.with_conn(|conn| {
            for id in ids {
                conn.execute(
                    "INSERT INTO action_outbox \
                     (id, project_id, kind, summary, payload, status, next_attempt_at, created_at, updated_at) \
                     VALUES (?1, 'p1', 'review', 's', '{}', 'pending', 0, 0, 0)",
                    rusqlite::params![id],
                )?;
            }
            Ok(())
        })
        .expect("seed action_outbox parent rows");
        db
    }

    /// Round-trip + the **Hard** PK-idempotency carrier: a fresh `begin_claim` returns `None`
    /// (NULL thread), `attach_thread` fills it, and a SECOND `begin_claim` for the SAME `outbox_id`
    /// does NOT insert a duplicate (PK conflict → `DO NOTHING`) and returns the attached thread.
    #[test]
    fn begin_claim_is_idempotent_and_round_trips_thread_id() {
        let db = db_with_outbox_rows(&[42]);

        // Fresh claim: row inserted, thread_id NULL → None (caller starts the review).
        assert_eq!(
            begin_claim(&db, 42, "p1", 7, "review").expect("begin"),
            None,
            "a fresh claim has no thread yet"
        );

        attach_thread(&db, 42, "thread-abc").expect("attach");

        // Replay: the SAME outbox_id is a PK conflict (no dup row) and surfaces the attached thread.
        assert_eq!(
            begin_claim(&db, 42, "p1", 7, "review").expect("re-begin"),
            Some("thread-abc".to_string()),
            "a replayed claim returns its attached thread_id"
        );
        assert_eq!(peek(&db, 42), Some(Some("thread-abc".to_string())));
    }

    /// A different outbox row claims independently (the key is the row id, not `(project,pr,kind)`)
    /// — this is what lets a new commit's review (a new outbox row) run even though a prior row for
    /// the same `(project,pr,kind)` was claimed.
    #[test]
    fn claims_are_per_outbox_row() {
        let db = db_with_outbox_rows(&[1, 2]);
        assert_eq!(begin_claim(&db, 1, "p1", 7, "review").expect("c1"), None);
        // Same (project,pr,kind) but a DIFFERENT outbox row → a fresh, independent claim.
        assert_eq!(begin_claim(&db, 2, "p1", 7, "review").expect("c2"), None);
    }

    /// `release_claim` deletes the row (table hygiene) and is idempotent on a missing row.
    #[test]
    fn release_claim_deletes_and_is_idempotent() {
        let db = db_with_outbox_rows(&[9]);
        begin_claim(&db, 9, "p1", 3, "check").expect("begin");
        assert!(peek(&db, 9).is_some(), "claimed");

        release_claim(&db, 9).expect("release");
        assert_eq!(peek(&db, 9), None, "released");

        // Idempotent: releasing an absent claim is a no-op success.
        release_claim(&db, 9).expect("release-again");
    }

    /// F2 (AB#1204): engine-start FAILS after `begin_claim` — i.e. `attach_thread` is never called,
    /// so the claim row stays present with a NULL `thread_id`. A subsequent replay of the SAME row
    /// must STILL be allowed to start the review (the durable guard must not degrade at-least-once
    /// into "claimed once, then never retried"). The claim row is RETAINED (not deleted) and its
    /// `thread_id` is still NULL, so the replay's `begin_claim` returns `None` ⇒ the caller starts.
    /// Asserts the row is not duplicated and `thread_id` is still NULL across the replay.
    #[test]
    fn begin_claim_retains_null_thread_when_engine_failed_so_replay_restarts() {
        let db = db_with_outbox_rows(&[7]);

        // First attempt: fresh claim → None (caller would start the review).
        assert_eq!(begin_claim(&db, 7, "p1", 3, "review").expect("begin"), None);
        // Simulate `engine.start` FAILING: `attach_thread` is NOT called, so the breadcrumb stays
        // NULL. (The real path returns the start error to the outbox worker, which retries the row.)
        assert_eq!(peek(&db, 7), Some(None), "claim retained with NULL thread");
        assert_eq!(row_count(&db, 7), 1, "exactly one claim row");

        // Replay of the SAME outbox row: still None (NULL thread ⇒ no review ran yet) → restartable.
        assert_eq!(
            begin_claim(&db, 7, "p1", 3, "review").expect("re-begin"),
            None,
            "a claim that never attached a thread (engine failed) is restartable on replay"
        );
        assert_eq!(
            row_count(&db, 7),
            1,
            "replay does NOT insert a duplicate row"
        );
        assert_eq!(
            peek(&db, 7),
            Some(None),
            "thread_id is still NULL after replay"
        );
    }

    /// F6 (AB#1204): `attach_thread` is idempotent under a double call with the SAME `thread_id`
    /// (a crash-replay can re-reach `thread/start` and re-record the breadcrumb). The second
    /// `attach_thread` neither errors nor inserts a row; the stored `thread_id` is unchanged.
    #[test]
    fn attach_thread_is_idempotent_on_repeat() {
        let db = db_with_outbox_rows(&[11]);
        begin_claim(&db, 11, "p1", 5, "check").expect("begin");

        attach_thread(&db, 11, "t-1").expect("attach");
        attach_thread(&db, 11, "t-1").expect("attach-again");

        assert_eq!(
            peek(&db, 11),
            Some(Some("t-1".to_string())),
            "thread_id unchanged"
        );
        assert_eq!(
            row_count(&db, 11),
            1,
            "double attach inserts no duplicate row"
        );
    }

    /// F1 (AB#1204) claim-layer coverage of the BREADCRUMB-AT-THREAD-START forward placement. The
    /// engine now calls `attach_thread` INSIDE `start` — right after `thread/start` yields a stable
    /// thread id and the `Starting` session is persisted, but BEFORE `start_turn` runs the turn /
    /// posts a `pm:` comment. So the durable claim already carries the thread BEFORE the turn could
    /// have posted: a crash anywhere in the turn window then finds the breadcrumb on replay (rather
    /// than NULL → "never started" → duplicate). The full async engine flow needs a live codex
    /// app-server / `claude` subprocess (not constructible in a unit test), so this pins the
    /// claim-layer invariant the forward placement relies on: the SAME `attach_thread` the engine
    /// now invokes pre-turn makes the breadcrumb visible to a subsequent `begin_claim` (the replay
    /// path). Asserting the attach is visible before any "turn ran" step is the unit-testable core of
    /// the window closure; the engine-level ordering (attach BEFORE `start_turn`/`set_running`) is
    /// pinned by the placement + comments in `session.rs` / `engines/claude/engine.rs`.
    #[test]
    fn breadcrumb_attached_pre_turn_is_visible_to_replay() {
        let db = db_with_outbox_rows(&[31]);
        // Engine `start` sequence on the outbox path: begin_claim (fresh → None) THEN attach_thread,
        // both BEFORE the turn runs.
        assert_eq!(
            begin_claim(&db, 31, "p1", 7, "review").expect("begin"),
            None,
            "fresh claim has no thread yet (engine is about to start the turn)"
        );
        attach_thread(&db, 31, "thread-pre-turn").expect("attach pre-turn");
        // The breadcrumb is durable BEFORE the turn could post a `pm:` comment, so it is already
        // present at the moment a turn-window crash could strike.
        assert_eq!(
            peek(&db, 31),
            Some(Some("thread-pre-turn".to_string())),
            "breadcrumb is durable before the turn runs"
        );
        // A crash-replay re-enters `begin_claim` for the SAME outbox row and now SEES the thread,
        // so the caller resolves the prior review instead of starting a duplicate.
        assert_eq!(
            begin_claim(&db, 31, "p1", 7, "review").expect("re-begin"),
            Some("thread-pre-turn".to_string()),
            "replay resolves the prior review via the pre-turn breadcrumb (no duplicate)"
        );
    }

    /// F2 (AB#1204) claim-layer coverage of the DEAD-RETAINS-CLAIM contract. The outbox service now
    /// releases a claim ONLY on `Done`, NOT on `Dead`: a `dead` row can be manually re-queued
    /// (`store::reset_for_retry`) back to `pending` → it re-enters `start_for_outbox`, where the
    /// RETAINED claim's breadcrumb suppresses a duplicate review (if the prior review already
    /// started/posted). This pins the claim-layer half: after the engine recorded a thread (review
    /// started) and the row later dead-lettered WITHOUT a release, the claim is still present and a
    /// subsequent `begin_claim` (the manual-retry replay) STILL surfaces the breadcrumb so the caller
    /// can resolve-and-suppress. (The "Dead does not call release" service decision is pinned by
    /// `service.rs`'s `matches!(outcome, Outcome::Done)`; this is the durable consequence it relies on.)
    #[test]
    fn dead_terminal_retains_claim_breadcrumb_for_manual_retry() {
        let db = db_with_outbox_rows(&[51]);
        // Review started: claim attached a thread (the engine's pre-turn breadcrumb, F1).
        begin_claim(&db, 51, "p1", 7, "review").expect("begin");
        attach_thread(&db, 51, "thread-dead").expect("attach");

        // The row dead-letters. Per F2 the service does NOT release the claim on `Dead`, so it
        // stays present with its breadcrumb.
        assert_eq!(
            peek(&db, 51),
            Some(Some("thread-dead".to_string())),
            "Dead terminal retains the claim (no release) — breadcrumb survives for manual retry"
        );

        // Manual `reset_for_retry` re-queues the dead row → it re-enters `start_for_outbox`, whose
        // `begin_claim` now returns the retained thread so the caller resolves-and-suppresses
        // instead of duplicating the review.
        assert_eq!(
            begin_claim(&db, 51, "p1", 7, "review").expect("re-begin after manual retry"),
            Some("thread-dead".to_string()),
            "manual retry of a dead row resolves the prior review via the retained breadcrumb"
        );
    }

    /// F5 (AB#1204) claim-layer coverage of the `Deduped` link: when `start_for_outbox` takes the
    /// `Deduped` branch it does NOT call `attach_thread`, so the claim row stays with a NULL
    /// `thread_id` (expected — an existing in-flight session needs no new thread). When the outbox
    /// row terminalizes, `release_claim` must clean up that NULL-thread claim normally. This pins
    /// the claim-store half of the Deduped→done→release chain (the full outbox→engine flow is
    /// exercised by the outbox service tests; see the comment in `commands.rs::start_for_outbox`).
    #[test]
    fn release_claim_cleans_up_null_thread_claim() {
        let db = db_with_outbox_rows(&[21]);
        // A Deduped path leaves the claim with thread_id NULL (no `attach_thread`).
        begin_claim(&db, 21, "p1", 9, "review").expect("begin");
        assert_eq!(peek(&db, 21), Some(None), "Deduped claim has NULL thread");

        release_claim(&db, 21).expect("release");
        assert_eq!(
            peek(&db, 21),
            None,
            "NULL-thread claim is released normally"
        );
    }
}
