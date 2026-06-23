//! GitHub [`super::source::EventSourceProvider`] implementation via the `gh` CLI.
//!
//! `discover` shells out to `gh pr list --repo <repo> --state open ... --json ...`
//! and maps the JSON into [`Candidate`]s (gating) plus the display fields the PR
//! list needs. The fetch shape depends on the project's [`crate::model::LabelSource`]
//! (AB#717):
//! - `Native` (status quo): one `--label <label>` call per trigger label, merged by
//!   PR number (`parse_pr_list` / `to_row` / `merge_rows`).
//! - `Title`: ONE all-open-PRs call (no `--label`), classified client-side from
//!   bracketed title segments (`to_row_classified`, sharing `super::labels::classify`).
//!
//! Parsing is split into pure functions so the `router.py` discovery semantics are
//! unit-tested without invoking `gh`.

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::error::{AppError, AppResult};
use crate::model::{Candidate, Event, EventType, LabelSource, SourceKind};

use super::labels;
use super::source::{pr_dedupe_key, DiscoveredEvent, EventSourceProvider};

/// Wall-clock budget for any single `gh` invocation. A hung subprocess (network
/// stall, auth prompt) is bounded here; `kill_on_drop(true)` means dropping the
/// timed-out (or cancelled) future kills the child — see [`GithubCli::run_pr_list`].
const GH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// `gh pr list` defaults to only 30 rows (AB#717 F6). The title-label path fetches ALL open
/// PRs (no `--label` server filter) and classifies client-side, so the default would silently
/// drop the 31st+ open PR; the native path's per-label list can also exceed 30. Pass an explicit
/// high `--limit` so neither path truncates. (gh caps it server-side at the repo's PR count.)
const GH_PR_LIST_LIMIT: u32 = 1000;

/// `--json` field set requested from `gh pr list`. `router.py` only needs the
/// gating fields; the prmonitor UI additionally needs `title,url,labels`.
const PR_LIST_FIELDS: &str =
    "number,title,url,headRefName,headRefOid,author,isCrossRepository,isDraft,labels";

/// A discovered PR: its gating [`Candidate`] plus the display fields the PR list
/// shows. `conflict` marks a PR that carried BOTH trigger labels — `router.py`
/// drops these from dispatch; we surface them with a skip reason instead.
#[derive(Debug, Clone)]
struct GhRow {
    candidate: Candidate,
    title: String,
    url: String,
    labels: Vec<String>,
    conflict: bool,
}

/// gh `--json author` shape (`{ "login": "octocat", ... }`, or null).
#[derive(Debug, Deserialize)]
struct RawAuthor {
    #[serde(default)]
    login: String,
}

/// gh `--json labels` element shape (`{ "name": "...", ... }`).
#[derive(Debug, Deserialize)]
struct RawLabel {
    #[serde(default)]
    name: String,
}

/// One row of `gh pr list --json <PR_LIST_FIELDS>` output.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPr {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    head_ref_name: String,
    #[serde(default)]
    head_ref_oid: String,
    #[serde(default)]
    author: Option<RawAuthor>,
    #[serde(default)]
    is_cross_repository: bool,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    labels: Vec<RawLabel>,
}

/// Parses `gh pr list --json ...` output (a JSON array) into raw rows.
fn parse_pr_list(json: &str) -> AppResult<Vec<RawPr>> {
    serde_json::from_str(json).map_err(|e| AppError::new(format!("解析 gh pr list JSON 失败: {e}")))
}

/// Maps one raw gh row + its trigger `kind` into a [`GhRow`] (mirrors
/// `router.py` `candidate_from_json`: author is the nested `login`, defaulting
/// to empty when the author object is null).
fn to_row(raw: RawPr, kind: &str) -> GhRow {
    let author = raw.author.map(|a| a.login).unwrap_or_default();
    let labels = raw
        .labels
        .into_iter()
        .map(|l| l.name)
        .filter(|n| !n.is_empty())
        .collect();
    GhRow {
        candidate: Candidate {
            number: raw.number,
            head_sha: raw.head_ref_oid,
            head_ref: raw.head_ref_name,
            author,
            is_cross_repository: raw.is_cross_repository,
            is_draft: raw.is_draft,
            kind: kind.to_string(),
        },
        title: raw.title,
        url: raw.url,
        labels,
        conflict: false,
    }
}

