//! PR slice Tauri commands: manual fetch + `gh` auth status.

use tauri::Emitter; // for app.emit

use crate::config::service as config_service;
use crate::error::AppResult;
use crate::model::{Candidate, PullRequestView};

use super::discover::{self, MonitorParams};
use super::gh::{gh_auth_status, GhRow, GhStatus, GithubCli};
use super::ledger::{now_epoch, Ledger};
use super::webhook::WebhookStatus;

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
/// `prs.json` partition projected at the current epoch — so the frontend can render
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

/// Apply the SAME static + cooldown gates the poll path applies (via `build_view`)
/// to `project_id`'s webhook-sourced candidates (#35), so a push trigger has dispatch
/// parity with that project's scheduler: no draft / fork / disallowed-author /
/// within-cooldown PR slips through just because it arrived by webhook. Resolves THAT
/// project's config (authors / cooldown) and loads ITS ledger partition, then filters
/// each candidate through [`discover::should_skip`] and [`discover::cooldown_skip`].
///
/// BOTH reads fail CLOSED: an unresolvable project / unreadable config OR an
/// unreadable ledger returns an empty Vec (dispatch nothing), same spirit as
/// [`super::scheduler::auto_review_enabled`]. The ledger is the dedup/cooldown source
/// of truth — degrading it to an empty ledger (the prior `unwrap_or_default()`) would
/// pass EVERY cooldown/dedup gate and re-review storm, so an unprovable "not a recent
/// duplicate" must fail closed, matching the poll path (`discover` uses
/// `Ledger::load(app, project_id)?`).
///
/// Called by the composition root's webhook dispatcher closure (`lib.rs`) with the
/// `project_id` the route matched; the conflict (both-labels) gate already dropped in
/// `webhook::payload_to_candidate`.
///
/// Coverage: the pure predicate composition is unit-tested via [`gate_candidates`];
/// the predicates themselves at their source (`discover::should_skip` /
/// `cooldown_skip`). The `AppHandle`-bound load branches run in the live app (a Tauri
/// `AppHandle` isn't constructible in a plain test).
pub(crate) fn gate_dispatchable<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: &str,
    candidates: Vec<Candidate>,
) -> Vec<Candidate> {
    let Ok(project) = config_service::project(app, project_id) else {
        return Vec::new();
    };
    let params = MonitorParams {
        repo: project.repo,
        review_label: project.review_label,
        check_label: project.check_label,
        authors: project.authors,
        pr_cooldown_seconds: project.pr_cooldown_seconds,
    };
    let Ok(ledger) = Ledger::load(app, project_id) else {
        return Vec::new();
    };
    gate_candidates(candidates, &params, &ledger, now_epoch())
}

/// Drop candidates a fresh `should_skip` / `cooldown_skip` rejects against `ledger`.
/// Split from [`gate_dispatchable`] so the predicate composition is unit-testable
/// without an `AppHandle` — locking that the webhook gate applies BOTH the static and
/// the cooldown gate (the predicates themselves are tested at their source in
/// `discover.rs`).
fn gate_candidates(
    candidates: Vec<Candidate>,
    params: &MonitorParams,
    ledger: &Ledger,
    now: u64,
) -> Vec<Candidate> {
    candidates
        .into_iter()
        .filter(|c| {
            discover::should_skip(c, params, ledger).is_none()
                && discover::cooldown_skip(c, params, ledger, now).is_none()
        })
        .collect()
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

    #[test]
    fn gate_candidates_drops_dispatched_and_cooldown_but_keeps_clean() {
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

        // Locks that the webhook gate applies BOTH the static (dispatched) and the
        // cooldown gate — only the clean candidate survives.
        let kept = gate_candidates(vec![clean, dispatched, cooled], &params(), &ledger, 1_500);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].number, 1);
    }
}
