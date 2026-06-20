//! Azure DevOps [`super::source::PrSource`] implementation via the `az` CLI (#818).
//!
//! `discover_rows` shells out to ONE `az repos pr list --organization
//! https://dev.azure.com/<org> --project <project> --repository <repo> --status
//! active --output json` call (unlike `gh.rs`'s per-label double call — Azure returns
//! every active PR with its labels in one shot) and maps the JSON into display-ready
//! [`AzRow`]s (the Azure analogue of `gh.rs`'s `GhRow`: gating [`Candidate`] plus the
//! title / url / labels the PR list shows, and a `conflict` flag for both-trigger-label
//! PRs). Parsing is split into the pure [`parse_rows`] so the discovery / label-classify
//! / display-field semantics are unit-tested without invoking `az`; [`parse_pr_list`] is
//! the thin gating wrapper (rows minus conflict → candidates) the trait view reuses.
//!
//! Mirrors `gh.rs`'s subprocess discipline: a [`AZ_TIMEOUT`] wall-clock bound and
//! `kill_on_drop(true)`, so a hung / cancelled discovery kills the child (the same F1
//! cancellation domain the scheduler relies on).

use std::process::Stdio;

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::error::{AppError, AppResult};
use crate::model::Candidate;

use super::source::PrSource;

/// Wall-clock budget for the single `az` invocation. A hung subprocess (network
/// stall, auth prompt) is bounded here; `kill_on_drop(true)` means dropping the
/// timed-out (or cancelled) future kills the child — mirrors `gh.rs`'s `GH_TIMEOUT`.
const AZ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Max `az` stdout we will read + parse (#818 F9 / #124 F6). A hostile / misconfigured `az`
/// could emit an enormous body; the bounded reader stops at this many bytes (erroring rather
/// than allocating unboundedly) BEFORE `serde_json::from_str`. 10 MiB is far above any
/// realistic active-PR JSON.
const AZ_MAX_STDOUT_BYTES: usize = 10 * 1024 * 1024;

/// Max `az` stderr we will read (#124 F6). Stderr only feeds [`stderr_tail`] (the last 512
/// chars), so a small cap suffices — 64 KiB is ample headroom for the tail plus any preceding
/// auth-debug while still bounding memory if `az` floods stderr. Reading is also drained
/// concurrently with stdout to avoid a pipe-buffer deadlock.
const AZ_STDERR_CAP: usize = 64 * 1024;

/// Max `az` stderr chars surfaced in a non-zero-exit error (#818 F1). `az`'s user-facing
/// error is at the END of stderr; the early lines are MSAL/ADAL auth debug where a token
/// could appear. Surfacing only the tail bounds the message AND avoids leaking early
/// auth-debug noise. The error keeps the `"az repos pr list 失败:"` prefix.
const AZ_STDERR_TAIL_CHARS: usize = 512;

/// The trailing `AZ_STDERR_TAIL_CHARS` chars of `stderr` (the user-facing tail), prefixed
/// with `…` when truncated. Counts CHARS (not bytes) so the slice never splits a UTF-8
/// boundary. Pure so the truncation is unit-tested.
fn stderr_tail(stderr: &str) -> String {
    let trimmed = stderr.trim();
    let char_count = trimmed.chars().count();
    if char_count <= AZ_STDERR_TAIL_CHARS {
        return trimmed.to_string();
    }
    let tail: String = trimmed
        .chars()
        .skip(char_count - AZ_STDERR_TAIL_CHARS)
        .collect();
    format!("…{tail}")
}

/// Reads `reader` to EOF but stops once more than `limit` bytes have arrived (#124 F6),
/// returning `Err(<over_size_msg>)` instead of allocating unboundedly. The bound is applied
/// at READ time: the output `Vec` never grows past `limit + 1` bytes (the one extra is what
/// proves the limit was exceeded), so a hostile / runaway stream can't exhaust memory before
/// a post-collection length check (the bug this replaces). Generic over [`AsyncRead`] so it
/// is unit-tested with an in-memory `&[u8]` (itself an `AsyncRead` under tokio).
async fn read_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
    over_size_msg: &str,
) -> AppResult<Vec<u8>> {
    let mut buf = Vec::new();
    // Read into a fixed scratch chunk so a single huge `read` can't pre-allocate the whole
    // body; append until EOF or the limit is exceeded.
    let mut chunk = [0u8; 8 * 1024];
    loop {
        let n = reader
            .read(&mut chunk)
            .await
            .map_err(|e| AppError::new(format!("读取 az 输出失败: {e}")))?;
        if n == 0 {
            return Ok(buf); // EOF within the limit.
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > limit {
            // Stop as soon as we exceed the bound; do NOT keep draining (memory stays ~limit).
            return Err(AppError::new(over_size_msg.to_string()));
        }
    }
}

/// Percent-encodes one URL PATH segment (#124 F5). The Azure org/project/repo config
/// allows spaces / unicode (they go to `az` as separate argv, no URL parsing), so building
/// the PR web URL by raw interpolation would yield a malformed link. This is a tiny inline
/// encoder (no new crate — `percent-encoding` is only a transitive dep): each UTF-8 byte
/// passes through iff it is RFC 3986 "unreserved" (`A-Za-z0-9-._~`), else it is emitted as
/// `%XX` (uppercase hex). Encoding the byte stream covers multi-byte UTF-8 correctly. A
/// path segment never contains `/`, so `/` is (correctly) encoded to `%2F` here.
fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for &byte in segment.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(
                char::from_digit((byte >> 4) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
            out.push(
                char::from_digit((byte & 0xf) as u32, 16)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
        }
    }
    out
}

/// `az repos pr list --createdBy` shape (`{ "uniqueName": "...", "displayName": "..." }`).
/// `uniqueName` is the account email/upn; `displayName` is the human name fallback.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCreatedBy {
    #[serde(default)]
    unique_name: String,
    #[serde(default)]
    display_name: String,
}

