//! PR slice Tauri commands: manual fetch + `gh` auth status.

use crate::config::service as config_service;
use crate::error::AppResult;
use crate::model::{Candidate, PullRequestView};

use super::discover::{self, MonitorParams};
use super::gh::{gh_auth_status, GhRow, GhStatus, GithubCli};
use super::ledger::{now_epoch, Ledger};

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

/// Discovers the monitored repo's open trigger-labelled PRs now, returning both
/// the annotated views (for the PR list / snapshot) and the dispatchable
/// [`Candidate`]s — the clean rows (`skip_reason` None), which already exclude
/// conflict / draft / cross-repo / disallowed-author / already-dispatched /
/// within-cooldown PRs. Reads config (repo, labels, authors, cooldown) and the
/// dedup ledger; performs two `gh pr list` calls (review + check labels).
///
/// This is the shared discovery body driven by the scheduler's poll loop
/// (`scheduler::discover_emit_dispatch`), its only caller; there is no
/// manual-fetch command — the frontend triggers a refresh via `poll_now`. The
/// dispatchable candidates flow to the auto-trigger dispatcher ([`crate::dispatch`]).
pub(crate) async fn discover<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> AppResult<(Vec<PullRequestView>, Vec<Candidate>)> {
    // Cross-slice read of the config slice's public service (function-level, not
    // a type contract — `AppConfig` stays config-private; we snapshot the fields
    // the pr slice needs into `MonitorParams`).
    let cfg = config_service::load(app)?;
    let params = MonitorParams {
        repo: cfg.repo,
        review_label: cfg.review_label,
        check_label: cfg.check_label,
        authors: cfg.authors,
        pr_cooldown_seconds: cfg.pr_cooldown_seconds,
    };

    let ledger = Ledger::load(app)?;
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

/// Starts the scheduled-pull loop only when the persisted config validates,
/// returning the validation error (without starting) otherwise.
///
/// The single enforcement point for the "no poll loop under an invalid config"
/// funnel (PR #41 F1). BOTH entry paths go through here, so neither can start the
/// loop on a config that fails [`config_service::load_validated`]:
/// - launch (`lib.rs` setup) discards the `Err` so a first launch (empty
///   `repoRoot` default) routes to onboarding instead of polling a default config;
/// - the public `start_polling` command surfaces the `Err` to the frontend.
///
/// Previously the command called `scheduler.start` directly, leaving the funnel's
/// downstream open: a user could start the loop on an invalid hand-edited config,
/// spamming a per-cycle `DispatchError` and running `gh pr list` against a bad repo.
pub(crate) fn start_if_config_valid<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &crate::state::AppState,
) -> AppResult<()> {
    config_service::load_validated(app)?;
    state.scheduler.start(app.clone());
    Ok(())
}

/// Starts the scheduled-pull loop. Idempotent (a no-op if already running); errors
/// without starting when the persisted config is invalid (see
/// [`start_if_config_valid`]), so the loop never runs under a bad config — the
/// frontend surfaces the returned error.
#[tauri::command]
pub async fn start_polling<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<()> {
    start_if_config_valid(&app, state.inner())
}

/// Stops the scheduled-pull loop (no-op if not running).
#[tauri::command]
pub async fn stop_polling(state: tauri::State<'_, crate::state::AppState>) -> AppResult<()> {
    state.scheduler.stop();
    Ok(())
}

/// Triggers an immediate discovery cycle ("立即拉取"). Returns an error when the
/// scheduler is paused (stopped): `wake` is a no-op on a stopped loop and would
/// emit no `prs:updated` event, leaving the frontend stuck in a loading state.
/// Defense-in-depth alongside the disabled-while-paused button.
#[tauri::command]
pub async fn poll_now(state: tauri::State<'_, crate::state::AppState>) -> AppResult<()> {
    if state.scheduler.wake() {
        Ok(())
    } else {
        Err(crate::error::AppError::new("轮询已暂停，请先恢复轮询"))
    }
}

/// Re-reads the poll period and rebuilds the loop's ticker (after a config save).
#[tauri::command]
pub async fn reschedule(state: tauri::State<'_, crate::state::AppState>) -> AppResult<()> {
    state.scheduler.reconfigure();
    Ok(())
}

/// Reports `gh` CLI auth status for the StatusBar.
#[tauri::command]
pub async fn gh_status() -> AppResult<GhStatus> {
    Ok(gh_auth_status("gh").await)
}

/// Returns the latest discovered PR list (the scheduler's snapshot) so the
/// frontend can render current state on mount without waiting for the next
/// `prs:updated` event (closes the startup lost-event race).
#[tauri::command]
pub fn get_prs(state: tauri::State<'_, crate::state::AppState>) -> AppResult<Vec<PullRequestView>> {
    Ok(state.scheduler.snapshot())
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
}
