//! Dispatch de-duplication ledger + cooldown source.
//!
//! Port of `router.py`'s two state files (`dispatched` keys + `dispatch-events`
//! epochs), persisted in the unified SQLite store (#70) — the `dispatch_key` (dedup
//! set) and `dispatch_event` (cooldown log) tables, each partitioned by a real
//! `project_id` column (replacing the old `ledger.json` `prefix:{pid}` store keys).
//!
//! PR3 uses the **read** path (`has_dispatched` / `last_dispatch_at`) to annotate
//! the PR list with "already dispatched" / cooldown skip reasons. The **write**
//! path (`record_many`) is invoked by the auto-trigger dispatcher
//! ([`crate::dispatch`]) once review turns actually start; recording it here keeps
//! the dedup machinery complete. The write is batched (one transaction for the whole
//! cycle's started candidates) so unbounded concurrent starts can't race the store.

use std::collections::HashSet;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::db::{map_err, Database};
use crate::error::AppResult;
use crate::model::Candidate;

/// Serializes EVERY load→stage→save of the dispatch ledger across projects (#35). Two
/// parallel project cycles each doing a load→stage→save would interleave and one would
/// clobber the other's just-recorded partition (a lost dispatch record → re-review
/// storm). A process-global `Mutex<()>` (the data lives in SQLite, not behind the lock)
/// guards the critical section in [`Ledger::record_many`]; a module static so the lock
/// IDENTITY is fixed (a caller cannot serialize on the wrong mutex). Mirrors the
/// registry's `WRITE_LOCK` rationale. `std` (not `tokio`) `Mutex`: the guarded section
/// is fully synchronous (the SQLite calls never `.await`), so no `.await` is held across
/// the guard. The single SQLite connection's own mutex additionally serializes
/// individual statements, but THIS lock is what makes a project's load→save one atomic
/// critical section relative to other projects' cycles (so a `load` here can't read a
/// partition another project is mid-rewrite of).
static LEDGER_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// One recorded dispatch — the cooldown source (mirrors `router.py`
/// dispatch-events: `(pr, kind, dispatchedAtEpoch)`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchEvent {
    pub pr: u64,
    pub kind: String,
    pub head_sha: String,
    pub key: String,
    pub dispatched_at_epoch: u64,
}

/// In-memory snapshot of the dedup ledger + cooldown events.
#[derive(Debug, Default)]
pub struct Ledger {
    pub(crate) dispatched: HashSet<String>,
    pub(crate) events: Vec<DispatchEvent>,
}

/// `{number}@{head_sha}:{kind}` — the per-(pr, head, kind) dedup key
/// (`router.py` `Candidate.key`). Re-dispatch is suppressed once this key is in
/// the ledger, so a force-push (new head_sha) is a fresh key and *can* dispatch.
pub fn dispatch_key(number: u64, head_sha: &str, kind: &str) -> String {
    format!("{number}@{head_sha}:{kind}")
}

/// Wall-clock seconds since the Unix epoch (the cooldown / dispatch clock). A
/// pre-epoch system clock degrades to 0 rather than panicking. `pub(crate)` so
/// both the discovery gating and the dispatch landing ([`record_dispatched`])
/// stamp the ledger with the same clock.
pub(crate) fn now_epoch() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load the ledger, batch-record the started candidates at one epoch, and persist
/// — the dispatch-time landing in ONE call, scoped to `project_id` (#35). Stamps the
/// clock internally so callers (the dispatcher, [`crate::dispatch`]) pass only the
/// candidates that started; the load + stage + persist + clock all stay in the pr
/// slice. The load→stage→save runs under [`LEDGER_WRITE_LOCK`] (in
/// [`Ledger::record_many`]) so parallel project cycles can't clobber each other's
/// whole-file rewrite.
pub fn record_dispatched<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: &str,
    cands: &[Candidate],
) -> AppResult<()> {
    // Hold the cross-project write lock across the WHOLE load→stage→save (#35): N
    // parallel project cycles each replace their `dispatch_*` partition (delete +
    // re-insert), so a load here racing another project's save would drop that project's
    // just-recorded partition. The guard makes load + persist one atomic section.
    // `.unwrap()` matches the registry's std-Mutex convention; the section is
    // synchronous (the SQLite calls never `.await`) so no `.await` is held across it, and
    // a panic mid-section can't leave torn state (the data lives in SQLite, the partition
    // rewritten wholesale by `record_many` in one transaction). Poisoning is benign.
    let _guard = LEDGER_WRITE_LOCK.lock().unwrap();
    let mut ledger = Ledger::load(app, project_id)?;
    ledger.record_many(app, project_id, cands, now_epoch())
}

