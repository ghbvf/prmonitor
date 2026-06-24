//! Outbox enqueue + worker logic (AB#1066): the producer-facing `enqueue` and the worker body the
//! manager's loop drives.
//!
//! **Decoupled from `review` / `pr`.** This module names NO foreign-slice type. The side effect is
//! performed by the OPAQUE composition-root-injected [`crate::outbox::ActionExecutor`] closure
//! (which alone, in `lib.rs`, knows `review::notify`); here we only sequence the durable claim →
//! execute → record-outcome around it.
//!
//! **Retry / dead-letter (the acceptance).** A claimed action runs through the executor; on `Ok` it
//! is `done`, on `Err` the worker bumps `attempt_count` and either reschedules it (still `pending`,
//! with exponential backoff) or — once the attempt budget is spent — flips it to the terminal `dead`
//! (the dead-letter). The DECISION is the pure [`decide_outcome`] / [`next_backoff`] (AppHandle-free,
//! unit-tested without a Tauri runtime, mirroring `inbox::service::record_terminal`); [`run_due_once`]
//! only sequences the IO around it.

use tauri::{Emitter, Manager, Runtime};

use crate::db::Database;
use crate::error::AppResult;
use crate::events::{OutboxEvent, OUTBOX_UPDATED_EVENT};
use crate::model::ActionKind;
use crate::outbox::{store, ActionExecutor};
use crate::state::AppState;

/// Max execution attempts before an action dead-letters (AB#1066). After this many failed attempts
/// the worker flips the row to terminal `dead` rather than rescheduling it again.
const MAX_ATTEMPTS: u32 = 5;

/// Backoff base (seconds) for the first retry (AB#1066); each subsequent retry doubles it.
const BACKOFF_BASE_SECS: u64 = 30;

/// Backoff cap (seconds) (AB#1066): a long-failing action retries at most once per this interval
/// rather than growing the delay unboundedly toward the dead-letter.
const BACKOFF_CAP_SECS: u64 = 3600;

/// Exponential backoff for the `attempt`-th failure (1-based) (AB#1066): `BASE * 2^(attempt-1)`,
/// capped at [`BACKOFF_CAP_SECS`]. `attempt` 1 → 30s, 2 → 60s, 3 → 120s, … Saturating + a bounded
/// shift so a large `attempt` can't overflow (it just pins at the cap).
fn next_backoff(attempt: u32) -> u64 {
    // Shifts ≥ 63 would overflow u64; clamp the exponent — the result pins at the cap anyway.
    let exp = attempt.saturating_sub(1).min(63);
    let delay = BACKOFF_BASE_SECS.saturating_mul(1u64 << exp);
    delay.min(BACKOFF_CAP_SECS)
}

/// What the worker does with a row after an execution attempt (AB#1066). The pure decision, so the
/// retry/dead-letter policy is unit-tested without a DB or AppHandle.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The executor succeeded — mark the row `done`.
    Done,
    /// A transient failure under the attempt budget — reschedule (still `pending`) at this epoch.
    Retry { next_attempt_at: u64 },
    /// The attempt budget is exhausted — dead-letter the row.
    Dead,
}

/// Decide a row's fate after an attempt (AB#1066): `Ok` → [`Outcome::Done`]; an error with attempts
/// left → [`Outcome::Retry`] at `now + next_backoff(new_attempt_count)`; an error at the
/// [`MAX_ATTEMPTS`] budget → [`Outcome::Dead`]. `new_attempt_count` is the count INCLUDING the
/// attempt that just ran (so `MAX_ATTEMPTS` total attempts dead-letter).
fn decide_outcome(new_attempt_count: u32, now: u64, is_err: bool) -> Outcome {
    if !is_err {
        return Outcome::Done;
    }
    if new_attempt_count >= MAX_ATTEMPTS {
        Outcome::Dead
    } else {
        Outcome::Retry {
            next_attempt_at: now.saturating_add(next_backoff(new_attempt_count)),
        }
    }
}