/// `az repos pr list --lastMergeSourceCommit` shape (`{ "commitId": "..." }`, or absent).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMergeCommit {
    #[serde(default)]
    commit_id: String,
}

/// `az repos pr list --labels` element shape (`{ "name": "...", ... }`).
#[derive(Debug, Deserialize)]
struct RawLabel {
    #[serde(default)]
    name: String,
}

/// One row of `az repos pr list --output json` output.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPr {
    pull_request_id: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    last_merge_source_commit: Option<RawMergeCommit>,
    #[serde(default)]
    source_ref_name: String,
    #[serde(default)]
    created_by: Option<RawCreatedBy>,
    #[serde(default)]
    is_draft: bool,
    /// `Option` (not bare `Vec`) because the REAL `az repos pr list` JSON emits `labels:
    /// null` (NOT `[]`) for a PR with no labels — the most common case. `#[serde(default)]`
    /// covers an absent key; the `Option` is what accepts an explicit JSON `null` (a bare
    /// `Vec` would error "invalid type: null, expected a sequence" and fail the WHOLE parse).
    /// `None` / absent both mean "no labels" → the PR carries no trigger label → skipped.
    #[serde(default)]
    labels: Option<Vec<RawLabel>>,
    /// Present (non-null) when the PR's source branch lives in a forked repo — the
    /// Azure analogue of gh's `isCrossRepository`. Absent on same-repo PRs.
    #[serde(default)]
    fork_source: Option<serde_json::Value>,
}

/// A discovered Azure PR: its gating [`Candidate`] plus the display fields the PR list
/// shows. The Azure analogue of `gh.rs`'s `GhRow` (same fields, same `conflict`
/// semantics) so the `pr` slice's view path treats both sources identically. `conflict`
/// marks a PR that carried BOTH trigger labels — kept (not dropped) here and surfaced
/// with a skip reason by `commands::build_view`, exactly like the gh path.
#[derive(Debug, Clone)]
pub struct AzRow {
    pub candidate: Candidate,
    pub title: String,
    pub labels: Vec<String>,
    pub url: String,
    pub conflict: bool,
}