/// Remaining cooldown seconds when `last` is within `secs` of `now`, else `None`
/// (cooldown elapsed). `saturating_sub` so a backwards clock (`last > now`) reads
/// as age 0 rather than underflowing.
pub fn cooldown_remaining(now: u64, last: u64, secs: u64) -> Option<u64> {
    let age = now.saturating_sub(last);
    if age < secs {
        Some(secs - age)
    } else {
        None
    }
}

impl Ledger {
    /// Loads `project_id`'s partition of the persisted ledger (#35), defaulting to
    /// empty when nothing is stored or a value is corrupt (a corrupt ledger must
    /// never block discovery — the worst case is a duplicate dispatch, which the
    /// in-process registry guard then drops for any still-active session). Reads only
    /// this project's rows (`WHERE project_id = ?1` on `dispatch_key` / `dispatch_event`),
    /// so a `has_dispatched` / cooldown check for one project never sees another's records.
    ///
    /// **Lock-free read (intentional).** The discovery path (`commands::discover`) and
    /// the webhook ingest (`commands::ingest_webhook` → `webhook_view`) call this OUTSIDE
    /// [`LEDGER_WRITE_LOCK`]; a load runs two SELECTs inside one `with_conn` closure (the
    /// connection mutex held across both — no torn read)
    /// and a stale-by-one-round snapshot is acceptable because it only gates an
    /// OPTIMIZATION — the real double-dispatch backstop is the session registry's
    /// `try_reserve_pair` atomic test-and-set at start time. A read racing a concurrent
    /// write at worst lets one extra candidate through the cooldown/dedup gate, which
    /// the reservation then rejects. The write path ([`record_dispatched`]) DOES hold
    /// the lock across its own load→stage→save (a lost write there is unrecoverable).
    pub fn load<R: tauri::Runtime>(app: &tauri::AppHandle<R>, project_id: &str) -> AppResult<Self> {
        Self::load_db(app.state::<Database>().inner(), project_id)
    }

