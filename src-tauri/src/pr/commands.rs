//! PR slice Tauri commands: manual fetch + `gh` auth status.

use tauri::Emitter; // for app.emit

use crate::config::service as config_service;
use crate::error::AppResult;
use crate::model::{Candidate, PullRequestView};

use super::discover::{self, MonitorParams};
use super::gh::{gh_auth_status, GhRow, GhStatus, GithubCli};
use super::ledger::{now_epoch, Ledger};
use super::scheduler::{PollStatus, ProjectDispatcher};
use super::webhook::{DeliveryStatus, IngestIntent, WebhookDelivery, WebhookEvent, WebhookStatus};

/// Which registry write `ingest_webhook` performs for a parsed intent: `Upsert` is the
/// insert-or-update Track path; `UpdatePresent` is the status-only path (refresh an
/// EXISTING row, never insert). File-private + module-level (not defined inside the async
/// fn body) so the ingest reads as one straight-line decision.
enum WriteKind {
    Upsert,
    UpdatePresent,
}

/// Annotates one discovered row for the PR list and surfaces its dispatchable
/// [`Candidate`] when nothing gates it. Conflict (both trigger labels) skips
/// first, matching `router.py`'s discovery-stage drop; otherwise static gates
/// then cooldown. The live gate is reserved dead code (no second `gh` call per
/// poll), so a `None` skip reason here *is* the dispatch decision: the candidate
/// is returned for auto-trigger.
///
/// Returns `(view, Some(candidate))` for a clean row, `(view, None)` for a skipped
/// one — so the caller partitions the cycle's rows into the emit list (all views)
/// and the dispatch list (clean candidates) in one pass.
fn build_view(
    row: GhRow,
    params: &MonitorParams,
    ledger: &Ledger,
    now: u64,
) -> (PullRequestView, Option<Candidate>) {
    let skip_reason = if row.conflict {
        Some(discover::BOTH_TRIGGER_LABELS_REASON.to_string())
    } else {
        discover::should_skip(&row.candidate, params, ledger)
            .or_else(|| discover::cooldown_skip(&row.candidate, params, ledger, now))
    };
    // Clone the candidate for dispatch only when it passes every static + cooldown
    // gate (skip_reason None); a skipped row contributes a view but no candidate.
    let dispatchable = skip_reason.is_none().then(|| row.candidate.clone());
    let view = PullRequestView {
        number: row.candidate.number,
        title: row.title,
        labels: row.labels,
        url: row.url,
        kind: row.candidate.kind,
        skip_reason,
    };
    (view, dispatchable)
}

/// Discovers `project_id`'s monitored repo's open trigger-labelled PRs now (#35),
/// returning both the annotated views (for the PR list / snapshot) and the
/// dispatchable [`Candidate`]s — the clean rows (`skip_reason` None), which already
/// exclude conflict / draft / cross-repo / disallowed-author / already-dispatched /
/// within-cooldown PRs. Reads THAT project's config (repo, labels, authors, cooldown)
/// and its ledger partition; performs two `gh pr list` calls (review + check labels).
///
/// This is the shared discovery body driven by the scheduler's per-project poll loop
/// (`scheduler::discover_emit_dispatch`), its only caller; there is no manual-fetch
/// command — the frontend triggers a refresh via `poll_now`. The dispatchable
/// candidates flow to the auto-trigger dispatcher ([`crate::dispatch`]).
pub(crate) async fn discover<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: &str,
) -> AppResult<(Vec<PullRequestView>, Vec<Candidate>)> {
    // Cross-slice read of the config slice's public service (function-level, not
    // a type contract — `AppConfig` stays config-private; we resolve THIS project by
    // id and snapshot the fields the pr slice needs into `MonitorParams`).
    let project = config_service::project(app, project_id)?;
    let params = MonitorParams {
        repo: project.repo,
        review_label: project.review_label,
        check_label: project.check_label,
        authors: project.authors,
        pr_cooldown_seconds: project.pr_cooldown_seconds,
    };

    // Lock-free ledger read (no `LEDGER_WRITE_LOCK`): a stale-by-one-round snapshot is
    // fine here — this gate is an optimization, and the session registry's atomic
    // `try_reserve_pair` test-and-set is the real double-dispatch backstop (see
    // `Ledger::load`). The write path (`record_dispatched`) is the half that locks.
    let ledger = Ledger::load(app, project_id)?;
    let source = GithubCli::new(
        params.repo.clone(),
        params.review_label.clone(),
        params.check_label.clone(),
    );

    let rows = source.discover_rows().await?;
    let now = now_epoch();
    let mut views = Vec::with_capacity(rows.len());
    let mut dispatchable = Vec::new();
    for row in rows {
        let (view, cand) = build_view(row, &params, &ledger, now);
        if let Some(cand) = cand {
            dispatchable.push(cand);
        }
        views.push(view);
    }
    Ok((views, dispatchable))
}

/// Reconciles the per-project scheduler set to the enabled projects, only when the
/// persisted config validates — returning the validation error (without starting any
/// loop) otherwise.
///
/// **Multi-project semantics (#35):** this is "start ALL enabled projects". It
/// delegates to [`crate::pr::scheduler::SchedulerSet::reconcile`], which is
/// idempotent — it creates a loop for each newly-enabled project, stops + drops loops
/// for disabled/removed ones, and reconfigures survivors. So a launch, a `start_polling`,
/// and a post-save `reschedule` all funnel through the same reconcile.
///
/// The single enforcement point for the "no poll loop under an invalid config"
/// funnel (PR #41 F1). BOTH entry paths go through here, so neither can start any loop
/// on a config that fails [`config_service::load_validated`]:
/// - launch (`lib.rs` setup) discards the `Err` so a first launch (empty `projects`
///   default) routes to onboarding instead of polling;
/// - the public `start_polling` command surfaces the `Err` to the frontend.
///
/// `load_validated` returns the validated [`AppConfig`], so `reconcile` reads its
/// `projects` directly (no second load — the validated snapshot is the source).
pub(crate) fn start_if_config_valid<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &crate::state::AppState,
) -> AppResult<()> {
    let cfg = config_service::load_validated(app)?;
    state.scheduler.reconcile(app, &cfg.projects);
    Ok(())
}

