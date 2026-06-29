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

use tauri::{Manager, Runtime};

use crate::db::Database;
use crate::error::AppResult;
use crate::events::{OutboxEvent, StreamEvent};
use crate::model::{ActionExecutionResult, ActionKind};
use crate::outbox::{store, ActionExecutor, ClaimReleaser};
use crate::state::AppState;

/// Max execution attempts before an action dead-letters (AB#1066). After this many failed attempts
/// the worker flips the row to terminal `dead` rather than rescheduling it again.
const MAX_ATTEMPTS: u32 = 5;

/// Backoff base (seconds) for the first retry (AB#1066); each subsequent retry doubles it.
const BACKOFF_BASE_SECS: u64 = 30;

/// Backoff cap (seconds) (AB#1066): a long-failing action retries at most once per this interval
/// rather than growing the delay unboundedly toward the dead-letter.
const BACKOFF_CAP_SECS: u64 = 3600;

/// Backoff jitter as a fraction of the base delay (AB#1182): the retry fires within ±25% of the
/// exponential base. Decorrelates rows that fail in the SAME epoch (thundering-herd avoidance) so a
/// burst of failures doesn't re-fire in lockstep — the risk rises as more `ActionKind`s (AB#1069/
/// 1070) share the worker. river-style jittered backoff (river's `DefaultClientRetryPolicy` jitters
/// each retry by `retrySeconds * (rand*0.2 - 0.1)`, ±10%); we widen it to ±25% per the issue and
/// derive it deterministically from the row id instead of an RNG. ref: river retry_policy.go
const JITTER_DEN: u64 = 4; // 1/4 = 25%