    /// SQLite-level load (no Tauri app) — reads this project's `dispatch_key` set +
    /// `dispatch_event` log. Split from [`Self::load`] so store round-trips are
    /// testable against an in-memory [`Database`]. Epochs/PR numbers are stored as
    /// `i64` (SQLite's only integer type) and read back as `u64`.
    pub(crate) fn load_db(db: &Database, project_id: &str) -> AppResult<Self> {
        db.with_conn(|conn| {
            let mut dispatched = HashSet::new();
            let mut stmt = conn.prepare("SELECT key FROM dispatch_key WHERE project_id = ?1")?;
            let rows = stmt.query_map([project_id], |r| r.get::<_, String>(0))?;
            for k in rows {
                dispatched.insert(k?);
            }

            let mut events = Vec::new();
            let mut stmt = conn.prepare(
                "SELECT pr, kind, head_sha, key, dispatched_at_epoch \
                 FROM dispatch_event WHERE project_id = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map([project_id], |r| {
                Ok(DispatchEvent {
                    pr: r.get::<_, i64>(0)? as u64,
                    kind: r.get(1)?,
                    head_sha: r.get(2)?,
                    key: r.get(3)?,
                    dispatched_at_epoch: r.get::<_, i64>(4)? as u64,
                })
            })?;
            for e in rows {
                events.push(e?);
            }

            Ok(Self { dispatched, events })
        })
    }

    /// Whether `key` has already been dispatched.
    pub fn has_dispatched(&self, key: &str) -> bool {
        self.dispatched.contains(key)
    }

    /// Most-recent dispatch epoch for `(pr, kind)`, or `None`. `router.py` scans
    /// dispatch-events in reverse and takes the first match (= most recent); the
    /// `max` here is order-independent and equivalent.
    pub fn last_dispatch_at(&self, pr: u64, kind: &str) -> Option<u64> {
        self.events
            .iter()
            .filter(|e| e.pr == pr && e.kind == kind)
            .map(|e| e.dispatched_at_epoch)
            .max()
    }

    /// Records a batch of dispatches (key + event per candidate) into `project_id`'s
    /// partition and persists **once**. Invoked by the auto-trigger dispatcher
    /// ([`crate::dispatch`]) via [`record_dispatched`] after a poll cycle's reviews
    /// have started; PR discovery itself never dispatches.
    ///
    /// The single-persist shape matters under unbounded concurrent starts: staging
    /// every candidate's key/event in memory and saving the store one time avoids
    /// the interleaved store writes (and redundant saves) that per-candidate
    /// `record` calls would produce. An empty `cands` slice still touches the store
    /// (a harmless no-op save) — callers gate on non-empty before calling.
    ///
    /// **Concurrency (#35):** writes ONLY this project's `dispatch_key` /
    /// `dispatch_event` rows, but does so by replacing the whole partition (delete +
    /// re-insert `self`). The cross-project lost-update race that the read→mutate→write
    /// shape creates is closed by [`record_dispatched`], which holds [`LEDGER_WRITE_LOCK`]
    /// across its `load` → this `record_many`, so the load this method's `self` came from
    /// and the save below are one atomic critical section relative to other projects'
    /// cycles. The delete+insert runs in one transaction (atomic on its own too).
    pub fn record_many<R: tauri::Runtime>(
        &mut self,
        app: &tauri::AppHandle<R>,
        project_id: &str,
        cands: &[Candidate],
        epoch: u64,
    ) -> AppResult<()> {
        self.stage_all(cands, epoch);
        self.save_db(app.state::<Database>().inner(), project_id)
    }

    /// SQLite-level save (no Tauri app) — replaces this project's partition with the
    /// full in-memory `self` (delete-all + insert-all, replacing the old tauri-plugin-store
    /// `Store::set`). Split from [`Self::record_many`] so the round-trip
    /// is testable against an in-memory [`Database`].
    pub(crate) fn save_db(&self, db: &Database, project_id: &str) -> AppResult<()> {
        db.with_tx(|tx| {
            tx.execute(
                "DELETE FROM dispatch_key WHERE project_id = ?1",
                [project_id],
            )
            .map_err(map_err)?;
            tx.execute(
                "DELETE FROM dispatch_event WHERE project_id = ?1",
                [project_id],
            )
            .map_err(map_err)?;
            {
                let mut stmt = tx
                    .prepare("INSERT INTO dispatch_key (project_id, key) VALUES (?1, ?2)")
                    .map_err(map_err)?;
                for k in &self.dispatched {
                    stmt.execute(rusqlite::params![project_id, k])
                        .map_err(map_err)?;
                }
            }
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO dispatch_event \
                         (project_id, pr, kind, head_sha, key, dispatched_at_epoch) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    )
                    .map_err(map_err)?;
                for e in &self.events {
                    stmt.execute(rusqlite::params![
                        project_id,
                        e.pr as i64,
                        e.kind,
                        e.head_sha,
                        e.key,
                        e.dispatched_at_epoch as i64
                    ])
                    .map_err(map_err)?;
                }
            }
            Ok(())
        })
    }

    /// Stages a batch into the in-memory ledger (the dedup key set + cooldown event
    /// log) without persisting. Split out so the staging — what `record_many`
    /// actually writes to the store — is unit-testable without a Tauri app
    /// (the persistence itself goes through `save_db` → `db.with_tx`).
    fn stage_all(&mut self, cands: &[Candidate], epoch: u64) {
        for cand in cands {
            let key = dispatch_key(cand.number, &cand.head_sha, &cand.kind);
            self.dispatched.insert(key.clone());
            self.events.push(DispatchEvent {
                pr: cand.number,
                kind: cand.kind.clone(),
                head_sha: cand.head_sha.clone(),
                key,
                dispatched_at_epoch: epoch,
            });
        }
    }
}