/// Starts polling for ALL enabled projects (#35). Idempotent — reconciles the
/// scheduler set to the enabled projects (creates new ones, stops removed/disabled
/// ones, reconfigures survivors). Errors without starting any loop when the persisted
/// config is invalid (see [`start_if_config_valid`]), so no loop runs under a bad
/// config — the frontend surfaces the returned error. Takes no `project_id`: the
/// set always mirrors exactly the enabled projects, so there is no per-project
/// "start" — enabling a project (then saving + reconciling) is how it joins.
#[tauri::command]
pub async fn start_polling<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<()> {
    start_if_config_valid(&app, state.inner())
}

/// Stops polling for ALL projects (#35) — `stop_all` tears down every running loop
/// and empties the set (no-op if none running). This is the global pause; a later
/// `start_polling` / `reschedule` rebuilds the enabled loops from scratch. There is
/// deliberately no per-project stop command: disabling a project in config + saving
/// (which reconciles) stops just that one, keeping config the single source of which
/// projects run.
#[tauri::command]
pub async fn stop_polling(state: tauri::State<'_, crate::state::AppState>) -> AppResult<()> {
    state.scheduler.stop_all();
    Ok(())
}

/// Triggers an immediate discovery cycle for `project_id` ("立即拉取", #35). Returns
/// an error when that project's scheduler is paused/unknown (stopped): `wake` is a
/// no-op on a missing loop and would emit no `prs:updated` event, leaving the
/// frontend stuck in a loading state. Defense-in-depth alongside the
/// disabled-while-paused button.
#[tauri::command]
pub async fn poll_now<R: tauri::Runtime>(
    _app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
    project_id: &str,
) -> AppResult<()> {
    if state.scheduler.wake(project_id) {
        Ok(())
    } else {
        Err(crate::error::AppError::new("轮询已暂停，请先恢复轮询"))
    }
}

/// Re-reads every project's poll period and reconciles the scheduler set after a
/// config save (#35). Reconcile is idempotent: it adds loops for newly-enabled
/// projects, stops loops for disabled/removed ones, and rebuilds each survivor's
/// ticker (the immediate first tick also re-polls). Reads the persisted config to get
/// the current `projects`; a read failure surfaces to the frontend. NOT validated
/// (unlike `start_polling`): a save already validated, and reconcile only acts on
/// `enabled` projects whose fields the save checked.
#[tauri::command]
pub async fn reschedule<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<()> {
    let cfg = config_service::load(&app)?;
    state.scheduler.reconcile(&app, &cfg.projects);
    Ok(())
}

/// Reports `gh` CLI auth status for the StatusBar.
#[tauri::command]
pub async fn gh_status() -> AppResult<GhStatus> {
    Ok(gh_auth_status("gh").await)
}

/// Returns `project_id`'s retained tracked-PR list (#35) — that project's persisted
/// `tracked_pr` rows projected at the current epoch — so the frontend can render
/// the active project's state on mount without waiting for the next `prs:updated`
/// event (closes the startup lost-event race). Reads only `app` (+ the project id):
/// the persisted set survives restarts, so this no longer depends on the scheduler
/// having run this session. The grace window is resolved from THAT project's period.
#[tauri::command]
pub fn get_prs<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: &str,
) -> AppResult<Vec<crate::model::TrackedPrView>> {
    let tracked = super::registry::TrackedPrs::load(&app, project_id)?;
    Ok(super::registry::project_snapshot(
        &tracked, &app, project_id,
    ))
}

/// Sets a tracked PR's `archived` flag within `project_id`'s set (#35) and re-emits
/// that project's retained list immediately so the UI reflects the archive/unarchive
/// without waiting for the next poll round. PRs are never auto-evicted; archiving is
/// how users retire inactive rows.
///
/// Routes through the registry's single serialized write seam ([`registry::mutate_tracked`],
/// F1) scoped to `project_id` so this can't interleave with a poll-cycle upsert and lose
/// a write. An unknown / raced `number` is a benign no-op: `set_archived` reports no
/// change, the closure returns `persist == false` so the seam skips the store write, and
/// we emit nothing — no phantom write/event — returning `Ok` rather than erroring. On a
/// real change we re-emit the retained projection (built inside the seam, under the lock,
/// carrying `project_id`) so the list updates immediately.
#[tauri::command]
pub fn set_pr_archived<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: String,
    number: u64,
    archived: bool,
) -> AppResult<()> {
    let emitted = super::registry::mutate_tracked(&app, &project_id, |tracked| {
        if tracked.set_archived(number, archived) {
            (
                true,
                Some(super::registry::project_snapshot(
                    tracked,
                    &app,
                    &project_id,
                )),
            )
        } else {
            (false, None) // unknown number — nothing changed, skip persist + emit.
        }
    })?;
    if let Some(list) = emitted {
        let _ = app.emit(
            crate::events::PRS_UPDATED_EVENT,
            &crate::events::PrEvent::Updated {
                project_id,
                prs: list,
            },
        );
    }
    Ok(())
}