/// Maps one raw gh row into a [`GhRow`] for the title-label path (AB#717), returning
/// `None` when it carries no trigger label (not monitored). Unlike [`to_row`] (which
/// stamps a server-filtered `kind` and never conflicts), this resolves effective labels
/// via [`labels::effective_labels`] and derives `kind` + `conflict` client-side through
/// the shared [`labels::classify`] — the same classification the native two-call path
/// gets from `merge_rows`, but from a single all-open-PRs fetch.
fn to_row_classified(
    raw: RawPr,
    review_label: &str,
    check_label: &str,
    label_source: crate::model::LabelSource,
) -> Option<GhRow> {
    let native: Vec<String> = raw
        .labels
        .into_iter()
        .map(|l| l.name)
        .filter(|n| !n.is_empty())
        .collect();
    let labels = labels::effective_labels(native, &raw.title, label_source);
    let (kind, conflict) = labels::classify(&labels, review_label, check_label)?;
    let author = raw.author.map(|a| a.login).unwrap_or_default();
    Some(GhRow {
        candidate: Candidate {
            number: raw.number,
            head_sha: raw.head_ref_oid,
            head_ref: raw.head_ref_name,
            author,
            is_cross_repository: raw.is_cross_repository,
            is_draft: raw.is_draft,
            kind: kind.to_string(),
        },
        title: raw.title,
        url: raw.url,
        labels,
        conflict,
    })
}

/// Merges the review-labelled and check-labelled rows by PR number, marking a
/// PR present under both labels as a `conflict` (mirrors `router.py`
/// `discover_candidates`). The `BTreeMap` yields rows sorted by PR number.
fn merge_rows(review: Vec<GhRow>, check: Vec<GhRow>) -> Vec<GhRow> {
    use std::collections::BTreeMap;

    let mut by_pr: BTreeMap<u64, GhRow> = BTreeMap::new();
    for row in review {
        by_pr.insert(row.candidate.number, row);
    }
    for row in check {
        match by_pr.get_mut(&row.candidate.number) {
            // Already seen under the review label → both labels present.
            Some(existing) => existing.conflict = true,
            None => {
                by_pr.insert(row.candidate.number, row);
            }
        }
    }
    by_pr.into_values().collect()
}

/// Builds a [`DiscoveredEvent`] from one display-ready [`GhRow`] (AB#1070): the
/// normalized AB#1079 [`Event`] is constructed from the SAME already-parsed locals the
/// row holds, alongside the row's gating [`Candidate`] and `conflict` flag. `repo` is the
/// impl's monitored repo (`owner/name`). `project_id` / `received_at_epoch` are left at
/// zero/empty values for the future inbox (AB#1065) to stamp on ingest (the source can't know
/// the matched project id or the receive time). Always a `PullRequest` event (the only class a
/// PR source emits).
fn row_into_event(row: GhRow, repo: &str) -> DiscoveredEvent {
    let event = Event {
        // Wire literal "github" matches `SourceKind::Github`'s serde string (format single-sourced).
        dedupe_key: pr_dedupe_key(
            "github",
            repo,
            row.candidate.number,
            &row.candidate.head_sha,
        ),
        source: SourceKind::Github,
        event_type: EventType::PullRequest,
        project_id: String::new(),
        repo: repo.to_string(),
        number: Some(row.candidate.number),
        title: row.title.clone(),
        // body 抓取留待 AB#1068（rule engine）
        body: String::new(),
        labels: row.labels.clone(),
        url: row.url.clone(),
        received_at_epoch: 0,
    };
    DiscoveredEvent {
        event,
        candidate: row.candidate,
        conflict: row.conflict,
    }
}

/// `gh` CLI auth status reported to the StatusBar (pr-slice-private wire type;
/// not a cross-slice contract, so it lives here and is mirrored in
/// `src/pr/types.ts`, not `model.rs` / `src/types.ts`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GhStatus {
    pub authenticated: bool,
    pub message: String,
}

/// The GitHub PR source backed by the `gh` CLI.
pub struct GithubCli {
    gh_bin: String,
    repo: String,
    review_label: String,
    check_label: String,
    label_source: LabelSource,
}

impl GithubCli {
    pub fn new(
        repo: String,
        review_label: String,
        check_label: String,
        label_source: LabelSource,
    ) -> Self {
        Self {
            gh_bin: "gh".to_string(),
            repo,
            review_label,
            check_label,
            label_source,
        }
    }

