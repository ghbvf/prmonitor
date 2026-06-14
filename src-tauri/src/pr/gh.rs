//! GitHub [`super::source::PrSource`] implementation via the `gh` CLI.
//!
//! `discover` shells out to `gh pr list --repo <repo> --state open --label
//! <label> --json ...` for each trigger label and maps the JSON into
//! [`Candidate`]s (gating) plus the display fields the PR list needs. Parsing is
//! split into pure functions (`parse_pr_list` / `to_row` / `merge_rows`) so the
//! `router.py` discovery semantics are unit-tested without invoking `gh`.

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::model::Candidate;

use super::source::PrSource;

/// `--json` field set requested from `gh pr list`. `router.py` only needs the
/// gating fields; the prmonitor UI additionally needs `title,url,labels`.
const PR_LIST_FIELDS: &str =
    "number,title,url,headRefName,headRefOid,author,isCrossRepository,isDraft,labels";

/// A discovered PR: its gating [`Candidate`] plus the display fields the PR list
/// shows. `conflict` marks a PR that carried BOTH trigger labels — `router.py`
/// drops these from dispatch; we surface them with a skip reason instead.
#[derive(Debug, Clone)]
pub struct GhRow {
    pub candidate: Candidate,
    pub title: String,
    pub url: String,
    pub labels: Vec<String>,
    pub conflict: bool,
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
}

impl GithubCli {
    pub fn new(repo: String, review_label: String, check_label: String) -> Self {
        Self {
            gh_bin: "gh".to_string(),
            repo,
            review_label,
            check_label,
        }
    }

    /// Runs `gh pr list` for one trigger label, returning raw stdout JSON.
    /// The blocking subprocess runs off the async executor via `spawn_blocking`.
    async fn run_pr_list(&self, label: &str) -> AppResult<String> {
        let gh = self.gh_bin.clone();
        let repo = self.repo.clone();
        let label = label.to_string();
        let label_arg = label.clone();

        let output = tauri::async_runtime::spawn_blocking(move || {
            std::process::Command::new(&gh)
                .args([
                    "pr",
                    "list",
                    "--repo",
                    &repo,
                    "--state",
                    "open",
                    "--label",
                    &label_arg,
                    "--json",
                    PR_LIST_FIELDS,
                ])
                .output()
        })
        .await
        .map_err(|e| AppError::new(format!("gh 子任务调度失败: {e}")))?
        .map_err(|e| AppError::new(format!("无法运行 gh（未安装或不在 PATH？）: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppError::new(format!(
                "gh pr list 失败（label={label}）: {}",
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Discovers open PRs carrying either trigger label as display-ready rows
    /// (conflict PRs included, marked). Two `gh` calls (review + check labels).
    pub async fn discover_rows(&self) -> AppResult<Vec<GhRow>> {
        let review = parse_pr_list(&self.run_pr_list(&self.review_label).await?)?
            .into_iter()
            .map(|r| to_row(r, "review"))
            .collect();
        let check = parse_pr_list(&self.run_pr_list(&self.check_label).await?)?
            .into_iter()
            .map(|r| to_row(r, "check"))
            .collect();
        Ok(merge_rows(review, check))
    }
}

impl PrSource for GithubCli {
    /// Trait view: gating [`Candidate`]s only, excluding conflict PRs (mirrors
    /// `router.py`, which drops both-label PRs before dispatch). The PR4
    /// scheduler depends on this; the PR3 list UI uses [`Self::discover_rows`].
    async fn discover(&self) -> AppResult<Vec<Candidate>> {
        Ok(self
            .discover_rows()
            .await?
            .into_iter()
            .filter(|row| !row.conflict)
            .map(|row| row.candidate)
            .collect())
    }
}

/// Probes `gh auth status` for the StatusBar. Never errors — any failure (gh
/// missing, not logged in, scheduling failure) maps to `authenticated: false`
/// with a human-readable message.
pub async fn gh_auth_status(gh_bin: &str) -> GhStatus {
    let gh = gh_bin.to_string();
    let result = tauri::async_runtime::spawn_blocking(move || {
        std::process::Command::new(&gh)
            .args(["auth", "status"])
            .output()
    })
    .await;

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
            message: "gh 状态检查调度失败".to_string(),
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
}