/// Parses `az repos pr list --output json` output (a JSON array) into display-ready
/// [`AzRow`]s — the SINGLE Azure parser (gating + display). Mirrors the gh path's
/// `parse_pr_list` + `to_row` + `merge_rows` combined, including the `conflict`
/// convention:
///
/// - a PR carrying the `review_label` → `kind = "review"`, `conflict = false`;
/// - a PR carrying the `check_label` → `kind = "check"`, `conflict = false`;
/// - BOTH trigger labels present → `kind = "review"` (mirrors gh `merge_rows`, which
///   keeps the review row and flips `conflict`), `conflict = true` — the row is KEPT so
///   the list surfaces it with a skip reason (the trait [`parse_pr_list`] drops it
///   before dispatch);
/// - NEITHER trigger label → dropped (not a monitored PR).
///
/// Display fields: `title` is captured verbatim; `labels` collects ALL label names (not
/// just the trigger); `url` is the constructed Azure PR web URL
/// `https://dev.azure.com/{org}/{project}/_git/{repo}/pullrequest/{pullRequestId}`.
///
/// Candidate field mapping: `pullRequestId` → `number`; `lastMergeSourceCommit.commitId`
/// → `head_sha` (absent / null → ""); `sourceRefName` with a leading `refs/heads/`
/// stripped → `head_ref`; `createdBy.uniqueName` (falling back to `displayName`) →
/// `author`; `isDraft` → `is_draft`; `forkSource` present/non-null →
/// `is_cross_repository`.
///
/// Pure (no `az` call) so the classify / conflict / field-mapping / url-build semantics
/// are unit-tested without a live Azure DevOps connection.
pub fn parse_rows(
    json: &str,
    org: &str,
    project: &str,
    repo: &str,
    review_label: &str,
    check_label: &str,
) -> AppResult<Vec<AzRow>> {
    let raw: Vec<RawPr> = serde_json::from_str(json)
        .map_err(|e| AppError::new(format!("解析 az repos pr list JSON 失败: {e}")))?;

    let mut rows = Vec::new();
    for pr in raw {
        // `labels` is `None` for `null` / absent (the real Azure shape for a no-label PR);
        // `.iter().flatten()` yields nothing in that case → no trigger label → dropped below.
        let labels: Vec<String> = pr
            .labels
            .iter()
            .flatten()
            .map(|l| l.name.clone())
            .filter(|n| !n.is_empty())
            .collect();
        let has_review = labels.iter().any(|n| n == review_label);
        let has_check = labels.iter().any(|n| n == check_label);

        // Classify by trigger label, mirroring gh `merge_rows`: both labels → conflict,
        // kept with kind "review" (the review row gh keeps); neither → not monitored,
        // dropped; exactly one → that kind, no conflict.
        let (kind, conflict) = match (has_review, has_check) {
            (true, true) => ("review", true),
            (true, false) => ("review", false),
            (false, true) => ("check", false),
            (false, false) => continue, // no trigger label: not a monitored PR
        };

        let head_sha = pr
            .last_merge_source_commit
            .map(|c| c.commit_id)
            .unwrap_or_default();
        // `sourceRefName` is a full git ref ("refs/heads/feature/x"); strip the
        // `refs/heads/` prefix to the branch name the candidate carries.
        let head_ref = pr
            .source_ref_name
            .strip_prefix("refs/heads/")
            .unwrap_or(&pr.source_ref_name)
            .to_string();
        // Author: prefer the account upn/email (`uniqueName`), fall back to the human
        // `displayName` when it is empty, then to "" (mirrors gh's null-author default).
        let author = pr
            .created_by
            .map(|c| {
                if c.unique_name.is_empty() {
                    c.display_name
                } else {
                    c.unique_name
                }
            })
            .unwrap_or_default();
        // The Azure PR web URL (constructed — Azure's `az` JSON has no ready web url field).
        // Each path segment is percent-encoded (#124 F5): config allows spaces / unicode in
        // org/project/repo (separate argv, no URL parsing), so a raw interpolation would
        // produce a malformed URL — encode them so the link is well-formed.
        let url = format!(
            "https://dev.azure.com/{}/{}/_git/{}/pullrequest/{}",
            encode_path_segment(org),
            encode_path_segment(project),
            encode_path_segment(repo),
            pr.pull_request_id
        );

        rows.push(AzRow {
            candidate: Candidate {
                number: pr.pull_request_id,
                head_sha,
                head_ref,
                author,
                is_cross_repository: pr.fork_source.is_some(),
                is_draft: pr.is_draft,
                kind: kind.to_string(),
            },
            title: pr.title,
            labels,
            url,
            conflict,
        });
    }
    Ok(rows)
}

/// The gating projection of discovered rows (#818 F10): the [`Candidate`]s, EXCLUDING
/// conflict rows (mirrors gh.rs `PrSource::discover`, which filters `conflict` before
/// dispatch). The SINGLE canonical "rows → dispatch candidates" step shared by both the
/// trait [`PrSource::discover`] (over live `discover_rows`) and [`parse_pr_list`] (over a
/// parsed JSON string), so the conflict-drop logic lives in exactly one place.
fn rows_into_candidates(rows: Vec<AzRow>) -> Vec<Candidate> {
    rows.into_iter()
        .filter(|row| !row.conflict)
        .map(|row| row.candidate)
        .collect()
}