/// A cheap, allocation-free 64-bit mix (splitmix64) (AB#1182): maps ONE already-mixed `seed` to a
/// well-distributed hash WITHOUT an RNG dependency, so the jitter is DETERMINISTIC (reproducible in
/// tests) yet decorrelated across rows. Distinct seeds map to distinct, uniformly-spread outputs.
/// The caller ([`next_backoff`]) folds the row id and attempt into the single `seed` it passes here.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Exponential backoff for the `attempt`-th failure (1-based) (AB#1066): `BASE * 2^(attempt-1)`,
/// capped at [`BACKOFF_CAP_SECS`], then jittered ±25% (AB#1182). `attempt` 1 ≈ 30s, 2 ≈ 60s, … each
/// within [0.75·base, 1.25·base]. Saturating + a bounded shift so a large `attempt` can't overflow
/// (it just pins at the cap). `seed` (the row id) keys the jitter so simultaneous failures spread
/// out instead of retrying in lockstep; the same `(attempt, seed)` is reproducible. The result is
/// clamped to ≥ 1 so the schedule always advances (a 0 delay would re-claim the row immediately).
fn next_backoff(attempt: u32, seed: u64) -> u64 {
    // Shifts ≥ 63 would overflow u64; clamp the exponent — the result pins at the cap anyway.
    let exp = attempt.saturating_sub(1).min(63);
    let base = BACKOFF_BASE_SECS
        .saturating_mul(1u64 << exp)
        .min(BACKOFF_CAP_SECS);
    // ±(base/JITTER_DEN) deterministic jitter keyed by (seed, attempt). `span == 0` (base < 4)
    // can't happen with BASE=30, but guard it so a future small base degrades to no-jitter.
    let span = base / JITTER_DEN;
    if span == 0 {
        return base.max(1);
    }
    let h = splitmix64(seed ^ ((attempt as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)));
    let offset = (h % (2 * span + 1)) as i64 - span as i64; // uniform in [-span, +span]
    (base as i64 + offset).max(1) as u64
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
/// left → [`Outcome::Retry`] at `now + next_backoff(new_attempt_count, seed)`; an error at the
/// [`MAX_ATTEMPTS`] budget → [`Outcome::Dead`]. `new_attempt_count` is the count INCLUDING the
/// attempt that just ran (so `MAX_ATTEMPTS` total attempts dead-letter). `seed` (the row id, AB#1182)
/// keys the backoff jitter so rows that fail together don't retry in lockstep.
fn decide_outcome(new_attempt_count: u32, now: u64, is_err: bool, seed: u64) -> Outcome {
    if !is_err {
        return Outcome::Done;
    }
    if new_attempt_count >= MAX_ATTEMPTS {
        Outcome::Dead
    } else {
        Outcome::Retry {
            next_attempt_at: now.saturating_add(next_backoff(new_attempt_count, seed)),
        }
    }
}

/// Per-kind staleness TTL in seconds (AB#1182), or `None` when the kind never expires. `0` (the
/// config disable sentinel) maps to `None` so a misconfigured `0` can't expire every queued action
/// instantly.
///
/// **Hard carrier:** the exhaustive `match ActionKind` (NO `_` wildcard arm) makes adding a kind
/// without deciding its TTL a COMPILE error — the per-kind policy can't silently default. Mirrors
/// `store::kind_as_wire`'s sealed-enum match. A `notification` carries the configured TTL (default
/// 2h) because a stale one fired late is a GHOST UI event. The work-triggering `review`/`check`
/// (AB#1069) do NOT expire: a row queued before a restart is still valid work the "restart-resume"
/// drain SHOULD fire, not a ghost to suppress; `stopReview` is idempotent (a no-op success when no
/// session is live), so it doesn't expire either. A future time-sensitive non-idempotent kind would
/// add its own TTL arm here.
fn ttl_secs(kind: ActionKind, notification_ttl_secs: u64) -> Option<u64> {
    match kind {
        ActionKind::Notification | ActionKind::MessagingReply | ActionKind::MessagingSend => {
            (notification_ttl_secs > 0).then_some(notification_ttl_secs)
        }
        ActionKind::Review | ActionKind::Check | ActionKind::StopReview => None,
    }
}

/// Whether a claimed row is STALE (AB#1182): its `created_at` is older than its kind's TTL as of
/// `now`. A kind with no TTL ([`ttl_secs`] `None`) never expires. Pure, so the sweep decision is
/// unit-tested without a DB or AppHandle (the [`run_due_once`] glue only dead-letters + emits).
///
/// Degenerate `created_at = 0` (the `store::now_epoch()` pre-epoch-clock fallback) is treated as
/// stale once the TTL has elapsed since the Unix epoch — effectively immediate on any real clock.
/// Acceptable: a 0 timestamp is already the documented degenerate, and dead-lettering such a row is
/// safer than firing a notification with an unknowable age.
fn is_expired(kind: ActionKind, created_at: u64, now: u64, notification_ttl_secs: u64) -> bool {
    ttl_secs(kind, notification_ttl_secs).is_some_and(|ttl| created_at.saturating_add(ttl) <= now)
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

pub(crate) struct EnqueueInput<'a> {
    pub(crate) project_id: &'a str,
    pub(crate) kind: ActionKind,
    pub(crate) summary: &'a str,
    pub(crate) payload: &'a str,
    pub(crate) dedupe_key: Option<&'a str>,
}

/// Enqueue a logical batch atomically, then announce the committed rows and wake the worker once.
pub(crate) fn enqueue_many<R: Runtime>(
    app: &tauri::AppHandle<R>,
    rows: &[EnqueueInput<'_>],
) -> AppResult<Vec<i64>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let db = app.state::<Database>();
    let now = store::now_epoch();
    let store_rows: Vec<store::EnqueueInput<'_>> = rows
        .iter()
        .map(|row| store::EnqueueInput {
            project_id: row.project_id,
            kind: row.kind,
            summary: row.summary,
            payload: row.payload,
            dedupe_key: row.dedupe_key,
        })
        .collect();
    let ids = store::enqueue_many(db.inner(), &store_rows, now)?;
    for id in &ids {
        announce_updated(app, db.inner(), *id);
    }
    app.state::<AppState>().outbox.wake();
    Ok(ids)
}

/// Enqueue a produced action with a live-pending dedupe key (#1379). Used by default rule
/// production for review/check actions so repeated poll/webhook processing of the same candidate
/// reuses one pending outbox row.
pub fn enqueue_deduped<R: Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: &str,
    kind: ActionKind,
    summary: &str,
    payload: &str,
    dedupe_key: &str,
) -> AppResult<i64> {
    let db = app.state::<Database>();
    let now = store::now_epoch();
    let id = store::enqueue_deduped(
        db.inner(),
        project_id,
        kind,
        summary,
        payload,
        dedupe_key,
        now,
    )?;
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
    result: &AppResult<ActionExecutionResult>,
) -> AppResult<Outcome> {
    let new_attempt_count = prev_attempt_count.saturating_add(1);
    let (outcome, error) = match result {
        Ok(ActionExecutionResult::Done) => (Outcome::Done, String::new()),
        Ok(ActionExecutionResult::Dead { message }) => (Outcome::Dead, message.clone()),
        Ok(ActionExecutionResult::Retry {
            message,
            retry_after_secs,
        }) => {
            if new_attempt_count >= MAX_ATTEMPTS {
                (Outcome::Dead, message.clone())
            } else {
                let next_attempt_at = retry_after_secs
                    .map(|s| now.saturating_add(s.clamp(1, BACKOFF_CAP_SECS)))
                    .unwrap_or_else(|| {
                        // The row `id` seeds the backoff jitter (AB#1182): rows that fail in the
                        // same cycle get distinct retry delays, so they don't re-fire in lockstep.
                        now.saturating_add(next_backoff(new_attempt_count, id as u64))
                    });
                (Outcome::Retry { next_attempt_at }, message.clone())
            }
        }
        Err(e) => {
            // Legacy executor errors stay retryable so non-notification actions keep their existing
            // behavior. Channel adapters should return a classified `ActionExecutionResult`.
            (
                decide_outcome(new_attempt_count, now, true, id as u64),
                e.message.clone(),
            )
        }
    };
    match &outcome {
        Outcome::Done => store::mark_done(db, id, new_attempt_count, now)?,
        Outcome::Retry { next_attempt_at } => {
            store::mark_retry(db, id, new_attempt_count, *next_attempt_at, &error, now)?
        }
        Outcome::Dead => store::mark_dead(db, id, new_attempt_count, &error, now)?,
    }
    Ok(outcome)
}