/// Enqueue a produced side effect (AB#1066) — the public producer API. Persists a `pending` row
/// (durable BEFORE the action runs, so it survives a restart), emits `outbox:updated` for the new
/// row, and WAKES the worker so it runs promptly rather than waiting for the next tick. Returns the
/// new row id. Generic over the runtime so any slice's producer (e.g. `review::deeplink`) can call
/// it with its `&AppHandle<R>`.
pub fn enqueue<R: Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: &str,
    kind: ActionKind,
    summary: &str,
    payload: &str,
) -> AppResult<i64> {
    let db = app.state::<Database>();
    let now = store::now_epoch();
    let id = store::enqueue(db.inner(), project_id, kind, summary, payload, now)?;
    announce_updated(app, db.inner(), id);
    app.state::<AppState>().outbox.wake();
    Ok(id)
}

/// Persist one action's execution outcome (AB#1066): given the row's PRIOR `attempt_count` and the
/// executor `result`, bump the count, [`decide_outcome`], and write the matching terminal/retry state
/// (`mark_done` records the succeeding attempt's count too — consistent with the failure paths).
/// Returns the [`Outcome`] applied. AppHandle-free + db-only, so the worker's full claim→execute→record
/// state machine is unit-testable with a fake result (no Tauri runtime) — the IO wrapper
/// [`run_due_once`] only adds the executor call + the emit around it.
fn record_action_result(
    db: &Database,
    id: i64,
    prev_attempt_count: u32,
    now: u64,
    result: &AppResult<()>,
) -> AppResult<Outcome> {
    let new_attempt_count = prev_attempt_count.saturating_add(1);
    let error = result
        .as_ref()
        .err()
        .map(|e| e.message.clone())
        .unwrap_or_default();
    let outcome = decide_outcome(new_attempt_count, now, result.is_err());
    match &outcome {
        Outcome::Done => store::mark_done(db, id, new_attempt_count, now)?,
        Outcome::Retry { next_attempt_at } => {
            store::mark_retry(db, id, new_attempt_count, *next_attempt_at, &error, now)?
        }
        Outcome::Dead => store::mark_dead(db, id, new_attempt_count, &error, now)?,
    }
    Ok(outcome)
}

/// Run ONE worker cycle (AB#1066): claim the due `pending` rows and, for each, execute the injected
/// closure and record the outcome (done / reschedule / dead-letter) via [`record_action_result`],
/// re-emitting `outbox:updated`. Also emits for any rows `claim_due` dead-lettered as corrupt-kind
/// (so an open panel sees that transition). Best-effort throughout — a claim or record error is
/// logged, not propagated (the next tick retries). Concrete [`tauri::AppHandle`] (the executor's
/// signature is concrete, like the inbox's injected hooks).
pub async fn run_due_once(app: &tauri::AppHandle, db: &Database, executor: &ActionExecutor) {
    let (due, quarantined) = match store::claim_due(db, store::now_epoch()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("outbox: claim_due 失败：{}", e.message);
            return;
        }
    };
    // Announce the corrupt-kind rows claim_due just dead-lettered (their `dead` transition).
    for id in quarantined {
        announce_updated(app, db, id);
    }
    for action in due {
        let id = action.id;
        let prev_attempt_count = action.attempt_count;

        // The executor RESULT is authoritative: Ok → done, Err → retry/dead (no false `done`).
        let result = executor(app.clone(), action).await;
        let now = store::now_epoch();
        if let Err(e) = record_action_result(db, id, prev_attempt_count, now, &result) {
            eprintln!("outbox: 记录动作终态失败（id={id}）：{}", e.message);
        }
        announce_updated(app, db, id);
    }
}

