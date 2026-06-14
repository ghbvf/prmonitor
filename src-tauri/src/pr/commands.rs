//! PR slice Tauri commands: manual fetch + `gh` auth status.

use crate::config::service as config_service;
use crate::error::AppResult;
use crate::model::PullRequestView;

use super::discover::{self, MonitorParams};
use super::gh::{gh_auth_status, GhRow, GhStatus, GithubCli};
use super::ledger::Ledger;

/// Wall-clock seconds since the Unix epoch (the cooldown clock). A pre-epoch
/// system clock degrades to 0 rather than panicking.
fn now_epoch() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Annotates one discovered row with its static skip reason for the PR list.
/// Conflict (both trigger labels) skips first, matching `router.py`'s discovery-
/// stage drop; otherwise static gates then cooldown. The live gate is the
/// dispatch-time check (PR4/PR5), not run here.
fn build_view(row: GhRow, params: &MonitorParams, ledger: &Ledger, now: u64) -> PullRequestView {
    let skip_reason = if row.conflict {
        Some(discover::BOTH_TRIGGER_LABELS_REASON.to_string())
    } else {
        discover::should_skip(&row.candidate, params, ledger)
            .or_else(|| discover::cooldown_skip(&row.candidate, params, ledger, now))
    };
    PullRequestView {
        number: row.candidate.number,
        title: row.title,
        labels: row.labels,
        url: row.url,
        kind: row.candidate.kind,
        skip_reason,
    }
}

/// Discovers the monitored repo's open trigger-labelled PRs now and returns them
/// annotated with `kind` + skip reason for the PR list. Reads config (repo,
/// labels, authors, cooldown) and the dedup ledger; performs two `gh pr list`
/// calls (review + check labels).
///
/// This is the shared discovery body driven by the scheduler's poll loop
/// (`scheduler::run_and_emit`); there is no manual-fetch command — the frontend
/// triggers a refresh via `poll_now`.
pub(crate) async fn discover_views<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> AppResult<Vec<PullRequestView>> {
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
    Ok(rows
        .into_iter()
        .map(|row| build_view(row, &params, &ledger, now))
        .collect())
}

/// Starts the scheduled-pull loop (idempotent: a no-op if already running).
#[tauri::command]
pub async fn start_polling<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<()> {
    state.scheduler.start(app);
    Ok(())
}

/// Stops the scheduled-pull loop (no-op if not running).
#[tauri::command]
pub async fn stop_polling(state: tauri::State<'_, crate::state::AppState>) -> AppResult<()> {
    state.scheduler.stop();
    Ok(())
}

/// Triggers an immediate discovery cycle ("立即拉取").
#[tauri::command]
pub async fn poll_now(state: tauri::State<'_, crate::state::AppState>) -> AppResult<()> {
    state.scheduler.wake();
    Ok(())
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
    fn build_view_clean_row_has_no_skip_reason() {
        let view = build_view(row(1, "review", false), &params(), &Ledger::default(), 0);
        assert_eq!(view.number, 1);
        assert_eq!(view.kind, "review");
        assert_eq!(view.title, "PR 1");
        assert_eq!(view.skip_reason, None);
    }

    #[test]
    fn build_view_conflict_row_skips_with_both_labels_reason() {
        let view = build_view(row(2, "check", true), &params(), &Ledger::default(), 0);
        assert_eq!(
            view.skip_reason,
            Some("both review and check trigger labels are present".to_string())
        );
    }

    #[test]
    fn build_view_propagates_static_gate_skip() {
        let mut r = row(3, "review", false);
        r.candidate.is_draft = true;
        let view = build_view(r, &params(), &Ledger::default(), 0);
        assert_eq!(view.skip_reason, Some("draft PR".to_string()));
    }

    #[test]
    fn build_view_propagates_cooldown_skip() {
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
        let view = build_view(r, &params(), &ledger, 1_500);
        assert!(
            view.skip_reason
                .as_deref()
                .unwrap()
                .contains("within cooldown"),
            "{:?}",
            view.skip_reason
        );
    }
}