/// Gating-only view over [`parse_rows`]: the [`Candidate`]s, EXCLUDING conflict rows (via
/// [`rows_into_candidates`]). The url args don't affect the candidate shape, so callers
/// that only want candidates pass the project's org/project/repo. `#[cfg(test)]`: the live
/// path goes through `discover_rows` → trait `discover` → [`rows_into_candidates`], so this
/// string-input convenience exists ONLY to exercise the gating projection in unit tests.
#[cfg(test)]
fn parse_pr_list(
    json: &str,
    org: &str,
    project: &str,
    repo: &str,
    review_label: &str,
    check_label: &str,
) -> AppResult<Vec<Candidate>> {
    Ok(rows_into_candidates(parse_rows(
        json,
        org,
        project,
        repo,
        review_label,
        check_label,
    )?))
}

/// The Azure DevOps PR source backed by the `az` CLI (#818).
pub struct AzureDevOpsCli {
    az_bin: String,
    org: String,
    project: String,
    repo: String,
    review_label: String,
    check_label: String,
}

impl AzureDevOpsCli {
    pub fn new(
        org: String,
        project: String,
        repo: String,
        review_label: String,
        check_label: String,
    ) -> Self {
        Self {
            az_bin: "az".to_string(),
            org,
            project,
            repo,
            review_label,
            check_label,
        }
    }

    /// Runs the single `az repos pr list` call, returning raw stdout JSON. Async
    /// `tokio::process` with an [`AZ_TIMEOUT`] bound and `kill_on_drop(true)`: if this
    /// future is dropped (timeout, or the scheduler's stop-select tearing down an
    /// in-flight cycle) the child `az` process is killed — closing the cancellation
    /// domain (F1 parity with `gh.rs`).
    async fn run_pr_list(&self) -> AppResult<String> {
        let org_url = format!("https://dev.azure.com/{}", self.org);
        let mut cmd = Command::new(&self.az_bin);
        cmd.args([
            "repos",
            "pr",
            "list",
            "--organization",
            &org_url,
            "--project",
            &self.project,
            "--repository",
            &self.repo,
            "--status",
            "active",
            "--output",
            "json",
        ])
        // Pipe stdout + stderr so we can BOUND them at read time (#124 F6) rather than letting
        // `output()` buffer the whole body first. `kill_on_drop` keeps the cancellation domain
        // (a dropped future kills the child).
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| AppError::new(format!("无法运行 az（未安装或不在 PATH？）: {e}")))?;
        // Take the pipe handles. They are `Some` because we requested `Stdio::piped()` above.
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::new("az stdout 管道缺失"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::new("az stderr 管道缺失"))?;

        // Drain stdout AND stderr CONCURRENTLY under one timeout (#124 F6). Reading them
        // concurrently is REQUIRED: reading stdout to EOF first can deadlock if the child
        // fills the stderr pipe buffer (and vice-versa) — neither side drains, both block.
        // `try_join!` polls both; stdout is bounded at the parse limit, stderr at a small cap
        // (it only feeds the error tail).
        let drained = tokio::time::timeout(AZ_TIMEOUT, async {
            tokio::try_join!(
                read_bounded(stdout, AZ_MAX_STDOUT_BYTES, "az repos pr list 输出过大"),
                read_bounded(stderr, AZ_STDERR_CAP, "az repos pr list 错误输出过大"),
            )
        })
        .await;
        let (stdout_bytes, stderr_bytes) = match drained {
            Err(_) => return Err(AppError::new("az repos pr list 超时")),
            Ok(Err(e)) => return Err(e), // a bounded-read error (over-size / IO)
            Ok(Ok(pair)) => pair,
        };

        // Reap the child for its exit status (the pipes are at EOF, so this won't block).
        let status = child
            .wait()
            .await
            .map_err(|e| AppError::new(format!("等待 az 退出失败: {e}")))?;

        if !status.success() {
            // Surface only the trailing tail of stderr (F1): bounds the message and avoids
            // leaking early MSAL/ADAL auth-debug (where a token could appear). The stderr is
            // already byte-bounded by `read_bounded`, so this never sees an unbounded buffer.
            let stderr = String::from_utf8_lossy(&stderr_bytes);
            return Err(AppError::new(format!(
                "az repos pr list 失败: {}",
                stderr_tail(&stderr)
            )));
        }
        // stdout is already bounded at read time (no redundant post-collection length check).
        Ok(String::from_utf8_lossy(&stdout_bytes).into_owned())
    }

    /// Discovers active PRs carrying either trigger label as display-ready [`AzRow`]s
    /// (conflict PRs included, marked) — the Azure analogue of `gh.rs`'s `discover_rows`,
    /// so the `pr` slice's view path treats both sources identically. ONE `az` call (vs
    /// gh's two), then [`parse_rows`] (passing the org/project/repo so the row url is
    /// constructed).
    pub async fn discover_rows(&self) -> AppResult<Vec<AzRow>> {
        let json = self.run_pr_list().await?;
        parse_rows(
            &json,
            &self.org,
            &self.project,
            &self.repo,
            &self.review_label,
            &self.check_label,
        )
    }
}