/// Fold a worker cycle's per-row failures of ONE operation into the single representative
/// cycle-error to emit (AB#1182 review F1/F2), or `None` when none failed (→ no emit). A single
/// failure surfaces the error verbatim; many collapse to "{n} {noun}（首条）：{first}" so the panel
/// sees the SCALE plus one sample, NOT one IPC event per row. Pure (no IO / AppHandle) so the "emit
/// once, not once-per-row" dedup is unit-tested directly; [`run_due_once`] only emits the result.
fn cycle_error_summary(failures: u32, first: Option<String>, noun: &str) -> Option<String> {
    let first = first?;
    Some(if failures > 1 {
        format!("{failures} {noun}（首条）：{first}")
    } else {
        first
    })
}

/// Run ONE worker cycle (AB#1066): claim the due `pending` rows and, for each, execute the injected
/// closure and record the outcome (done / reschedule / dead-letter) via [`record_action_result`],
/// re-emitting `outbox:updated`. Also emits for any rows `claim_due` dead-lettered as corrupt-kind
/// (so an open panel sees that transition). Best-effort throughout — a claim or record error is
/// logged, not propagated (the next tick retries). Concrete [`tauri::AppHandle`] (the executor's
/// signature is concrete, like the inbox's injected hooks).
///
/// **Staleness sweep (AB#1182):** a claimed row whose `created_at` is older than its kind's
/// [`ttl_secs`] is DEAD-LETTERED instead of executed, so a restart (whose first tick drains the
/// persisted backlog at once) doesn't fire hours-old notifications as ghosts. `notification_ttl_secs`
/// is the live config value the [`super::manager`] reads each cycle (so a config edit takes effect
/// without a restart).
///
/// **Cycle-error observability (AB#1182):** a `claim_due` / record / announce-read failure now ALSO
/// emits an [`crate::events::OutboxEvent::Error`] (alongside the `eprintln!`) so a persistent worker
/// failure surfaces in the panel rather than only on the desktop app's invisible stderr. The looped
/// `record`-write and `announce`-read failures are AGGREGATED (review F1/F2): each operation emits at
/// most ONE representative cycle-error per tick via [`cycle_error_summary`], never one per row.
pub async fn run_due_once(
    app: &tauri::AppHandle,
    db: &Database,
    executor: &ActionExecutor,
    release_terminal: &ClaimReleaser,
    notification_ttl_secs: u64,
) {
    // One epoch for the whole cycle's claim + staleness check (a row is "due" and "stale" against
    // the same `now`); the per-action record re-reads `now` after the (awaited) executor call.
    let now0 = store::now_epoch();
    let (due, quarantined) = match store::claim_due(db, now0) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("outbox: claim_due 失败：{}", e.message);
            announce_cycle_error(app, "claim", &e.message);
            return;
        }
    };
    // Aggregate per-row failures so a batch that fails EVERY write/read emits ONE cycle-error per
    // OPERATION after the loop, not one IPC event per row (AB#1182 review F1/F2: a locked WAL could
    // otherwise emit up to CLAIM_LIMIT events in a single tick — for the record-WRITE path AND the
    // announce READ-back path, both of which run inside this per-row loop). `first_*` is the
    // representative sample; the count conveys the scale. The single-command announce path
    // ([`announce_updated`]) keeps emitting immediately — only the looped worker path aggregates.
    let mut record_failures = 0u32;
    let mut first_record_error: Option<String> = None;
    let mut announce_failures = 0u32;
    let mut first_announce_error: Option<String> = None;

    // Announce the corrupt-kind rows claim_due just dead-lettered (their `dead` transition),
    // aggregating any read-back failure with the rest of this cycle's. (A corrupt-kind row never
    // reached `start_for_outbox`, so it has no AB#1204 claim to release.)
    for id in quarantined {
        if let Err(m) = try_announce_updated(app, db, id) {
            announce_failures += 1;
            first_announce_error.get_or_insert(m);
        }
    }
    for action in due {
        let id = action.id;
        let prev_attempt_count = action.attempt_count;

        // Staleness sweep (AB#1182): dead-letter a too-old row INSTEAD of executing it. `kind`
        // (Copy) and `created_at` are read before `action` moves into the executor below.
        if is_expired(action.kind, action.created_at, now0, notification_ttl_secs) {
            // A static, user-readable reason (AB#1182 review F3): no raw epochs — the panel already
            // renders this row's `created` / `updated` timestamps next to the error.
            let reason =
                "通知过期未投递：超过保留时限 / notification expired before delivery (stale beyond TTL)";
            if let Err(e) = store::mark_dead(db, id, prev_attempt_count, reason, now0) {
                eprintln!("outbox: 死信过期行失败（id={id}）：{}", e.message);
                record_failures += 1;
                first_record_error.get_or_insert(e.message);
            }
            if let Err(m) = try_announce_updated(app, db, id) {
                announce_failures += 1;
                first_announce_error.get_or_insert(m);
            }
            continue;
        }

        // The executor RESULT is authoritative: Ok → done, Err → retry/dead (no false `done`).
        let result = executor(app.clone(), action).await;
        let now = store::now_epoch();
        match record_action_result(db, id, prev_attempt_count, now, &result) {
            // Only `Done` explicitly releases its AB#1204 review claim — a `Done` row is never
            // re-claimed by the worker NOR re-queued, so the claim has no further reader and is
            // dropped to bound `outbox_review_claim` (a no-op for non-review rows). `Dead` does NOT
            // release: `store::reset_for_retry` lets a user manually re-queue a `dead` row back to
            // `pending` → it re-enters `start_for_outbox`, where the RETAINED claim is the breadcrumb
            // that SUPPRESSES a duplicate review (if the dead row's review already started/posted).
            // Releasing on `Dead` would drop that breadcrumb and re-open the dup window on manual
            // retry. The dead/leaked claim is instead cleaned up by the F4 FK `ON DELETE CASCADE`
            // when the owning `action_outbox` row is retention-pruned (so the table stays bounded
            // without the outbox slice ever needing to know about the claim — review-blind). A
            // still-`pending` Retry ALSO keeps the claim (a later sweep re-resolves it).
            Ok(outcome) => {
                if matches!(outcome, Outcome::Done) {
                    release_terminal(db, id);
                }
            }
            // AB#1182 cycle-error aggregation: a record-write failure is counted (one representative
            // cycle-error emitted after the loop), not one IPC event per row.
            Err(e) => {
                eprintln!("outbox: 记录动作终态失败（id={id}）：{}", e.message);
                record_failures += 1;
                first_record_error.get_or_insert(e.message);
            }
        }
        if let Err(m) = try_announce_updated(app, db, id) {
            announce_failures += 1;
            first_announce_error.get_or_insert(m);
        }
    }
    // One cycle-error PER OPERATION for the whole batch's failures (AB#1182 review F1/F2): dedup so
    // a persistently failing DB write/read surfaces once per tick per operation, not once per row.
    if let Some(summary) = cycle_error_summary(record_failures, first_record_error, "行写入失败")
    {
        announce_cycle_error(app, "record", &summary);
    }
    if let Some(summary) =
        cycle_error_summary(announce_failures, first_announce_error, "行更新发送失败")
    {
        announce_cycle_error(app, "announce", &summary);
    }
}