/// Look up row `id` and emit `outbox:updated` for it (best-effort). Always re-reads so the emit
/// carries the CURRENT persisted state (post-transition), and routes on the entry's OWN
/// `project_id` (no external threading — unlike the inbox, `OutboxEntry` carries `projectId` at the
/// top level). A gone row (e.g. pruned) is silently skipped; a store ERROR is logged (not swallowed)
/// so a persistent read failure stays diagnosable.
pub(crate) fn announce_updated<R: Runtime>(app: &tauri::AppHandle<R>, db: &Database, id: i64) {
    match store::get_entry(db, id) {
        Ok(Some(entry)) => {
            let _ = app.emit(
                OUTBOX_UPDATED_EVENT,
                &OutboxEvent::Updated {
                    project_id: entry.project_id.clone(),
                    entry,
                },
            );
        }
        Ok(None) => {} // row gone (e.g. pruned) — nothing to emit.
        Err(e) => eprintln!(
            "outbox: 读取条目以发送 outbox:updated 失败（id={id}）：{}",
            e.message
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppError;

    // Exponential backoff is monotonic non-decreasing and capped (AB#1066).
    #[test]
    fn next_backoff_grows_then_caps() {
        // `attempt` is documented 1-based; `0` is never passed in practice (the caller uses
        // `attempt_count + 1`, min 1), but pin that `0` and `1` coincide so a future 0-based refactor
        // can't silently change the first-retry delay.
        assert_eq!(next_backoff(0), 30);
        assert_eq!(next_backoff(1), 30);
        assert_eq!(next_backoff(2), 60);
        assert_eq!(next_backoff(3), 120);
        assert_eq!(next_backoff(4), 240);
        // Large attempt pins at the cap and never overflows (the bounded shift guard).
        assert_eq!(next_backoff(100), BACKOFF_CAP_SECS);
        let mut prev = 0;
        for n in 1..=20 {
            let d = next_backoff(n);
            assert!(d >= prev, "non-decreasing");
            assert!(d <= BACKOFF_CAP_SECS, "capped");
            prev = d;
        }
    }

    // The retry/dead-letter decision table (AB#1066): success → Done regardless of count; an error
    // under the budget → Retry at now+backoff; an error AT the budget → Dead.
    #[test]
    fn decide_outcome_done_retry_dead() {
        // Success is Done no matter the attempt count.
        assert_eq!(decide_outcome(1, 1_000, false), Outcome::Done);
        assert_eq!(decide_outcome(MAX_ATTEMPTS, 1_000, false), Outcome::Done);

        // Error with attempts left → Retry at now + backoff(new_attempt_count).
        assert_eq!(
            decide_outcome(1, 1_000, true),
            Outcome::Retry {
                next_attempt_at: 1_000 + next_backoff(1)
            }
        );
        assert_eq!(
            decide_outcome(MAX_ATTEMPTS - 1, 1_000, true),
            Outcome::Retry {
                next_attempt_at: 1_000 + next_backoff(MAX_ATTEMPTS - 1)
            }
        );

        // Error at the budget → Dead (no further reschedule).
        assert_eq!(decide_outcome(MAX_ATTEMPTS, 1_000, true), Outcome::Dead);
        assert_eq!(decide_outcome(MAX_ATTEMPTS + 1, 1_000, true), Outcome::Dead);
    }

    // End-to-end retry → dead-letter walk over the REAL store (AB#1066 acceptance), driven through
    // the SAME `record_action_result` seam `run_due_once` uses (claim_due → record_action_result),
    // without an AppHandle (the executor result is a fake `Err`). An always-failing action climbs
    // attempt_count across retries (staying `pending`, rescheduled forward) and finally dead-letters
    // at MAX_ATTEMPTS — locking the worker's real record state machine (F5), not just `decide_outcome`.
    #[test]
    fn always_failing_action_retries_then_dead_letters() {
        let db = Database::open_in_memory().expect("open db");
        let id =
            store::enqueue(&db, "p1", ActionKind::Notification, "s", "{}", 0).expect("enqueue");

        let mut now = 0u64;
        let mut retries_seen = 0u32;
        let err: AppResult<()> = Err(AppError::new("always boom"));
        // Simulate the worker draining the row until it dead-letters. Bound the loop well above
        // MAX_ATTEMPTS so a regression (never dead-lettering) fails loudly instead of looping.
        for _ in 0..(MAX_ATTEMPTS + 3) {
            let (due, _q) = store::claim_due(&db, now).expect("claim");
            let Some(action) = due.into_iter().find(|a| a.id == id) else {
                break; // no longer claimable (dead) — stop
            };
            match record_action_result(&db, id, action.attempt_count, now, &err).expect("record") {
                Outcome::Done => unreachable!("the action always fails"),
                Outcome::Retry { next_attempt_at } => {
                    retries_seen += 1;
                    // The increment is exact each step, not just at the terminal state: after N
                    // retries the row reads attempt_count == N (catches an off-by-one in the bump).
                    let mid = store::get_entry(&db, id).expect("get").expect("exists");
                    assert_eq!(
                        mid.status,
                        crate::model::ActionStatus::Pending,
                        "retry stays pending"
                    );
                    assert_eq!(
                        mid.attempt_count, retries_seen,
                        "attempt_count increments by 1/retry"
                    );
                    now = next_attempt_at; // advance the clock to the next schedule
                }
                Outcome::Dead => break,
            }
        }
        // MAX_ATTEMPTS total attempts = (MAX_ATTEMPTS - 1) retries then the final dead-letter.
        assert_eq!(
            retries_seen,
            MAX_ATTEMPTS - 1,
            "retried up to the budget, then dead-lettered"
        );

        let entry = store::get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(
            entry.status,
            crate::model::ActionStatus::Dead,
            "dead-lettered"
        );
        assert_eq!(
            entry.attempt_count, MAX_ATTEMPTS,
            "used the full attempt budget"
        );
        assert_eq!(entry.last_error.as_deref(), Some("always boom"));
        // A dead row is no longer claimable.
        let (due, _q) = store::claim_due(&db, now + 1_000_000).expect("claim");
        assert!(due.is_empty());
    }

    // A succeeding action on its first attempt is `done` AND records attempt_count = 1 (AB#1066 F3:
    // the success path now writes the attempt count, consistent with the failure paths — a done row
    // reflects its real execution count). Driven through `record_action_result` (F5).
    #[test]
    fn succeeding_action_is_done_first_attempt() {
        let db = Database::open_in_memory().expect("open db");
        let id =
            store::enqueue(&db, "p1", ActionKind::Notification, "s", "{}", 0).expect("enqueue");
        let (mut due, _q) = store::claim_due(&db, 0).expect("claim");
        let action = due.remove(0);
        let ok: AppResult<()> = Ok(());
        assert_eq!(
            record_action_result(&db, id, action.attempt_count, 0, &ok).expect("record"),
            Outcome::Done
        );

        let entry = store::get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(entry.status, crate::model::ActionStatus::Done);
        assert_eq!(
            entry.attempt_count, 1,
            "done records the succeeding attempt (F3)"
        );
        let (due, _q) = store::claim_due(&db, 0).expect("claim");
        assert!(due.is_empty(), "done is not re-claimed");
    }

    // A success AFTER prior failures records the cumulative attempt_count (AB#1066 F3): an action
    // that failed twice then succeeds on attempt 3 ends `done` with attempt_count = 3, not 0/1.
    #[test]
    fn success_after_retries_records_cumulative_attempt_count() {
        let db = Database::open_in_memory().expect("open db");
        let id =
            store::enqueue(&db, "p1", ActionKind::Notification, "s", "{}", 0).expect("enqueue");
        let err: AppResult<()> = Err(AppError::new("transient"));
        let ok: AppResult<()> = Ok(());

        // Two failures (attempt_count → 1 then 2), then a success on attempt 3.
        record_action_result(&db, id, 0, 0, &err).expect("fail 1");
        record_action_result(&db, id, 1, 100, &err).expect("fail 2");
        record_action_result(&db, id, 2, 200, &ok).expect("succeed");

        let entry = store::get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(entry.status, crate::model::ActionStatus::Done);
        assert_eq!(
            entry.attempt_count, 3,
            "done reflects all attempts, not just the last"
        );
        assert_eq!(entry.last_error, None, "success clears the prior error");
    }
}