/// Annotate a webhook-sourced candidate into a [`PullRequestView`] + dispatch decision
/// (#61), applying the SAME static + cooldown gates the poll path applies via
/// [`build_view`] — minus the conflict branch (the caller handles conflict / StatusOnly
/// upstream from the parsed [`IngestIntent`], so this only ever sees a single-label
/// candidate). The push-path counterpart to `build_view`: `skip_reason = should_skip(..)
/// .or_else(|| cooldown_skip(..))`; `dispatchable = skip_reason.is_none().then(..)`, so a
/// clean candidate dispatches and a gated one (draft / fork / disallowed-author /
/// within-cooldown) becomes a skipped row with NO dispatch — dispatch parity with that
/// project's scheduler, so nothing slips through just because it arrived by webhook.
///
/// The view's title / labels / url come from the webhook payload (passed in by the
/// caller), so the list row is built without a `gh pr view` round trip.
///
/// Pure (no `AppHandle`) so the gate composition is unit-tested without a Tauri handle —
/// it replaces the deleted `gate_candidates` parity test (it asserts BOTH the static and
/// the cooldown gate apply; the predicates themselves are tested at their source in
/// `discover.rs`).
fn webhook_view(
    cand: Candidate,
    title: String,
    labels: Vec<String>,
    url: String,
    params: &MonitorParams,
    ledger: &Ledger,
    now: u64,
) -> (PullRequestView, Option<Candidate>) {
    let skip_reason = discover::should_skip(&cand, params, ledger)
        .or_else(|| discover::cooldown_skip(&cand, params, ledger, now));
    // Clone for dispatch only when it passes every static + cooldown gate; a gated row
    // contributes a view but no dispatch candidate (parity with `build_view`).
    let dispatchable = skip_reason.is_none().then(|| cand.clone());
    let view = PullRequestView {
        number: cand.number,
        title,
        labels,
        url,
        kind: cand.kind,
        skip_reason,
    };
    (view, dispatchable)
}

/// The full ingest decision for ONE routed [`WebhookEvent`], computed PURELY from plain
/// data by [`decide_ingest`] (no `AppHandle`) so EVERY branch is unit-tested. The
/// AppHandle-bound [`ingest_webhook`] becomes a thin shell that just performs the IO this
/// describes: `write` the row through the serialized seam, emit, spawn the dispatch when
/// `dispatchable` is `Some` AND autoReview is on, and record the delivery with `status` /
/// `message`.
struct IngestDecision {
    /// The list-row view to persist (Upsert) or refresh (UpdatePresent).
    view: PullRequestView,
    /// Which registry write to perform for `view`.
    write: WriteKind,
    /// The candidate to auto-dispatch when autoReview is on; `None` = nothing to dispatch
    /// (gated / conflict / status-only). Whether a `Some` is actually spawned is the
    /// shell's call (it depends on autoReview), but the TERMINAL `status` below already
    /// reflects the autoReview gate, so the shell never re-decides the status.
    dispatchable: Option<Candidate>,
    /// The single terminal delivery diagnostic status (#62) — finalized HERE from the
    /// dispatch decision + autoReview, so the shell records it verbatim.
    status: DeliveryStatus,
    /// The delivery diagnostic's human-readable note (skip reason / `None` for a clean
    /// dispatch).
    message: Option<String>,
}

/// PURE webhook-ingest decision (#61/#62): maps a parsed [`IngestIntent`] + the project's
/// gating inputs to the FULL [`IngestDecision`] (view + write + dispatch + terminal
/// delivery status + message), WITHOUT any `AppHandle` so every branch is unit-tested.
/// Extracted from the old inline `ingest_webhook` body so the decision path — including the
/// #61 core "autoReview off still LISTS the PR" — has automated coverage rather than only a
/// "verified via integration / manual verify" note. Behavior-preserving: the IO shell
/// ([`ingest_webhook`]) feeds it the same data the inline match consumed and acts on its
/// output verbatim.
///
/// `auto_review` is the project's resolved autoReview flag (the shell reads it ONCE off the
/// loaded project — the SAME per-project gate `scheduler::auto_review_enabled` resolves —
/// and passes it here so the dispatch decision and the terminal status agree on one value).
///
/// Branch semantics (each preserved from the inline body):
/// - `Track { candidate: Some(cand), .. }`: run the SAME static + cooldown gates as the
///   poll path (via [`webhook_view`]). `write = Upsert`. Gated (skip_reason `Some`) →
///   `dispatchable = None`, `status = Gated`, `message = skip_reason`. Clean (skip_reason
///   `None`) → `dispatchable = Some(cand)`; `status = if auto_review { Dispatched } else
///   { ListUpdated }` (#61: autoReview off still lists), `message = None`.
/// - `Track { candidate: None, conflict: .. }`: both trigger labels → a skipped "review"
///   row with the conflict reason; `write = Upsert`; no dispatch; `status = Gated`.
/// - `StatusOnly { kind }`: refresh an EXISTING row's status (`write = UpdatePresent`),
///   never insert / dispatch. Reason text + terminal status BOTH come from the type-locked
///   [`StatusOnlyKind`] (no string compare — see ai-robust.md).
// The flat plain-data arg list (the row metadata + the gating inputs) is deliberate: this
// is a PURE decision seam whose whole point is to be callable from a `#[test]` with no
// `AppHandle`, so it takes exactly the data the IO shell already holds rather than an
// AppHandle-bound bundle. Bundling into a struct would just move the arg count around and
// add a single-use type — same as `review::session::start_review`'s allow.
#[allow(clippy::too_many_arguments)]
fn decide_ingest(
    intent: IngestIntent,
    number: u64,
    title: String,
    labels: Vec<String>,
    url: String,
    params: &MonitorParams,
    ledger: &Ledger,
    now: u64,
    auto_review: bool,
) -> IngestDecision {
    match intent {
        IngestIntent::Track {
            candidate: Some(cand),
            ..
        } => {
            let (view, dispatchable) = webhook_view(cand, title, labels, url, params, ledger, now);
            // A single trigger label. A gated one (draft/fork/author/cooldown) is a `Gated`
            // row carrying the reason. A clean (dispatchable) one's terminal status is
            // decided HERE by the autoReview flag — Dispatched (on) vs ListUpdated (off, the
            // #61 core "PR enters the list even with autoReview off"); the shell only acts on
            // `dispatchable` + `auto_review`, it never re-derives the status.
            match (dispatchable, &view.skip_reason) {
                (Some(cand), _) => IngestDecision {
                    status: if auto_review {
                        DeliveryStatus::Dispatched
                    } else {
                        DeliveryStatus::ListUpdated
                    },
                    message: None,
                    dispatchable: Some(cand),
                    write: WriteKind::Upsert,
                    view,
                },
                (None, reason) => IngestDecision {
                    status: DeliveryStatus::Gated,
                    message: reason.clone(),
                    dispatchable: None,
                    write: WriteKind::Upsert,
                    view,
                },
            }
        }
        IngestIntent::Track {
            candidate: None,
            conflict: _,
        } => {
            // Both trigger labels (conflict): a skipped row, never dispatched. Kind "review"
            // for the view (the parse picked review for the conflict view).
            let view = PullRequestView {
                number,
                title,
                labels,
                url,
                kind: "review".to_string(),
                skip_reason: Some(discover::BOTH_TRIGGER_LABELS_REASON.to_string()),
            };
            IngestDecision {
                view,
                write: WriteKind::Upsert,
                dispatchable: None,
                status: DeliveryStatus::Gated,
                message: Some(discover::BOTH_TRIGGER_LABELS_REASON.to_string()),
            }
        }
        IngestIntent::StatusOnly { kind: status_kind } => {
            // Closed/merged or trigger-label-removed: refresh an existing row's status,
            // never insert, never dispatch. `kind` from the current labels (check vs the
            // review default). The reason text + terminal delivery status both come from the
            // type-locked `StatusOnlyKind` (no string compare — see FIX 1 / ai-robust.md).
            let kind = if labels.iter().any(|l| l == &params.check_label) {
                "check"
            } else {
                "review"
            };
            let reason = status_kind.reason().to_string();
            let view = PullRequestView {
                number,
                title,
                labels,
                url,
                kind: kind.to_string(),
                skip_reason: Some(reason.clone()),
            };
            IngestDecision {
                view,
                write: WriteKind::UpdatePresent,
                dispatchable: None,
                status: status_kind.delivery_status(),
                message: Some(reason),
            }
        }
    }
}