    /// Runs `gh pr list` for one trigger label, returning raw stdout JSON.
    /// Uses async `tokio::process` with a [`GH_TIMEOUT`] bound and
    /// `kill_on_drop(true)`: if this future is dropped (timeout, or the
    /// scheduler's stop-select tearing down an in-flight cycle), the child `gh`
    /// process is killed — closing the cancellation domain (F1).
    async fn run_pr_list(&self, label: Option<&str>) -> AppResult<String> {
        let mut cmd = Command::new(&self.gh_bin);
        cmd.args(["pr", "list", "--repo", &self.repo, "--state", "open"]);
        // AB#717: the native path filters server-side per trigger label (one call each);
        // the title-label path passes `None` to fetch ALL open PRs and classify client-side
        // (a title tag isn't a real label, so `--label` would match nothing).
        if let Some(label) = label {
            cmd.args(["--label", label]);
        }
        // F6: override gh's default 30-row cap so neither path silently truncates.
        let limit_str = GH_PR_LIST_LIMIT.to_string();
        cmd.args(["--limit", &limit_str, "--json", PR_LIST_FIELDS])
            .kill_on_drop(true);

        let ctx = label
            .map(|l| format!("label={l}"))
            .unwrap_or_else(|| "all open".to_string());
        let output = match tokio::time::timeout(GH_TIMEOUT, cmd.output()).await {
            Err(_) => return Err(AppError::new(format!("gh pr list 超时（{ctx}）"))),
            Ok(Err(e)) => {
                return Err(AppError::new(format!(
                    "无法运行 gh（未安装或不在 PATH？）: {e}"
                )))
            }
            Ok(Ok(output)) => output,
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppError::new(format!(
                "gh pr list 失败（{ctx}）: {}",
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Discovers open PRs carrying either trigger label as display-ready rows
    /// (conflict PRs included, marked). Branches on the project's [`LabelSource`]
    /// (AB#717):
    /// - [`LabelSource::Native`] (status quo): two `gh` calls (review + check labels)
    ///   with server-side `--label` filtering, merged by PR number.
    /// - [`LabelSource::Title`]: ONE `gh` call for all open PRs, then client-side
    ///   classify from bracketed title segments (`--label` can't match a title tag).
    async fn discover_rows(&self) -> AppResult<Vec<GhRow>> {
        match self.label_source {
            LabelSource::Native => {
                let review = parse_pr_list(&self.run_pr_list(Some(&self.review_label)).await?)?
                    .into_iter()
                    .map(|r| to_row(r, "review"))
                    .collect();
                let check = parse_pr_list(&self.run_pr_list(Some(&self.check_label)).await?)?
                    .into_iter()
                    .map(|r| to_row(r, "check"))
                    .collect();
                Ok(merge_rows(review, check))
            }
            LabelSource::Title => {
                let mut rows: Vec<GhRow> = parse_pr_list(&self.run_pr_list(None).await?)?
                    .into_iter()
                    .filter_map(|raw| {
                        to_row_classified(
                            raw,
                            &self.review_label,
                            &self.check_label,
                            self.label_source,
                        )
                    })
                    .collect();
                // Sort by PR number for parity with the native path's BTreeMap order.
                rows.sort_by_key(|r| r.candidate.number);
                Ok(rows)
            }
        }
    }
}

impl EventSourceProvider for GithubCli {
    /// Discovers open PRs (conflict rows included, marked) as normalized AB#1079
    /// [`DiscoveredEvent`]s (AB#1070): the live producer. The display-ready rows from
    /// [`Self::discover_rows`] are each mapped to a `PullRequest` [`Event`] + gating
    /// [`Candidate`] via [`row_into_event`], so the event pipeline (epic AB#1078) is fed
    /// directly WITHOUT losing the review-gating candidate.
    async fn discover_events(&self) -> AppResult<Vec<DiscoveredEvent>> {
        Ok(self
            .discover_rows()
            .await?
            .into_iter()
            .map(|row| row_into_event(row, &self.repo))
            .collect())
    }
}

/// Probes `gh auth status` for the StatusBar. Never errors — any failure (gh
/// missing, not logged in, scheduling failure) maps to `authenticated: false`
/// with a human-readable message.
pub async fn gh_auth_status(gh_bin: &str) -> GhStatus {
    let mut cmd = Command::new(gh_bin);
    cmd.args(["auth", "status"]).kill_on_drop(true);
    let result = tokio::time::timeout(GH_TIMEOUT, cmd.output()).await;

    match result {
        Ok(Ok(output)) if output.status.success() => {
            // `gh auth status` prints the account summary to stderr.
            let text = String::from_utf8_lossy(&output.stderr);
            let summary = text
                .lines()
                .map(str::trim)
                .find(|line| line.contains("Logged in"))
                .unwrap_or("gh 已认证")
                .to_string();
            GhStatus {
                authenticated: true,
                message: summary,
            }
        }
        Ok(Ok(_)) => GhStatus {
            authenticated: false,
            message: "gh 未认证（运行 gh auth login）".to_string(),
        },
        Ok(Err(_)) => GhStatus {
            authenticated: false,
            message: "未找到 gh CLI（请安装并登录）".to_string(),
        },
        Err(_) => GhStatus {
            authenticated: false,
            message: "gh 状态检查超时".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pr_list_maps_fields_and_kind() {
        let json = r#"[
            {
                "number": 12,
                "title": "Add widget",
                "url": "https://github.com/o/r/pull/12",
                "headRefName": "feature/widget",
                "headRefOid": "abc123",
                "author": {"login": "octocat"},
                "isCrossRepository": false,
                "isDraft": false,
                "labels": [{"name": "pr-status/needs-review-again"}, {"name": "area/ui"}]
            }
        ]"#;
        let rows: Vec<GhRow> = parse_pr_list(json)
            .expect("parses")
            .into_iter()
            .map(|r| to_row(r, "review"))
            .collect();

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.candidate.number, 12);
        assert_eq!(row.candidate.head_sha, "abc123");
        assert_eq!(row.candidate.head_ref, "feature/widget");
        assert_eq!(row.candidate.author, "octocat");
        assert_eq!(row.candidate.kind, "review");
        assert!(!row.candidate.is_cross_repository);
        assert!(!row.candidate.is_draft);
        assert_eq!(row.title, "Add widget");
        assert_eq!(row.url, "https://github.com/o/r/pull/12");
        assert_eq!(row.labels, vec!["pr-status/needs-review-again", "area/ui"]);
        assert!(!row.conflict);

        // AB#1070: the same row maps to a normalized `Event` (PullRequest) on a
        // `DiscoveredEvent`, built from the SAME parsed locals, while the gating
        // `Candidate` rides along unchanged.
        let de = row_into_event(rows[0].clone(), "o/r");
        assert_eq!(de.event.source, SourceKind::Github);
        assert_eq!(de.event.event_type, EventType::PullRequest);
        assert_eq!(de.event.number, Some(de.candidate.number));
        assert_eq!(de.event.title, "Add widget");
        assert_eq!(de.event.url, "https://github.com/o/r/pull/12");
        assert_eq!(
            de.event.labels,
            vec!["pr-status/needs-review-again", "area/ui"]
        );
        assert_eq!(de.event.body, "");
        // dedupe_key is the exact inbox idempotency-key seed (format single-sourced).
        assert_eq!(de.event.dedupe_key, "github:pullRequest:o/r#12@abc123");
        assert!(!de.conflict);
    }

    #[test]
    fn to_row_defaults_null_author_and_draft_fork_flags() {
        let json = r#"[
            {
                "number": 5,
                "headRefOid": "def456",
                "author": null,
                "isCrossRepository": true,
                "isDraft": true
            }
        ]"#;
        let row = to_row(parse_pr_list(json).expect("parses").pop().unwrap(), "check");
        assert_eq!(row.candidate.author, ""); // null author → empty login
        assert!(row.candidate.is_cross_repository);
        assert!(row.candidate.is_draft);
        assert_eq!(row.candidate.kind, "check");
        assert_eq!(row.title, ""); // missing optional display fields default empty
        assert!(row.labels.is_empty());
    }

    #[test]
    fn parse_pr_list_empty_array() {
        let rows = parse_pr_list("[]").expect("parses empty");
        assert!(rows.is_empty());
    }

    #[test]
    fn parse_pr_list_rejects_non_array() {
        assert!(parse_pr_list("{\"number\": 1}").is_err());
        assert!(parse_pr_list("not json").is_err());
    }

    fn row(number: u64, kind: &str) -> GhRow {
        to_row(
            RawPr {
                number,
                title: format!("PR {number}"),
                url: format!("https://x/{number}"),
                head_ref_name: "ref".to_string(),
                head_ref_oid: "sha".to_string(),
                author: Some(RawAuthor {
                    login: "octocat".to_string(),
                }),
                is_cross_repository: false,
                is_draft: false,
                labels: vec![],
            },
            kind,
        )
    }

    #[test]
    fn merge_rows_marks_both_label_conflict_and_sorts() {
        let review = vec![row(3, "review"), row(1, "review")];
        let check = vec![row(1, "check"), row(2, "check")];
        let merged = merge_rows(review, check);

        // Sorted by number: 1, 2, 3.
        let numbers: Vec<u64> = merged.iter().map(|r| r.candidate.number).collect();
        assert_eq!(numbers, vec![1, 2, 3]);

        // PR 1 carried both labels → conflict; PRs 2 and 3 do not.
        let pr1 = merged.iter().find(|r| r.candidate.number == 1).unwrap();
        assert!(pr1.conflict);
        assert!(
            !merged
                .iter()
                .find(|r| r.candidate.number == 2)
                .unwrap()
                .conflict
        );
        assert!(
            !merged
                .iter()
                .find(|r| r.candidate.number == 3)
                .unwrap()
                .conflict
        );
    }

    #[test]
    fn merge_rows_check_only_keeps_check_kind() {
        let merged = merge_rows(vec![], vec![row(2, "check")]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].candidate.kind, "check");
        assert!(!merged[0].conflict);
    }

    #[test]
    fn merge_rows_review_only_keeps_review_kind() {
        let merged = merge_rows(vec![row(1, "review")], vec![]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].candidate.kind, "review");
        assert!(!merged[0].conflict);
    }

    // AB#717: the title-label path resolves effective labels from the PR title and
    // classifies client-side, ignoring native labels.
    #[test]
    fn to_row_classified_uses_title_tags_and_ignores_native_labels() {
        use crate::model::LabelSource;
        const REVIEW: &str = "pr-status/needs-review-again";
        const CHECK: &str = "pr-status/needs-check-fix";

        let raw = RawPr {
            number: 7,
            title: format!("Fix login [{REVIEW}]"),
            url: "https://x/7".to_string(),
            head_ref_name: "fix".to_string(),
            head_ref_oid: "sha".to_string(),
            author: Some(RawAuthor {
                login: "octocat".to_string(),
            }),
            is_cross_repository: false,
            is_draft: false,
            // Native label is ignored under Title mode.
            labels: vec![RawLabel {
                name: "area/ui".to_string(),
            }],
        };
        let row = to_row_classified(raw, REVIEW, CHECK, LabelSource::Title).expect("monitored");
        assert_eq!(row.candidate.kind, "review");
        assert_eq!(row.labels, vec![REVIEW.to_string()]);
        assert!(!row.conflict);

        // Native-only trigger label (no title tag) → not monitored under Title mode.
        let raw_native_only = RawPr {
            number: 8,
            title: "No tags".to_string(),
            url: "https://x/8".to_string(),
            head_ref_name: "x".to_string(),
            head_ref_oid: "sha".to_string(),
            author: None,
            is_cross_repository: false,
            is_draft: false,
            labels: vec![RawLabel {
                name: REVIEW.to_string(),
            }],
        };
        assert!(to_row_classified(raw_native_only, REVIEW, CHECK, LabelSource::Title).is_none());

        // Both trigger tags in the title → conflict, kept with kind "review".
        let raw_both = RawPr {
            number: 9,
            title: format!("[{REVIEW}][{CHECK}] both"),
            url: "https://x/9".to_string(),
            head_ref_name: "x".to_string(),
            head_ref_oid: "sha".to_string(),
            author: None,
            is_cross_repository: false,
            is_draft: false,
            labels: vec![],
        };
        let row_both =
            to_row_classified(raw_both, REVIEW, CHECK, LabelSource::Title).expect("kept");
        assert!(row_both.conflict);
        assert_eq!(row_both.candidate.kind, "review");
    }

    // Wire-shape lock for `GhStatus` — the `gh_status` command's front/back wire
    // type, mirrored in `src/pr/types.ts` (Medium carrier per ai-robust.md; a
    // field rename would otherwise drift the TS mirror silently).
    #[test]
    fn gh_status_wire_shape_is_camel_case() {
        let v = serde_json::to_value(GhStatus {
            authenticated: true,
            message: "ok".to_string(),
        })
        .expect("GhStatus serializes");
        assert!(v.get("authenticated").is_some());
        assert!(v.get("message").is_some());
    }
}