impl PrSource for AzureDevOpsCli {
    /// Trait view: gating [`Candidate`]s only, excluding conflict PRs (mirrors gh.rs's
    /// `discover`, which drops both-label PRs before dispatch). The list UI uses
    /// [`Self::discover_rows`] for the full display fields.
    async fn discover(&self) -> AppResult<Vec<Candidate>> {
        Ok(rows_into_candidates(self.discover_rows().await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVIEW: &str = "pr-status/needs-review-again";
    const CHECK: &str = "pr-status/needs-check-fix";
    const ORG: &str = "myorg";
    const PROJECT: &str = "myproject";
    const REPO: &str = "myrepo";

    /// `parse_rows` with the test org/project/repo wired in (so each case only passes the
    /// JSON + labels). The single Azure parser under test.
    fn rows(json: &str) -> AppResult<Vec<AzRow>> {
        parse_rows(json, ORG, PROJECT, REPO, REVIEW, CHECK)
    }

    /// `parse_pr_list` (the trait gating view: rows minus conflict → candidates) with the
    /// test org/project/repo wired in.
    fn candidates(json: &str) -> AppResult<Vec<Candidate>> {
        parse_pr_list(json, ORG, PROJECT, REPO, REVIEW, CHECK)
    }

    #[test]
    fn parse_rows_maps_review_pr_with_title_url_and_all_labels() {
        let json = format!(
            r#"[
                {{
                    "pullRequestId": 12,
                    "title": "Add widget",
                    "lastMergeSourceCommit": {{ "commitId": "abc123" }},
                    "sourceRefName": "refs/heads/feature/widget",
                    "createdBy": {{ "uniqueName": "octocat@example.com", "displayName": "Octo Cat" }},
                    "isDraft": false,
                    "labels": [{{ "active": true, "id": "1", "name": "{REVIEW}", "url": "u" }}, {{ "name": "area/ui" }}]
                }}
            ]"#
        );
        let r = rows(&json).expect("parses");
        assert_eq!(r.len(), 1);
        let row = &r[0];
        // Candidate fields.
        assert_eq!(row.candidate.number, 12);
        assert_eq!(row.candidate.head_sha, "abc123");
        assert_eq!(row.candidate.head_ref, "feature/widget"); // refs/heads/ stripped
        assert_eq!(row.candidate.author, "octocat@example.com"); // uniqueName preferred
        assert_eq!(row.candidate.kind, "review");
        assert!(!row.candidate.is_cross_repository);
        assert!(!row.candidate.is_draft);
        // Display fields (the parity goal): real title, ALL labels, constructed url.
        assert_eq!(row.title, "Add widget");
        assert_eq!(row.labels, vec![REVIEW.to_string(), "area/ui".to_string()]);
        assert_eq!(
            row.url,
            "https://dev.azure.com/myorg/myproject/_git/myrepo/pullrequest/12"
        );
        assert!(!row.conflict);
    }

    // #124 F5: the backend allows spaces / unicode in Azure project & repo names (separate
    // argv, no URL parsing — see config validate), so the constructed PR web URL must
    // percent-encode each path segment or it is malformed. Golden: a space → "%20".
    #[test]
    fn encode_path_segment_percent_encodes_unsafe_chars() {
        // A plain unreserved segment is unchanged.
        assert_eq!(encode_path_segment("myrepo"), "myrepo");
        assert_eq!(encode_path_segment("my-repo_v2.0~x"), "my-repo_v2.0~x");
        // A space → %20.
        assert_eq!(encode_path_segment("my project"), "my%20project");
        // Other reserved / non-ASCII bytes are %XX (uppercase hex), e.g. `/`, `#`, unicode.
        assert_eq!(encode_path_segment("a/b#c"), "a%2Fb%23c");
        assert_eq!(encode_path_segment("café"), "caf%C3%A9"); // é = U+00E9 → UTF-8 C3 A9
    }

    #[test]
    fn parse_rows_url_percent_encodes_segments_with_spaces() {
        // A project/repo carrying a space (allowed by config) yields a well-formed URL with
        // "%20" in the segment, NOT a raw space.
        let json = format!(
            r#"[
                {{
                    "pullRequestId": 12,
                    "title": "Add widget",
                    "lastMergeSourceCommit": {{ "commitId": "abc123" }},
                    "sourceRefName": "refs/heads/x",
                    "createdBy": {{ "uniqueName": "dev@example.com" }},
                    "isDraft": false,
                    "labels": [{{ "name": "{REVIEW}" }}]
                }}
            ]"#
        );
        let r =
            parse_rows(&json, "my org", "my project", "my repo", REVIEW, CHECK).expect("parses");
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0].url,
            "https://dev.azure.com/my%20org/my%20project/_git/my%20repo/pullrequest/12"
        );
    }

    #[test]
    fn parse_rows_classifies_check_label() {
        let json = format!(
            r#"[
                {{
                    "pullRequestId": 5,
                    "title": "Fix it",
                    "lastMergeSourceCommit": {{ "commitId": "def456" }},
                    "sourceRefName": "refs/heads/fix",
                    "createdBy": {{ "uniqueName": "dev@example.com" }},
                    "isDraft": false,
                    "labels": [{{ "name": "{CHECK}" }}]
                }}
            ]"#
        );
        let r = rows(&json).expect("parses");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].candidate.number, 5);
        assert_eq!(r[0].candidate.kind, "check");
        assert_eq!(r[0].title, "Fix it");
        assert_eq!(
            r[0].url,
            "https://dev.azure.com/myorg/myproject/_git/myrepo/pullrequest/5"
        );
        assert!(!r[0].conflict);
    }

    #[test]
    fn parse_rows_keeps_both_label_conflict_with_review_kind() {
        // gh `merge_rows` parity: a PR carrying BOTH trigger labels is KEPT as a row, flagged
        // `conflict`, with kind "review" (the review row gh keeps). `commands::build_view`
        // turns the flag into a skip reason — the list surfaces it rather than hiding it.
        let json = format!(
            r#"[
                {{
                    "pullRequestId": 7,
                    "title": "Both labels",
                    "lastMergeSourceCommit": {{ "commitId": "sha" }},
                    "sourceRefName": "refs/heads/both",
                    "createdBy": {{ "uniqueName": "dev@example.com" }},
                    "isDraft": false,
                    "labels": [{{ "name": "{REVIEW}" }}, {{ "name": "{CHECK}" }}]
                }}
            ]"#
        );
        let r = rows(&json).expect("parses");
        assert_eq!(
            r.len(),
            1,
            "a conflict row is KEPT (not dropped) by parse_rows"
        );
        assert!(r[0].conflict, "both labels → conflict flagged");
        assert_eq!(
            r[0].candidate.kind, "review",
            "conflict keeps review kind (gh parity)"
        );
        // Both labels surface in the row's labels.
        assert_eq!(r[0].labels, vec![REVIEW.to_string(), CHECK.to_string()]);

        // The trait gating view (parse_pr_list) DROPS the conflict → no candidate dispatched.
        let cands = candidates(&json).expect("parses");
        assert!(
            cands.is_empty(),
            "the trait discover drops conflict before dispatch"
        );
    }

    #[test]
    fn parse_rows_drops_unlabelled_pr() {
        // No trigger label → not a monitored PR → dropped entirely (no row).
        let json = r#"[
            {
                "pullRequestId": 9,
                "title": "Chore",
                "lastMergeSourceCommit": { "commitId": "sha" },
                "sourceRefName": "refs/heads/chore",
                "createdBy": { "uniqueName": "dev@example.com" },
                "isDraft": false,
                "labels": [{ "name": "area/docs" }]
            }
        ]"#;
        assert!(
            rows(json).expect("parses").is_empty(),
            "an unlabelled PR is dropped"
        );
    }

    #[test]
    fn parse_rows_marks_draft() {
        let json = format!(
            r#"[
                {{
                    "pullRequestId": 3,
                    "lastMergeSourceCommit": {{ "commitId": "sha" }},
                    "sourceRefName": "refs/heads/wip",
                    "createdBy": {{ "uniqueName": "dev@example.com" }},
                    "isDraft": true,
                    "labels": [{{ "name": "{REVIEW}" }}]
                }}
            ]"#
        );
        let r = rows(&json).expect("parses");
        assert_eq!(r.len(), 1);
        assert!(r[0].candidate.is_draft);
    }

    #[test]
    fn parse_rows_marks_cross_repo_via_fork_source() {
        // A non-null `forkSource` key → the source branch lives in a fork → cross-repo.
        let json = format!(
            r#"[
                {{
                    "pullRequestId": 4,
                    "lastMergeSourceCommit": {{ "commitId": "sha" }},
                    "sourceRefName": "refs/heads/contrib",
                    "createdBy": {{ "uniqueName": "outside@example.com" }},
                    "isDraft": false,
                    "labels": [{{ "name": "{REVIEW}" }}],
                    "forkSource": {{ "repository": {{ "id": "fork-id" }} }}
                }}
            ]"#
        );
        let r = rows(&json).expect("parses");
        assert_eq!(r.len(), 1);
        assert!(
            r[0].candidate.is_cross_repository,
            "a present forkSource marks the PR cross-repo"
        );
    }

    #[test]
    fn parse_rows_empty_array() {
        assert!(rows("[]").expect("parses empty").is_empty());
    }

    #[test]
    fn parse_rows_treats_null_labels_as_no_labels() {
        // REAL Azure JSON: `labels` is JSON `null` (not `[]`) when a PR has no labels — the
        // most common real-world case. The parse must NOT panic / error on it; a null /
        // absent `labels` means "no trigger label" → the PR is dropped (zero rows).
        let null_labels = r#"[
            {
                "pullRequestId": 1,
                "lastMergeSourceCommit": { "commitId": "sha" },
                "sourceRefName": "refs/heads/x",
                "createdBy": { "uniqueName": "dev@example.com" },
                "isDraft": false,
                "labels": null
            }
        ]"#;
        assert!(
            rows(null_labels)
                .expect("null labels must parse")
                .is_empty(),
            "a PR with null labels has no trigger label → dropped"
        );

        // An absent `labels` key behaves the same way.
        let absent_labels = r#"[
            {
                "pullRequestId": 2,
                "lastMergeSourceCommit": { "commitId": "sha" },
                "sourceRefName": "refs/heads/y",
                "createdBy": { "uniqueName": "dev@example.com" },
                "isDraft": false
            }
        ]"#;
        assert!(
            rows(absent_labels)
                .expect("absent labels must parse")
                .is_empty(),
            "a PR with no labels key is dropped"
        );
    }

    #[test]
    fn parse_rows_null_last_merge_commit_defaults_head_sha_empty() {
        // REAL Azure JSON: `lastMergeSourceCommit` can be null (e.g. a brand-new PR) →
        // head_sha falls back to "". The labelled PR is still discovered. Label objects use
        // the real shape `{ "active", "id", "name", "url" }` — we match on `.name`.
        let json = format!(
            r#"[
                {{
                    "pullRequestId": 11,
                    "lastMergeSourceCommit": null,
                    "sourceRefName": "refs/heads/new",
                    "createdBy": {{ "uniqueName": "dev@example.com" }},
                    "isDraft": false,
                    "labels": [{{ "active": true, "id": "x", "name": "{REVIEW}", "url": "u" }}]
                }}
            ]"#
        );
        let r = rows(&json).expect("parses");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].candidate.head_sha, "");
        assert_eq!(r[0].candidate.kind, "review");
    }

    #[test]
    fn parse_rows_label_match_is_case_sensitive_like_gh() {
        // gh filters labels server-side with `gh pr list --label` (exact, case-sensitive),
        // so the Azure client-side classify mirrors that: a case-mismatched label name does
        // NOT match → the PR is dropped (no trigger label).
        let json = r#"[
            {
                "pullRequestId": 13,
                "lastMergeSourceCommit": { "commitId": "sha" },
                "sourceRefName": "refs/heads/z",
                "createdBy": { "uniqueName": "dev@example.com" },
                "isDraft": false,
                "labels": [{ "active": true, "id": "x", "name": "PR-STATUS/NEEDS-REVIEW-AGAIN", "url": "u" }]
            }
        ]"#;
        assert!(
            rows(json).expect("parses").is_empty(),
            "a case-mismatched label name does not match (exact-match parity with gh)"
        );
    }

    #[test]
    fn parse_rows_rejects_non_array() {
        assert!(rows("{\"pullRequestId\": 1}").is_err());
        assert!(rows("not json").is_err());
    }

    #[test]
    fn parse_rows_defaults_missing_optional_fields() {
        // Missing title / lastMergeSourceCommit / createdBy / sourceRefName / forkSource →
        // empty title / head_sha / author / head_ref and not cross-repo (gh null-field parity).
        let json = format!(
            r#"[
                {{
                    "pullRequestId": 8,
                    "isDraft": false,
                    "labels": [{{ "name": "{REVIEW}" }}]
                }}
            ]"#
        );
        let r = rows(&json).expect("parses");
        assert_eq!(r.len(), 1);
        let row = &r[0];
        assert_eq!(row.candidate.number, 8);
        assert_eq!(row.title, "");
        assert_eq!(row.candidate.head_sha, "");
        assert_eq!(row.candidate.head_ref, "");
        assert_eq!(row.candidate.author, "");
        assert!(!row.candidate.is_cross_repository);
        // url is still constructed from the id even when display fields are absent.
        assert_eq!(
            row.url,
            "https://dev.azure.com/myorg/myproject/_git/myrepo/pullrequest/8"
        );
    }

    #[test]
    fn parse_rows_author_falls_back_to_display_name() {
        // Empty uniqueName → fall back to displayName.
        let json = format!(
            r#"[
                {{
                    "pullRequestId": 10,
                    "lastMergeSourceCommit": {{ "commitId": "sha" }},
                    "sourceRefName": "refs/heads/x",
                    "createdBy": {{ "uniqueName": "", "displayName": "Display Only" }},
                    "isDraft": false,
                    "labels": [{{ "name": "{REVIEW}" }}]
                }}
            ]"#
        );
        let r = rows(&json).expect("parses");
        assert_eq!(r[0].candidate.author, "Display Only");
    }

    // F1: a short stderr passes through trimmed; a long one is truncated to the trailing
    // `AZ_STDERR_TAIL_CHARS` chars with a leading `…`, so the EARLIEST stderr (the MSAL/ADAL
    // auth-debug where a token could appear) is dropped and the message length is bounded.
    #[test]
    fn stderr_tail_passes_short_and_truncates_long_to_the_end() {
        // Short → trimmed, no ellipsis.
        assert_eq!(stderr_tail("  ERROR: not found  "), "ERROR: not found");

        // Long: a unique earliest marker, then enough filler to push it WELL past the tail
        // window, then the user-facing error at the very end.
        let earliest = "EARLIEST-SECRET-MARKER";
        let filler = "x".repeat(AZ_STDERR_TAIL_CHARS * 2); // ≫ the tail window
        let tail_msg = "ERROR: az repos pr list failed: repository not found";
        let full = format!("{earliest}{filler}{tail_msg}");
        let out = stderr_tail(&full);
        assert!(
            out.starts_with('…'),
            "truncated output is ellipsis-prefixed: {out}"
        );
        assert!(
            out.ends_with(tail_msg),
            "the user-facing tail is preserved: {out}"
        );
        // The earliest stderr (where a token would be) is beyond the window → dropped.
        assert!(
            !out.contains(earliest),
            "the earliest auth-debug is dropped: {out}"
        );
        // Bounded: ellipsis + exactly AZ_STDERR_TAIL_CHARS tail chars.
        assert_eq!(out.chars().count(), AZ_STDERR_TAIL_CHARS + 1);
    }

    // F1: char-counting (not byte) so a multibyte tail never splits a UTF-8 boundary
    // (the truncation `.chars().skip(..)` would panic on a byte slice mid-codepoint).
    #[test]
    fn stderr_tail_is_utf8_safe_on_multibyte() {
        let long = "中".repeat(AZ_STDERR_TAIL_CHARS + 50); // each char is 3 bytes
        let out = stderr_tail(&long);
        assert!(out.starts_with('…'));
        assert_eq!(out.chars().count(), AZ_STDERR_TAIL_CHARS + 1);
    }

    // #124 F6: `read_bounded` drains a reader but stops once it would exceed `limit`,
    // erroring rather than allocating unboundedly. A `&[u8]` is an `AsyncRead` under tokio,
    // so the bound is unit-tested without a real subprocess. `<= limit` → Ok with the bytes;
    // `limit + 1` → Err. The limit is applied at READ time (memory never exceeds ~limit).
    #[tokio::test]
    async fn read_bounded_accepts_up_to_limit_and_rejects_over() {
        // Exactly at the limit → Ok with all bytes.
        let exact = [b'a'; 100];
        let got = read_bounded(&exact[..], 100, "测试")
            .await
            .expect("at-limit input is accepted");
        assert_eq!(got, exact);

        // Under the limit → Ok.
        let under = [b'b'; 50];
        assert_eq!(
            read_bounded(&under[..], 100, "测试").await.expect("under"),
            under
        );

        // One byte over the limit → Err carrying the supplied label.
        let over = [b'c'; 101];
        let err = read_bounded(&over[..], 100, "az repos pr list 输出过大")
            .await
            .expect_err("over-limit input is rejected");
        assert!(
            err.message.contains("输出过大"),
            "error carries the over-size label: {}",
            err.message
        );

        // Empty input → Ok empty.
        let empty: [u8; 0] = [];
        assert!(read_bounded(&empty[..], 100, "测试")
            .await
            .expect("empty")
            .is_empty());
    }
}