/// The AppHandle-bound webhook ingest (#61): the body of the [`WebhookIngestor`] the
/// composition root installs. A THIN IO shell around the pure [`decide_ingest`]: it
/// resolves config + ledger (fail-closed), calls `decide_ingest`, then performs only the
/// AppHandle-bound IO — the registry `write`, the `prs:updated` emit, the detached dispatch
/// spawn, and the single delivery record. The branch LOGIC (view + write + dispatchable +
/// terminal status + message) lives in `decide_ingest` and is unit-tested there; the
/// remaining `mutate_tracked` / `emit` / `spawn` here is the untestable AppHandle shell.
/// Takes ONE parsed, routed [`WebhookEvent`] and (a) upserts /
/// updates the persisted PR list row, (b) emits `prs:updated` so the list reflects the
/// push WITHOUT waiting for the next poll round (the #61 fix — webhook PRs now enter the
/// list even when autoReview is off), and (c) dispatches the gated-clean candidate iff
/// that project's autoReview is on. Records EXACTLY ONE delivery diagnostic at the end
/// (#62) — the handler records the early-exit classifications, this records the routable
/// terminal status.
///
/// **Fail-closed (parity with the deleted `gate_dispatchable`'s fail-closed reads):** an
/// unresolvable project / unreadable config OR an unreadable ledger records a `Gated`
/// delivery with the error message and RETURNS without upsert/emit/dispatch. The ledger
/// is the dedup/cooldown source of truth — degrading it to an empty ledger would pass
/// EVERY cooldown/dedup gate and re-review storm, so an unprovable "not a recent
/// duplicate" must fail closed, matching the poll path (`discover` uses
/// `Ledger::load(app, project_id)?`).
///
/// Reuses the registry's single serialized write seam ([`registry::mutate_tracked`]) for
/// the upsert + emit (so this can't interleave with a poll-cycle upsert / `set_pr_archived`
/// and lose a write). The dispatch decision uses the project's `auto_review` flag — read
/// ONCE off the same loaded project that `scheduler::auto_review_enabled` resolves from, so
/// both auto-trigger paths share the SAME per-project autoReview gate.
pub(crate) async fn ingest_webhook<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    dispatcher: &ProjectDispatcher,
    ev: WebhookEvent,
) {
    let WebhookEvent {
        project_id,
        action,
        repo,
        number,
        title,
        labels,
        url,
        intent,
    } = ev;

    // Resolve config + ledger, failing closed on either error (see the fn doc). On a
    // failure we record a `Gated` delivery with the error and return without touching the
    // list — fail-closed parity with the deleted `gate_dispatchable`.
    let record_failclosed = |app: &tauri::AppHandle<R>, msg: String| {
        record_webhook_delivery(
            app,
            &repo,
            &action,
            number,
            None,
            DeliveryStatus::Gated,
            Some(msg),
        );
    };
    let project = match config_service::project(app, &project_id) {
        Ok(p) => p,
        Err(e) => {
            record_failclosed(app, format!("项目配置不可读：{}", e.message));
            return;
        }
    };
    let params = MonitorParams {
        repo: project.repo,
        review_label: project.review_label,
        check_label: project.check_label,
        authors: project.authors,
        pr_cooldown_seconds: project.pr_cooldown_seconds,
    };
    // The project's autoReview flag, read ONCE off the SAME loaded project (the per-project
    // gate `scheduler::auto_review_enabled` resolves from the same `config_service::project`).
    // Reading it here — rather than re-loading config via `auto_review_enabled` after persist —
    // ties the dispatch decision and the terminal delivery status to one consistent value and
    // lets the pure `decide_ingest` finalize both.
    let auto_review = project.auto_review;
    let ledger = match Ledger::load(app, &project_id) {
        Ok(l) => l,
        Err(e) => {
            record_failclosed(app, format!("ledger 不可读：{}", e.message));
            return;
        }
    };
    let now = now_epoch();

    // The full ingest decision (view + write + dispatch + terminal status + message) is
    // computed by the PURE `decide_ingest` (unit-tested per branch); the rest of this fn is
    // the thin AppHandle-bound IO shell that acts on it.
    let IngestDecision {
        view,
        write: write_kind,
        dispatchable,
        status: final_status,
        message,
    } = decide_ingest(
        intent,
        number,
        title,
        labels,
        url,
        &params,
        &ledger,
        now,
        auto_review,
    );

    // Persist + emit through the single serialized write seam. The Upsert path always
    // persists + emits (an upsert always changes the set); the UpdatePresent path persists
    // + emits ONLY when the row existed (mirrors `set_pr_archived`'s no-op skip), so a
    // status-only event for an untracked PR is a benign no-op.
    let emitted = super::registry::mutate_tracked(app, &project_id, |tracked| match write_kind {
        WriteKind::Upsert => {
            tracked.upsert(std::slice::from_ref(&view), now);
            (
                true,
                Some(super::registry::project_snapshot(tracked, app, &project_id)),
            )
        }
        WriteKind::UpdatePresent => {
            if tracked.update_present(&view, now) {
                (
                    true,
                    Some(super::registry::project_snapshot(tracked, app, &project_id)),
                )
            } else {
                (false, None) // untracked PR — nothing changed, skip persist + emit.
            }
        }
    });
    match emitted {
        Ok(Some(list)) => {
            let _ = app.emit(
                crate::events::PRS_UPDATED_EVENT,
                &crate::events::PrEvent::Updated {
                    project_id: project_id.clone(),
                    prs: list,
                },
            );
        }
        // Persist no-op (untracked status-only PR) — nothing to emit.
        Ok(None) => {}
        // A store failure surfaces via `PrEvent::Error` (the project's error banner) —
        // SYMMETRIC with the poll path (`scheduler::persist_event` emits `PrEvent::Error`
        // on a persist failure). The delivery record below still reflects the dispatch
        // decision: dispatch is INDEPENDENT of persistence (it still runs if
        // dispatchable + autoReview), the same contract as the poll path. The list is
        // unchanged, but the banner tells the user the persist failed.
        Err(e) => {
            let _ = app.emit(
                crate::events::PRS_UPDATED_EVENT,
                &crate::events::PrEvent::Error {
                    project_id: project_id.clone(),
                    message: format!("PR 列表持久化失败：{}", e.message),
                },
            );
        }
    }

    // Dispatch the gated-clean candidate iff autoReview is on. `decide_ingest` already
    // gates `dispatchable` to `Some` ONLY for a clean (un-skipped) candidate AND already
    // baked the autoReview flag into `final_status` (Dispatched on / ListUpdated off, the
    // #61 core "PR enters the list even with autoReview off"), so the shell just spawns
    // when both hold — it never re-decides the status. The same `auto_review` value drives
    // both, so the spawn and the recorded status can't disagree. Detached spawn, mirroring
    // the scheduler's detached dispatch (a stop must not cancel a start in flight); the
    // JoinHandle is dropped explicitly so the task runs to completion regardless of caller.
    if let Some(cand) = dispatchable {
        if auto_review {
            drop(tauri::async_runtime::spawn(dispatcher(
                project_id.clone(),
                vec![cand],
            )));
        }
    }

    // Record the single terminal delivery diagnostic (#62) for this routable event. The
    // status came straight from `decide_ingest` — the dispatch decision above only acts on
    // it, it does not override it.
    record_webhook_delivery(
        app,
        &repo,
        &action,
        number,
        Some(view.kind),
        final_status,
        message,
    );
}

