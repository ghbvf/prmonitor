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

/// Run ONE worker cycle (AB#1066): claim the due `pending` rows and, for each, execute the injected
/// closure and record the outcome (done / reschedule / dead-letter), re-emitting `outbox:updated`.
/// Best-effort throughout — a claim or record error is logged, not propagated (the next tick
/// retries). Concrete [`tauri::AppHandle`] (the executor's signature is concrete, like the inbox's
/// injected hooks).
pub async fn run_due_once(app: &tauri::AppHandle, db: &Database, executor: &ActionExecutor) {
    let due = match store::claim_due(db, store::now_epoch()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("outbox: claim_due 失败：{}", e.message);
            return;
        }
    };
    for action in due {
        let id = action.id;
        let new_attempt_count = action.attempt_count.saturating_add(1);

        // The executor RESULT is authoritative: Ok → done, Err → retry/dead (no false `done`).
        let result = executor(app.clone(), action).await;
        let now = store::now_epoch();
        let error = result
            .as_ref()
            .err()
            .map(|e| e.message.clone())
            .unwrap_or_default();

        let recorded = match decide_outcome(new_attempt_count, now, result.is_err()) {
            Outcome::Done => store::mark_done(db, id, now),
            Outcome::Retry { next_attempt_at } => {
                store::mark_retry(db, id, new_attempt_count, next_attempt_at, &error, now)
            }
            Outcome::Dead => store::mark_dead(db, id, new_attempt_count, &error, now),
        };
        if let Err(e) = recorded {
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
fn announce_updated<R: Runtime>(app: &tauri::AppHandle<R>, db: &Database, id: i64) {
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

    // End-to-end retry → dead-letter walk over the REAL store (AB#1066 acceptance), driven without
    // an AppHandle by sequencing the same db-only seam `run_due_once` uses (claim_due →
    // decide_outcome → mark_retry/mark_dead). An always-failing action climbs attempt_count across
    // retries (staying `pending`, rescheduled forward) and finally dead-letters at MAX_ATTEMPTS.
    #[test]
    fn always_failing_action_retries_then_dead_letters() {
        let db = Database::open_in_memory().expect("open db");
        let id =
            store::enqueue(&db, "p1", ActionKind::Notification, "s", "{}", 0).expect("enqueue");

        let mut now = 0u64;
        let err: AppResult<()> = Err(AppError::new("always boom"));
        // Simulate the worker draining the row until it dead-letters. Bound the loop well above
        // MAX_ATTEMPTS so a regression (never dead-lettering) fails loudly instead of looping.
        for _ in 0..(MAX_ATTEMPTS + 3) {
            let due = store::claim_due(&db, now).expect("claim");
            let Some(action) = due.into_iter().find(|a| a.id == id) else {
                break; // no longer claimable (dead) — stop
            };
            let new_attempt_count = action.attempt_count + 1;
            match decide_outcome(new_attempt_count, now, err.is_err()) {
                Outcome::Done => unreachable!("the action always fails"),
                Outcome::Retry { next_attempt_at } => {
                    store::mark_retry(
                        &db,
                        id,
                        new_attempt_count,
                        next_attempt_at,
                        "always boom",
                        now,
                    )
                    .expect("retry");
                    now = next_attempt_at; // advance the clock to the next schedule
                }
                Outcome::Dead => {
                    store::mark_dead(&db, id, new_attempt_count, "always boom", now).expect("dead");
                    break;
                }
            }
        }

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
        assert!(store::claim_due(&db, now + 1_000_000)
            .expect("claim")
            .is_empty());
    }

    // A succeeding action on its first attempt is marked `done` (AB#1066), via the same seam.
    #[test]
    fn succeeding_action_is_done_first_attempt() {
        let db = Database::open_in_memory().expect("open db");
        let id =
            store::enqueue(&db, "p1", ActionKind::Notification, "s", "{}", 0).expect("enqueue");
        let action = store::claim_due(&db, 0).expect("claim").remove(0);
        let new_attempt_count = action.attempt_count + 1;
        let ok: AppResult<()> = Ok(());
        assert_eq!(
            decide_outcome(new_attempt_count, 0, ok.is_err()),
            Outcome::Done
        );
        store::mark_done(&db, id, 0).expect("done");

        let entry = store::get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(entry.status, crate::model::ActionStatus::Done);
        assert!(
            store::claim_due(&db, 0).expect("claim").is_empty(),
            "done is not re-claimed"
        );
    }
}