/// One-time legacy import (#70) of a project's `dispatched:{pid}` dedup-key set from the
/// old `ledger.json`. Parses the JSON array of keys and inserts `dispatch_key` rows.
/// Lenient: a corrupt value imports nothing (parity with `load`'s `unwrap_or_default`).
/// Runs inside the composition root's import transaction (see `lib::import_legacy_stores`).
pub fn import_legacy_dispatched(
    tx: &rusqlite::Transaction,
    project_id: &str,
    value: &serde_json::Value,
) -> AppResult<()> {
    let keys: HashSet<String> = serde_json::from_value(value.clone()).unwrap_or_default();
    let mut stmt = tx
        .prepare("INSERT OR IGNORE INTO dispatch_key (project_id, key) VALUES (?1, ?2)")
        .map_err(map_err)?;
    for k in &keys {
        stmt.execute(rusqlite::params![project_id, k])
            .map_err(map_err)?;
    }
    Ok(())
}

/// One-time legacy import (#70) of a project's `events:{pid}` cooldown log from the old
/// `ledger.json`. Parses the JSON array of [`DispatchEvent`] and inserts `dispatch_event`
/// rows (order preserved by insert order → `id`). Lenient like [`import_legacy_dispatched`].
pub fn import_legacy_events(
    tx: &rusqlite::Transaction,
    project_id: &str,
    value: &serde_json::Value,
) -> AppResult<()> {
    let events: Vec<DispatchEvent> = serde_json::from_value(value.clone()).unwrap_or_default();
    let mut stmt = tx
        .prepare(
            "INSERT INTO dispatch_event \
             (project_id, pr, kind, head_sha, key, dispatched_at_epoch) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .map_err(map_err)?;
    for e in &events {
        stmt.execute(rusqlite::params![
            project_id,
            e.pr as i64,
            e.kind,
            e.head_sha,
            e.key,
            e.dispatched_at_epoch as i64
        ])
        .map_err(map_err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(pr: u64, kind: &str, epoch: u64) -> DispatchEvent {
        DispatchEvent {
            pr,
            kind: kind.to_string(),
            head_sha: "sha".to_string(),
            key: dispatch_key(pr, "sha", kind),
            dispatched_at_epoch: epoch,
        }
    }

    fn cand(pr: u64, kind: &str) -> Candidate {
        Candidate {
            number: pr,
            head_sha: "sha".to_string(),
            head_ref: "ref".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            kind: kind.to_string(),
        }
    }

    // `record_many` persists via `save_db` → `db.with_tx`, which needs a `tauri::AppHandle`
    // to resolve the `Database` state; the
    // batch's data effect is `stage_all`, which is what gets serialized. This
    // round-trip asserts staging a batch records every candidate's dedup key and
    // a cooldown event per candidate at the shared epoch (the persisted shape).
    #[test]
    fn record_many_stages_every_candidate_key_and_event() {
        let mut ledger = Ledger::default();
        let cands = [cand(12, "review"), cand(12, "check"), cand(13, "review")];
        ledger.stage_all(&cands, 1_700_000_000);

        // One dedup key per candidate (distinct (pr, head, kind) tuples).
        assert!(ledger.has_dispatched(&dispatch_key(12, "sha", "review")));
        assert!(ledger.has_dispatched(&dispatch_key(12, "sha", "check")));
        assert!(ledger.has_dispatched(&dispatch_key(13, "sha", "review")));
        assert_eq!(ledger.dispatched.len(), 3);

        // One cooldown event per candidate, all at the shared epoch.
        assert_eq!(ledger.events.len(), 3);
        assert_eq!(ledger.last_dispatch_at(12, "review"), Some(1_700_000_000));
        assert_eq!(ledger.last_dispatch_at(12, "check"), Some(1_700_000_000));
        assert_eq!(ledger.last_dispatch_at(13, "review"), Some(1_700_000_000));
    }

    #[test]
    fn record_many_empty_batch_is_a_noop_stage() {
        let mut ledger = Ledger::default();
        ledger.stage_all(&[], 1_000);
        assert!(ledger.dispatched.is_empty());
        assert!(ledger.events.is_empty());
    }

    // Staging the same candidate twice (e.g. two cycles before its head moves)
    // documents the dedup-set vs event-log split: the `dispatched` key set is
    // idempotent (one key), while the cooldown event log appends each time (so
    // `last_dispatch_at` always tracks the most recent stamp).
    #[test]
    fn stage_all_repeat_call_dedups_key_but_appends_event() {
        let mut ledger = Ledger::default();
        let c = [cand(12, "review")];
        ledger.stage_all(&c, 1_000);
        ledger.stage_all(&c, 2_000);

        assert_eq!(ledger.dispatched.len(), 1); // same key deduped in the set.
        assert_eq!(ledger.events.len(), 2); // each stage appends a cooldown event.
        assert_eq!(ledger.last_dispatch_at(12, "review"), Some(2_000)); // most recent.
    }

    #[test]
    fn dispatch_key_format() {
        assert_eq!(dispatch_key(42, "abc123", "review"), "42@abc123:review");
        assert_eq!(dispatch_key(7, "deadbeef", "check"), "7@deadbeef:check");
    }

    // SQLite store round-trip (#70, Medium carrier): staging a batch, `save_db` then
    // `load_db` against an in-memory DB must round-trip the dedup set + cooldown log
    // intact (a column/SQL drift surfaces here), and a different project's partition
    // must read empty (the `project_id` column is the partitioning seam that replaced
    // the old `dispatched:{pid}` / `events:{pid}` store keys).
    #[test]
    fn sqlite_round_trip_and_per_project_isolation() {
        let db = Database::open_in_memory().expect("open db");
        let mut ledger = Ledger::default();
        ledger.stage_all(&[cand(12, "review"), cand(12, "check")], 1_700_000_000);
        ledger.save_db(&db, "alpha").expect("save");

        let back = Ledger::load_db(&db, "alpha").expect("load");
        assert!(back.has_dispatched(&dispatch_key(12, "sha", "review")));
        assert!(back.has_dispatched(&dispatch_key(12, "sha", "check")));
        assert_eq!(back.events.len(), 2);
        assert_eq!(back.last_dispatch_at(12, "review"), Some(1_700_000_000));

        // A different project's partition is empty — same (number, head, kind) is not
        // visible across projects.
        let other = Ledger::load_db(&db, "beta").expect("load other");
        assert!(other.dispatched.is_empty());
        assert!(other.events.is_empty());
    }

    // `save_db` replaces the whole partition (delete + re-insert `self`), so a later
    // save with FEWER rows shrinks the stored set rather than leaving orphans.
    #[test]
    fn save_db_replaces_partition() {
        let db = Database::open_in_memory().expect("open db");
        let mut full = Ledger::default();
        full.stage_all(&[cand(1, "review"), cand(2, "review")], 1_000);
        full.save_db(&db, "p").expect("save full");

        let mut fewer = Ledger::default();
        fewer.stage_all(&[cand(1, "review")], 1_000);
        fewer.save_db(&db, "p").expect("save fewer");

        let back = Ledger::load_db(&db, "p").expect("load");
        assert_eq!(back.dispatched.len(), 1);
        assert!(back.has_dispatched(&dispatch_key(1, "sha", "review")));
        assert!(!back.has_dispatched(&dispatch_key(2, "sha", "review")));
    }

    // One-time legacy import (#70): the old `ledger.json` shapes (a JSON array of dedup
    // keys, a JSON array of `DispatchEvent`) import into the SQLite partition and read
    // back through `load_db`. Guards the migration path existing users rely on.
    #[test]
    fn legacy_import_round_trips_through_load() {
        let db = Database::open_in_memory().expect("open db");
        let dispatched_v = serde_json::json!(["12@sha:review", "13@sha:check"]);
        let events_v = serde_json::to_value(vec![
            event(12, "review", 1_700_000_000),
            event(13, "check", 1_700_000_100),
        ])
        .expect("events serialize");

        db.with_tx(|tx| {
            import_legacy_dispatched(tx, "alpha", &dispatched_v)?;
            import_legacy_events(tx, "alpha", &events_v)?;
            Ok(())
        })
        .expect("import");

        let back = Ledger::load_db(&db, "alpha").expect("load");
        assert!(back.has_dispatched("12@sha:review"));
        assert!(back.has_dispatched("13@sha:check"));
        assert_eq!(back.last_dispatch_at(12, "review"), Some(1_700_000_000));
        assert_eq!(back.last_dispatch_at(13, "check"), Some(1_700_000_100));
    }

    // Ledger isolation (#35): two projects whose dedup sets are loaded from distinct
    // store-key partitions do NOT collide even when an identical (number, head, kind)
    // candidate was dispatched in one. `Ledger::load` is the partitioning seam (it
    // queries `WHERE project_id = ?1` on `dispatch_key`); here we simulate the two loaded
    // partitions directly (the live `load` needs a `tauri::AppHandle` to resolve the
    // `Database` state) and assert `has_dispatched` is true for
    // the project that recorded it and false for the other — the dedup gate is
    // per-project, so PR #1@sha:review reviewed under project A is still dispatchable
    // under project B.
    #[test]
    fn dedup_does_not_collide_across_projects() {
        let key = dispatch_key(1, "sha", "review");

        // Project A staged the candidate; project B's partition is empty.
        let mut ledger_a = Ledger::default();
        ledger_a.stage_all(&[cand(1, "review")], 1_000);
        let ledger_b = Ledger::default();

        assert!(
            ledger_a.has_dispatched(&key),
            "project A recorded the dispatch"
        );
        assert!(
            !ledger_b.has_dispatched(&key),
            "the SAME (number, head, kind) must NOT read as dispatched under project B"
        );
    }

    #[test]
    fn cooldown_remaining_within_window() {
        // 100s cooldown, dispatched 30s ago → 70s remaining.
        assert_eq!(cooldown_remaining(1_000, 970, 100), Some(70));
    }

    #[test]
    fn cooldown_remaining_elapsed() {
        // dispatched exactly `secs` ago (boundary) and beyond → elapsed.
        assert_eq!(cooldown_remaining(1_000, 900, 100), None);
        assert_eq!(cooldown_remaining(1_000, 800, 100), None);
    }

    #[test]
    fn cooldown_remaining_clock_skew_is_saturating() {
        // last in the future → age saturates to 0 → full window remaining.
        assert_eq!(cooldown_remaining(900, 1_000, 100), Some(100));
    }

    #[test]
    fn has_dispatched_matches_recorded_key() {
        let ledger = Ledger {
            dispatched: HashSet::from(["12@abc:review".to_string()]),
            events: vec![],
        };
        assert!(ledger.has_dispatched("12@abc:review"));
        assert!(!ledger.has_dispatched("12@abc:check"));
        assert!(!ledger.has_dispatched("13@abc:review"));
    }

    #[test]
    fn last_dispatch_at_takes_most_recent_matching() {
        let ledger = Ledger {
            dispatched: HashSet::new(),
            events: vec![
                event(12, "review", 100),
                event(12, "review", 300), // most recent for (12, review)
                event(12, "check", 200),
                event(13, "review", 500),
            ],
        };
        assert_eq!(ledger.last_dispatch_at(12, "review"), Some(300));
        assert_eq!(ledger.last_dispatch_at(12, "check"), Some(200));
        assert_eq!(ledger.last_dispatch_at(13, "review"), Some(500));
        assert_eq!(ledger.last_dispatch_at(99, "review"), None);
    }

    // Wire-shape lock for the persisted `ledger.json` events (Medium carrier per
    // ai-robust.md). A field rename would make `Ledger::load` silently drop the
    // events (deserialize → `unwrap_or_default()`), wiping cooldown state and
    // re-dispatching; this round-trip guards against that.
    #[test]
    fn dispatch_event_wire_shape_is_camel_case_and_round_trips() {
        let e = event(12, "review", 1_700_000_000);
        let v = serde_json::to_value(&e).expect("DispatchEvent serializes");

        // camelCase keys present.
        assert!(v.get("pr").is_some());
        assert!(v.get("kind").is_some());
        assert!(v.get("headSha").is_some());
        assert!(v.get("key").is_some());
        assert!(v.get("dispatchedAtEpoch").is_some());

        // snake_case forms absent — a rename surfaces here.
        assert!(v.get("head_sha").is_none());
        assert!(v.get("dispatched_at_epoch").is_none());

        // Round-trips without zeroing the cooldown epoch.
        let back: DispatchEvent = serde_json::from_value(v).expect("round-trips");
        assert_eq!(back.dispatched_at_epoch, 1_700_000_000);
        assert_eq!(back.head_sha, "sha");
    }
}