/// Record one webhook-delivery diagnostic into the manager's ring (#62) via `AppState`.
/// Helper so `ingest_webhook`'s several record sites (fail-closed + terminal) stay one
/// liners and never leak the secret/token into the diagnostic.
fn record_webhook_delivery<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    repo: &str,
    action: &Option<String>,
    number: u64,
    kind: Option<String>,
    status: DeliveryStatus,
    message: Option<String>,
) {
    use tauri::Manager;
    app.state::<crate::state::AppState>()
        .webhook
        .record_delivery(WebhookDelivery {
            received_at_epoch: now_epoch(),
            event: "pull_request".to_string(),
            action: action.clone(),
            repo: Some(repo.to_string()),
            pr_number: Some(number),
            kind,
            status,
            message,
        });
}

/// Starts the webhook receiver + Cloudflare Quick Tunnel. Requires `webhook_enabled`
/// in the persisted config; returns the resolved status (incl. the public
/// `*.trycloudflare.com` URL to paste into GitHub). The local server binds
/// `127.0.0.1` only — the public path is the tunnel.
///
/// **Multi-project (#35):** ONE receiver serves every project. The handler routes each
/// incoming event by repo, so this builds a [`super::webhook::ProjectRoute`] from every
/// `enabled` project (its repo + trigger labels + id) and hands the snapshot to
/// [`super::webhook::WebhookManager::start`]. The receiver params (port / secret /
/// cloudflared_bin / tunnel) stay GLOBAL on [`AppConfig`]. The route snapshot is fixed
/// for the runtime's life — a project add/remove requires a webhook restart (the
/// composition root wires that on `set_config`).
///
/// Uses `load_validated` (not bare `load`) so the SAME `validate` the wizard / poll
/// path enforce gates the start: when `webhook_enabled`, an empty `webhookSecret` or a
/// zero `webhookPort` (a hand-edited config that would bind a random port) is rejected
/// HERE, before cloudflared spawns — parity with `start_polling` / `start_review`.
/// `validate` only constrains the webhook fields WHEN enabled, so the explicit
/// `webhook_enabled` check below still owns the "enable it first" message.
#[tauri::command]
pub async fn start_webhook<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<WebhookStatus> {
    let cfg = config_service::load_validated(&app)?;
    if !cfg.webhook_enabled {
        return Err(crate::error::AppError::new(
            "请先在设置中启用 Webhook 并保存配置",
        ));
    }
    // One route per enabled project — the handler matches an event's repo against these
    // and tags the dispatched candidate with the owning project's id (#35).
    let routes: Vec<super::webhook::ProjectRoute> = cfg
        .projects
        .iter()
        .filter(|p| p.enabled)
        .map(|p| super::webhook::ProjectRoute {
            id: p.id.clone(),
            repo: p.repo.clone(),
            review_label: p.review_label.clone(),
            check_label: p.check_label.clone(),
        })
        .collect();
    state
        .webhook
        .start(
            cfg.webhook_port,
            cfg.webhook_secret,
            routes,
            cfg.cloudflared_bin,
            super::webhook::TunnelSpec {
                mode: cfg.webhook_tunnel_mode,
                command: cfg.webhook_tunnel_command,
                public_url: cfg.webhook_public_url,
            },
        )
        .await
}