/// Emit a worker-CYCLE-level failure to the panel (AB#1182) — a failure NOT tied to one row
/// (`claim_due` spans the whole queue; a record/emit read failed). Runs ALONGSIDE the `eprintln!`
/// (the log keeps the dev-console trail; the emit is the only way a desktop user sees it, since the
/// app's stderr is invisible). `operation` names the failing site (`"claim"` / `"record"` /
/// `"announce"`) so the panel banner is actionable. Best-effort: a failed emit is dropped (there is
/// nowhere left to route it). Carries NO `project_id` — a cycle failure isn't project-scoped.
pub(crate) fn announce_cycle_error<R: Runtime>(
    app: &tauri::AppHandle<R>,
    operation: &str,
    message: &str,
) {
    crate::stream::emit(
        app,
        StreamEvent::Action(OutboxEvent::Error {
            operation: operation.to_string(),
            // Clamp the panel-visible text to the SAME budget as a row's `last_error` (AB#1182
            // review F2): a raw DB error could otherwise carry an unbounded / path-bearing string
            // straight to the panel — parity with the per-row `clamp_error` defense.
            message: store::clamp_error(message),
        }),
    );
}

/// Look up row `id` and emit `outbox:updated` for it (best-effort). Always re-reads so the emit
/// carries the CURRENT persisted state (post-transition), and routes on the entry's OWN
/// `project_id` (no external threading — unlike the inbox, `OutboxEntry` carries `projectId` at the
/// top level). A gone row (e.g. pruned) is `Ok(())` (nothing to emit). A store READ error is LOGGED
/// (not swallowed) and RETURNED as `Err(message)` — the CALLER decides how to surface it: the worker
/// cycle ([`run_due_once`]) AGGREGATES per-tick read failures into ONE representative cycle-error
/// (AB#1182 review F2 — a persistently failing read in the per-row loop must not emit one IPC event
/// per row, up to CLAIM_LIMIT), while the single-command path ([`announce_updated`]) emits at once.
fn try_announce_updated<R: Runtime>(
    app: &tauri::AppHandle<R>,
    db: &Database,
    id: i64,
) -> Result<(), String> {
    match store::get_entry(db, id) {
        Ok(Some(entry)) => {
            crate::stream::emit(
                app,
                StreamEvent::Action(OutboxEvent::Updated {
                    project_id: entry.project_id.clone(),
                    entry,
                }),
            );
            Ok(())
        }
        Ok(None) => Ok(()), // row gone (e.g. pruned) — nothing to emit.
        Err(e) => {
            eprintln!(
                "outbox: 读取条目以发送 outbox:updated 失败（id={id}）：{}",
                e.message
            );
            Err(e.message)
        }
    }
}