/// Stops the webhook receiver + tunnel (no-op if not running). Returns the post-stop
/// status (so the UI reflects `running: false` + the current cloudflared install state).
#[tauri::command]
pub async fn stop_webhook<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<WebhookStatus> {
    state.webhook.stop().await;
    let cfg = config_service::load(&app)?;
    Ok(state
        .webhook
        .status(&cfg.cloudflared_bin, cfg.webhook_tunnel_mode)
        .await)
}

/// Reports webhook receiver + tunnel status (running, public URL, cloudflared install)
/// for the settings panel.
#[tauri::command]
pub async fn webhook_status<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<WebhookStatus> {
    let cfg = config_service::load(&app)?;
    Ok(state
        .webhook
        .status(&cfg.cloudflared_bin, cfg.webhook_tunnel_mode)
        .await)
}

/// Snapshot of the webhook-delivery diagnostics ring (#62) for the settings panel —
/// the recent window of "did GitHub reach us, and what did we do with each delivery".
/// Oldest→newest; capped at the manager's ring size. Never carries the secret/token.
#[tauri::command]
pub async fn webhook_deliveries(
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<Vec<WebhookDelivery>> {
    Ok(state.webhook.deliveries_snapshot())
}

/// Reports `project_id`'s poll-loop status (#62) for the settings panel: whether the
/// loop is running, its resolved interval, and the last cycle's diagnostics (started /
/// success / error / persist epochs + discovered count). Pulled on demand — NO new event
/// type, so the `events.rs` union stays untouched.
#[tauri::command]
pub async fn poll_status<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
    project_id: &str,
) -> AppResult<PollStatus> {
    Ok(state.scheduler.poll_status(&app, project_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Candidate;

    fn params() -> MonitorParams {
        MonitorParams {
            repo: "o/r".to_string(),
            review_label: "review-label".to_string(),
            check_label: "check-label".to_string(),
            authors: vec![],
            pr_cooldown_seconds: 1800,
        }
    }

    fn row(number: u64, kind: &str, conflict: bool) -> GhRow {
        GhRow {
            candidate: Candidate {
                number,
                head_sha: "sha".to_string(),
                head_ref: "ref".to_string(),
                author: "octocat".to_string(),
                is_cross_repository: false,
                is_draft: false,
                kind: kind.to_string(),
            },
            title: format!("PR {number}"),
            url: format!("https://x/{number}"),
            labels: vec!["review-label".to_string()],
            conflict,
        }
    }

    #[test]
    fn build_view_clean_row_has_no_skip_reason_and_is_dispatchable() {
        let (view, cand) = build_view(row(1, "review", false), &params(), &Ledger::default(), 0);
        assert_eq!(view.number, 1);
        assert_eq!(view.kind, "review");
        assert_eq!(view.title, "PR 1");
        assert_eq!(view.skip_reason, None);
        // Clean row (skip_reason None) → surfaced as a dispatchable candidate.
        let cand = cand.expect("clean row yields a dispatchable candidate");
        assert_eq!(cand.number, 1);
        assert_eq!(cand.kind, "review");
    }

    #[test]
    fn build_view_conflict_row_skips_and_is_not_dispatchable() {
        let (view, cand) = build_view(row(2, "check", true), &params(), &Ledger::default(), 0);
        assert_eq!(
            view.skip_reason,
            Some("both review and check trigger labels are present".to_string())
        );
        // Skipped row → no candidate for dispatch.
        assert!(cand.is_none());
    }

    #[test]
    fn build_view_propagates_static_gate_skip_and_omits_candidate() {
        let mut r = row(3, "review", false);
        r.candidate.is_draft = true;
        let (view, cand) = build_view(r, &params(), &Ledger::default(), 0);
        assert_eq!(view.skip_reason, Some("draft PR".to_string()));
        assert!(cand.is_none());
    }

    #[test]
    fn build_view_propagates_cooldown_skip_and_omits_candidate() {
        use crate::pr::ledger::{dispatch_key, DispatchEvent};
        use std::collections::HashSet;

        let r = row(4, "review", false);
        let key = dispatch_key(4, &r.candidate.head_sha, "review");
        let ledger = Ledger {
            dispatched: HashSet::new(),
            events: vec![DispatchEvent {
                pr: 4,
                kind: "review".to_string(),
                head_sha: r.candidate.head_sha.clone(),
                key,
                dispatched_at_epoch: 1_000,
            }],
        };
        // 1800s cooldown, dispatched 500s before `now` → within window.
        let (view, cand) = build_view(r, &params(), &ledger, 1_500);
        assert!(
            view.skip_reason
                .as_deref()
                .unwrap()
                .contains("within cooldown"),
            "{:?}",
            view.skip_reason
        );
        assert!(cand.is_none());
    }

    // Migrated from the deleted `gate_candidates` parity test: `webhook_view` (#61) must
    // apply BOTH the static (already-dispatched) and the cooldown gate to a webhook-sourced
    // candidate — a clean one dispatches (Some), a gated one is a skipped row (None) — so a
    // push trigger has dispatch parity with the poll path. The predicates themselves are
    // tested at their source in `discover.rs`.
    #[test]
    fn webhook_view_applies_both_static_and_cooldown_gates() {
        use crate::pr::ledger::{dispatch_key, DispatchEvent};
        use std::collections::HashSet;

        let clean = row(1, "review", false).candidate;
        let dispatched = row(2, "review", false).candidate;
        let cooled = row(3, "review", false).candidate;

        let ledger = Ledger {
            // #2 already dispatched at this head_sha → should_skip drops it.
            dispatched: HashSet::from([dispatch_key(2, &dispatched.head_sha, "review")]),
            // #3 dispatched 500s before `now` (1800s cooldown) → cooldown_skip drops it.
            events: vec![DispatchEvent {
                pr: 3,
                kind: "review".to_string(),
                head_sha: cooled.head_sha.clone(),
                key: dispatch_key(3, &cooled.head_sha, "review"),
                dispatched_at_epoch: 1_000,
            }],
        };

        let meta = |n: u64| {
            (
                format!("PR {n}"),
                vec!["review-label".to_string()],
                format!("https://x/{n}"),
            )
        };

        // Clean candidate → no skip_reason → dispatchable Some.
        let (v1, d1) = {
            let (t, l, u) = meta(1);
            webhook_view(clean, t, l, u, &params(), &ledger, 1_500)
        };
        assert_eq!(v1.number, 1);
        assert_eq!(v1.title, "PR 1");
        assert_eq!(v1.skip_reason, None);
        assert!(d1.is_some(), "a clean candidate is dispatchable");

        // Already-dispatched (static gate) → skip_reason Some → dispatchable None.
        let (v2, d2) = {
            let (t, l, u) = meta(2);
            webhook_view(dispatched, t, l, u, &params(), &ledger, 1_500)
        };
        assert!(
            v2.skip_reason
                .as_deref()
                .is_some_and(|r| r.contains("already dispatched")),
            "static gate fires: {:?}",
            v2.skip_reason
        );
        assert!(
            d2.is_none(),
            "a statically-gated candidate is not dispatchable"
        );

        // Within cooldown (cooldown gate) → skip_reason Some → dispatchable None.
        let (v3, d3) = {
            let (t, l, u) = meta(3);
            webhook_view(cooled, t, l, u, &params(), &ledger, 1_500)
        };
        assert!(
            v3.skip_reason
                .as_deref()
                .is_some_and(|r| r.contains("within cooldown")),
            "cooldown gate fires: {:?}",
            v3.skip_reason
        );
        assert!(
            d3.is_none(),
            "a cooldown-gated candidate is not dispatchable"
        );
    }

    // ── `decide_ingest` (F7): the PURE ingest-decision seam extracted from the
    // AppHandle-bound `ingest_webhook` body so EVERY branch (incl. the #61 core
    // "autoReview off still LISTS the PR") has automated coverage rather than only the old
    // "verified via integration / NOT a unit test" note. Each test asserts the FULL
    // decision: view fields + write + dispatchable + terminal status + message.
    use super::super::webhook::StatusOnlyKind;

    /// A clean single-label candidate wrapped as `IngestIntent::Track { candidate: Some }`.
    /// `row` builds a non-draft, non-fork, allowed-author candidate → clean under the
    /// empty-ledger `params()` gates, so the only thing left to vary is autoReview.
    fn wrap_some(number: u64) -> IngestIntent {
        IngestIntent::Track {
            candidate: Some(row(number, "review", false).candidate),
            conflict: false,
        }
    }

    #[test]
    fn decide_ingest_clean_candidate_auto_review_on_dispatches() {
        // #61: a clean candidate with autoReview ON → Upsert row, dispatch the candidate,
        // status Dispatched, no message.
        let d = decide_ingest(
            wrap_some(1),
            1,
            "PR 1".to_string(),
            vec!["review-label".to_string()],
            "https://x/1".to_string(),
            &params(),
            &Ledger::default(),
            0,
            true,
        );
        assert_eq!(d.view.number, 1);
        assert_eq!(d.view.kind, "review");
        assert_eq!(d.view.skip_reason, None);
        assert!(matches!(d.write, WriteKind::Upsert));
        assert!(d.dispatchable.is_some(), "clean candidate is dispatchable");
        assert!(matches!(d.status, DeliveryStatus::Dispatched));
        assert_eq!(d.message, None);
    }

    #[test]
    fn decide_ingest_clean_candidate_auto_review_off_lists_without_dispatch() {
        // THE #61 CORE: a clean candidate with autoReview OFF → still Upserts the row +
        // surfaces a dispatchable (the shell just won't spawn it), status ListUpdated (NOT
        // Dispatched), no message. This is the branch that previously had no unit test.
        let d = decide_ingest(
            wrap_some(2),
            2,
            "PR 2".to_string(),
            vec!["review-label".to_string()],
            "https://x/2".to_string(),
            &params(),
            &Ledger::default(),
            0,
            false,
        );
        assert_eq!(d.view.number, 2);
        assert_eq!(d.view.skip_reason, None);
        assert!(matches!(d.write, WriteKind::Upsert));
        assert!(
            d.dispatchable.is_some(),
            "autoReview-off still surfaces the candidate (the shell gates the spawn)"
        );
        assert!(
            matches!(d.status, DeliveryStatus::ListUpdated),
            "autoReview off → ListUpdated, not Dispatched"
        );
        assert_eq!(d.message, None);
    }

    #[test]
    fn decide_ingest_static_gated_candidate_is_gated_no_dispatch() {
        // A draft candidate is gated by `should_skip` → Upsert a skipped row, NO dispatch,
        // status Gated, message = the skip reason. autoReview on must NOT override the gate.
        let mut cand = row(3, "review", false).candidate;
        cand.is_draft = true;
        let intent = IngestIntent::Track {
            candidate: Some(cand),
            conflict: false,
        };
        let d = decide_ingest(
            intent,
            3,
            "PR 3".to_string(),
            vec!["review-label".to_string()],
            "https://x/3".to_string(),
            &params(),
            &Ledger::default(),
            0,
            true,
        );
        assert_eq!(d.view.skip_reason, Some("draft PR".to_string()));
        assert!(matches!(d.write, WriteKind::Upsert));
        assert!(
            d.dispatchable.is_none(),
            "a gated candidate never dispatches"
        );
        assert!(matches!(d.status, DeliveryStatus::Gated));
        assert_eq!(d.message, Some("draft PR".to_string()));
    }

    #[test]
    fn decide_ingest_cooldown_gated_candidate_is_gated_no_dispatch() {
        // A candidate within its dispatch cooldown is gated by `cooldown_skip` → Gated row,
        // no dispatch, message = the cooldown reason (even with autoReview on).
        use crate::pr::ledger::{dispatch_key, DispatchEvent};
        use std::collections::HashSet;

        let cand = row(4, "review", false).candidate;
        let ledger = Ledger {
            dispatched: HashSet::new(),
            events: vec![DispatchEvent {
                pr: 4,
                kind: "review".to_string(),
                head_sha: cand.head_sha.clone(),
                key: dispatch_key(4, &cand.head_sha, "review"),
                dispatched_at_epoch: 1_000,
            }],
        };
        let intent = IngestIntent::Track {
            candidate: Some(cand),
            conflict: false,
        };
        // dispatched 500s before `now` (1800s cooldown) → within window.
        let d = decide_ingest(
            intent,
            4,
            "PR 4".to_string(),
            vec!["review-label".to_string()],
            "https://x/4".to_string(),
            &params(),
            &ledger,
            1_500,
            true,
        );
        assert!(
            d.view
                .skip_reason
                .as_deref()
                .is_some_and(|r| r.contains("within cooldown")),
            "cooldown gate fires: {:?}",
            d.view.skip_reason
        );
        assert!(matches!(d.write, WriteKind::Upsert));
        assert!(d.dispatchable.is_none());
        assert!(matches!(d.status, DeliveryStatus::Gated));
        assert!(d
            .message
            .as_deref()
            .is_some_and(|m| m.contains("within cooldown")));
    }

    #[test]
    fn decide_ingest_conflict_is_gated_with_both_labels_reason() {
        // Both trigger labels (`candidate: None, conflict: true`) → Upsert a skipped "review"
        // row carrying the BOTH-labels reason, NO dispatch, status Gated.
        let intent = IngestIntent::Track {
            candidate: None,
            conflict: true,
        };
        let d = decide_ingest(
            intent,
            5,
            "PR 5".to_string(),
            vec!["review-label".to_string(), "check-label".to_string()],
            "https://x/5".to_string(),
            &params(),
            &Ledger::default(),
            0,
            true,
        );
        assert_eq!(d.view.number, 5);
        assert_eq!(d.view.kind, "review");
        assert_eq!(
            d.view.skip_reason,
            Some(discover::BOTH_TRIGGER_LABELS_REASON.to_string())
        );
        assert!(matches!(d.write, WriteKind::Upsert));
        assert!(d.dispatchable.is_none());
        assert!(matches!(d.status, DeliveryStatus::Gated));
        assert_eq!(
            d.message,
            Some(discover::BOTH_TRIGGER_LABELS_REASON.to_string())
        );
    }

    #[test]
    fn decide_ingest_status_only_closed_is_not_open_update_present() {
        // A closed/merged PR → StatusOnly { ClosedOrMerged }: UpdatePresent (refresh an
        // existing row, never insert / dispatch), status NotOpen, reason "PR 已关闭或合并".
        let intent = IngestIntent::StatusOnly {
            kind: StatusOnlyKind::ClosedOrMerged,
        };
        let d = decide_ingest(
            intent,
            6,
            "PR 6".to_string(),
            vec!["review-label".to_string()],
            "https://x/6".to_string(),
            &params(),
            &Ledger::default(),
            0,
            true,
        );
        assert_eq!(d.view.number, 6);
        assert_eq!(d.view.kind, "review");
        assert_eq!(d.view.skip_reason, Some("PR 已关闭或合并".to_string()));
        assert!(matches!(d.write, WriteKind::UpdatePresent));
        assert!(d.dispatchable.is_none());
        assert!(matches!(d.status, DeliveryStatus::NotOpen));
        assert_eq!(d.message, Some("PR 已关闭或合并".to_string()));
    }

    #[test]
    fn decide_ingest_status_only_trigger_label_removed_is_no_trigger_label_update_present() {
        // An open PR with the trigger label removed → StatusOnly { TriggerLabelRemoved }:
        // UpdatePresent, status NoTriggerLabel, reason "触发 label 已移除". With ONLY the
        // check label present, the view kind is "check" (the labels-derived kind).
        let intent = IngestIntent::StatusOnly {
            kind: StatusOnlyKind::TriggerLabelRemoved,
        };
        let d = decide_ingest(
            intent,
            7,
            "PR 7".to_string(),
            vec!["check-label".to_string()],
            "https://x/7".to_string(),
            &params(),
            &Ledger::default(),
            0,
            false,
        );
        assert_eq!(d.view.number, 7);
        assert_eq!(d.view.kind, "check", "check label present → kind check");
        assert_eq!(d.view.skip_reason, Some("触发 label 已移除".to_string()));
        assert!(matches!(d.write, WriteKind::UpdatePresent));
        assert!(d.dispatchable.is_none());
        assert!(matches!(d.status, DeliveryStatus::NoTriggerLabel));
        assert_eq!(d.message, Some("触发 label 已移除".to_string()));
    }
}