/// Single-command `outbox:updated` announce ([`enqueue`] + the `outbox_retry` command): re-read +
/// emit, and on a read failure surface it IMMEDIATELY as a cycle-error. One call site per command =
/// no storm, so immediacy is correct here; the worker cycle instead uses [`try_announce_updated`]
/// directly to AGGREGATE per-tick read failures into one representative event (AB#1182 review F2).
pub(crate) fn announce_updated<R: Runtime>(app: &tauri::AppHandle<R>, db: &Database, id: i64) {
    if let Err(message) = try_announce_updated(app, db, id) {
        announce_cycle_error(app, "announce", &message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppError;

    // Backoff is the exponential base ±25% jitter (AB#1066 + AB#1182): every result lands within the
    // jitter band of `BASE*2^(attempt-1)` (capped), is DETERMINISTIC per (attempt, seed), and
    // DECORRELATES across seeds — the machine-checked sub-properties that make the jitter a Medium
    // carrier (the emergent thundering-herd avoidance is the documented rationale, not lockable).
    #[test]
    fn next_backoff_is_jittered_within_band_and_deterministic() {
        // The un-jittered exponential base the jitter is applied around (mirrors `next_backoff`).
        fn base_of(attempt: u32) -> u64 {
            let exp = attempt.saturating_sub(1).min(63);
            BACKOFF_BASE_SECS
                .saturating_mul(1u64 << exp)
                .min(BACKOFF_CAP_SECS)
        }

        // `attempt` 0 and 1 share the base (the 1-based convention) — pinned so a 0-based refactor
        // can't silently shift the first-retry band.
        assert_eq!(base_of(0), base_of(1));

        // Bounds: every result is within [base - base/4, base + base/4] and ≥ 1 (the schedule must
        // always advance — a 0 delay would re-claim the row immediately). Jitter MAY exceed the
        // exponential cap by up to 25% (the cap bounds growth; jitter adds spread on top).
        for attempt in 0u32..=20 {
            let base = base_of(attempt);
            let span = base / JITTER_DEN;
            for seed in 0u64..64 {
                let d = next_backoff(attempt, seed);
                assert!(d >= 1, "delay always advances the schedule");
                assert!(
                    d >= base.saturating_sub(span) && d <= base + span,
                    "attempt={attempt} seed={seed}: {d} outside [{}, {}]",
                    base.saturating_sub(span),
                    base + span
                );
            }
        }

        // Deterministic: same (attempt, seed) → same delay (reproducible, RNG-free).
        assert_eq!(next_backoff(3, 42), next_backoff(3, 42));

        // Decorrelated: across many seeds at a fixed attempt the jitter spreads to >1 distinct delay
        // (a deterministic-but-constant backoff would collapse to one value — the herd this guards).
        let spread: std::collections::HashSet<u64> =
            (0u64..50).map(|seed| next_backoff(3, seed)).collect();
        assert!(spread.len() > 1, "jitter decorrelates rows by seed");

        // The jitter band INTENTIONALLY exceeds the exponential cap (review F8): at a large attempt
        // the base pins at BACKOFF_CAP_SECS, but +25% jitter can push the actual delay above it —
        // the cap bounds exponential GROWTH, not the jitter spread on top. Pin a concrete example so
        // that documented behavior is machine-checked, not just prose.
        assert_eq!(
            base_of(100),
            BACKOFF_CAP_SECS,
            "large attempt pins the base at the cap"
        );
        let max_at_cap = (0u64..256)
            .map(|seed| next_backoff(100, seed))
            .max()
            .expect("non-empty");
        assert!(
            max_at_cap > BACKOFF_CAP_SECS,
            "jitter can exceed the cap ({max_at_cap} > {BACKOFF_CAP_SECS})"
        );
    }

    // Per-kind staleness TTL (AB#1182): `ttl_secs` resolves the configured window via the exhaustive
    // `match ActionKind` (Hard carrier); `0` disables it; `is_expired` is the pure sweep predicate
    // `run_due_once` applies — true at/after `created_at + ttl`, false strictly before.
    #[test]
    fn ttl_expires_ephemeral_delivery_kinds_after_window_and_zero_disables() {
        const TTL: u64 = 7200; // 2h
        for kind in [
            ActionKind::Notification,
            ActionKind::MessagingReply,
            ActionKind::MessagingSend,
        ] {
            assert_eq!(ttl_secs(kind, TTL), Some(TTL));
            assert_eq!(ttl_secs(kind, 0), None, "0 disables the TTL");

            let created = 1_000u64;
            assert!(
                !is_expired(kind, created, created, TTL),
                "{kind:?}: fresh row not expired"
            );
            assert!(
                !is_expired(kind, created, created + TTL - 1, TTL),
                "{kind:?}: 1s before the window closes"
            );
            assert!(
                is_expired(kind, created, created + TTL, TTL),
                "{kind:?}: expired exactly at the TTL boundary"
            );
            assert!(
                is_expired(kind, created, created + TTL + 100, TTL),
                "{kind:?}: expired past the window"
            );
            assert!(
                !is_expired(kind, created, created + 10_000_000, 0),
                "{kind:?}: ttl=0 never expires (disabled)"
            );
        }
    }

    // End-to-end staleness sweep over the REAL store (AB#1182), WITHOUT an AppHandle: a row enqueued
    // long before `now` is claimed, judged stale by `is_expired`, and dead-lettered — the exact
    // claim → is_expired → mark_dead sequence `run_due_once`'s sweep runs (minus the emit). Proves a
    // restart that drains an old backlog terminalizes ghosts instead of firing them, and they never
    // re-claim.
    #[test]
    fn stale_pending_row_is_swept_to_dead() {
        const TTL: u64 = 7200;
        let db = Database::open_in_memory().expect("open db");
        // Enqueued at epoch 0; "now" is well past the TTL (a restart draining a stale backlog).
        let id =
            store::enqueue(&db, "p1", ActionKind::Notification, "stale", "{}", 0).expect("enqueue");
        let now = TTL + 1;

        let (due, _q) = store::claim_due(&db, now).expect("claim");
        let action = due.into_iter().find(|a| a.id == id).expect("claimed");
        assert!(
            is_expired(action.kind, action.created_at, now, TTL),
            "an old pending row is stale"
        );
        store::mark_dead(&db, id, action.attempt_count, "expired", now).expect("dead");

        let entry = store::get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(
            entry.status,
            crate::model::ActionStatus::Dead,
            "swept to dead, not executed"
        );
        // A swept row is terminal → never re-claimed (no ghost fire on a later tick).
        let (again, _q) = store::claim_due(&db, now + 1_000_000).expect("claim again");
        assert!(again.iter().all(|a| a.id != id), "dead row not re-claimed");
    }

    // Per-cycle error folding (AB#1182 review F1/F2): N per-row failures of one operation collapse
    // to ONE representative cycle-error, NOT one IPC event per row — the storm `run_due_once`
    // aggregates away for BOTH the record-write and announce-read loop paths. This locks the
    // emission COUNT at the decision seam (the worker loop's only cycle-error emit sites pass their
    // aggregated counters through here): none → no emit, one → verbatim, many → scale + first sample.
    #[test]
    fn cycle_error_summary_folds_many_failures_into_one() {
        // Nothing failed → nothing to emit (no spurious cycle-error on a clean tick).
        assert_eq!(cycle_error_summary(0, None, "行写入失败"), None);

        // A single failure surfaces verbatim (no count prefix) — the common 1-row case.
        assert_eq!(
            cycle_error_summary(1, Some("boom".to_string()), "行写入失败"),
            Some("boom".to_string())
        );

        // Many failures collapse to ONE summary carrying the scale + first sample, not one event per
        // row (the up-to-CLAIM_LIMIT storm this dedup prevents). Reused for the announce path too.
        assert_eq!(
            cycle_error_summary(7, Some("locked".to_string()), "行更新发送失败"),
            Some("7 行更新发送失败（首条）：locked".to_string())
        );
    }

    // The retry/dead-letter decision table (AB#1066): success → Done regardless of count; an error
    // under the budget → Retry at now+backoff; an error AT the budget → Dead.
    #[test]
    fn decide_outcome_done_retry_dead() {
        // A fixed seed so the jittered retry schedule is reproducible in the assertion (both sides
        // pass the SAME seed). The success/dead paths ignore the seed.
        const SEED: u64 = 7;

        // Success is Done no matter the attempt count.
        assert_eq!(decide_outcome(1, 1_000, false, SEED), Outcome::Done);
        assert_eq!(
            decide_outcome(MAX_ATTEMPTS, 1_000, false, SEED),
            Outcome::Done
        );

        // Error with attempts left → Retry at now + the SAME jittered backoff (same seed).
        assert_eq!(
            decide_outcome(1, 1_000, true, SEED),
            Outcome::Retry {
                next_attempt_at: 1_000 + next_backoff(1, SEED)
            }
        );
        assert_eq!(
            decide_outcome(MAX_ATTEMPTS - 1, 1_000, true, SEED),
            Outcome::Retry {
                next_attempt_at: 1_000 + next_backoff(MAX_ATTEMPTS - 1, SEED)
            }
        );

        // Error at the budget → Dead (no further reschedule).
        assert_eq!(
            decide_outcome(MAX_ATTEMPTS, 1_000, true, SEED),
            Outcome::Dead
        );
        assert_eq!(
            decide_outcome(MAX_ATTEMPTS + 1, 1_000, true, SEED),
            Outcome::Dead
        );
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
        let err: AppResult<ActionExecutionResult> = Err(AppError::new("always boom"));
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
        let ok: AppResult<ActionExecutionResult> = Ok(ActionExecutionResult::Done);
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
        let err: AppResult<ActionExecutionResult> = Err(AppError::new("transient"));
        let ok: AppResult<ActionExecutionResult> = Ok(ActionExecutionResult::Done);

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

    #[test]
    fn classified_dead_result_dead_letters_immediately() {
        let db = Database::open_in_memory().expect("open db");
        let id =
            store::enqueue(&db, "p1", ActionKind::Notification, "s", "{}", 0).expect("enqueue");
        let result: AppResult<ActionExecutionResult> = Ok(ActionExecutionResult::Dead {
            message: "permanent config error".to_string(),
        });

        assert_eq!(
            record_action_result(&db, id, 0, 10, &result).expect("record"),
            Outcome::Dead
        );

        let entry = store::get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(entry.status, crate::model::ActionStatus::Dead);
        assert_eq!(entry.attempt_count, 1);
        assert_eq!(entry.last_error.as_deref(), Some("permanent config error"));
    }

    #[test]
    fn classified_retry_result_honors_retry_after() {
        let db = Database::open_in_memory().expect("open db");
        let id =
            store::enqueue(&db, "p1", ActionKind::Notification, "s", "{}", 0).expect("enqueue");
        let result: AppResult<ActionExecutionResult> = Ok(ActionExecutionResult::Retry {
            message: "rate limited".to_string(),
            retry_after_secs: Some(90),
        });

        assert_eq!(
            record_action_result(&db, id, 0, 10, &result).expect("record"),
            Outcome::Retry {
                next_attempt_at: 100
            }
        );

        let entry = store::get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(entry.status, crate::model::ActionStatus::Pending);
        assert_eq!(entry.next_attempt_at, 100);
        assert_eq!(entry.last_error.as_deref(), Some("rate limited"));
    }

    #[test]
    fn classified_retry_result_caps_retry_after() {
        let db = Database::open_in_memory().expect("open db");
        let id =
            store::enqueue(&db, "p1", ActionKind::Notification, "s", "{}", 0).expect("enqueue");
        let result: AppResult<ActionExecutionResult> = Ok(ActionExecutionResult::Retry {
            message: "rate limited".to_string(),
            retry_after_secs: Some(BACKOFF_CAP_SECS * 10),
        });

        assert_eq!(
            record_action_result(&db, id, 0, 10, &result).expect("record"),
            Outcome::Retry {
                next_attempt_at: 10 + BACKOFF_CAP_SECS
            }
        );
    }
}
