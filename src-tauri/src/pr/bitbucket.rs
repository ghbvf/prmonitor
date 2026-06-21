//! Bitbucket Server / Data Center [`super::source::PrSource`] implementation via the
//! REST API v1.0 over HTTP (`reqwest`) (AB#717).
//!
//! `discover_rows` GETs `{host}/rest/api/1.0/projects/{project}/repos/{repo}/pull-requests
//! ?state=OPEN` (paginated `start`/`limit` until `isLastPage`), authenticating with
//! `Authorization: Bearer <token>` (a Bitbucket HTTP access token / PAT — there is no
//! universal Bitbucket CLI, unlike gh/az). It maps each PR into display-ready
//! [`BbRow`]s (the Bitbucket analogue of `gh.rs`'s `GhRow` / `azure.rs`'s `AzRow`).
//!
//! **No native labels.** Bitbucket Server PRs carry NO label concept, so labels are
//! ALWAYS resolved from the PR title via [`labels::effective_labels`] (the project's
//! [`LabelSource`] is `Title`, enforced by `config::model::validate_project`). The pure
//! [`parse_rows`] is unit-tested with golden Server JSON without a live connection; the
//! `reqwest` calls (`fetch_page` / `discover_rows`) are the thin IO shell around it.

use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::model::{Candidate, LabelSource};

use super::labels;
use super::source::PrSource;

/// Wall-clock budget for any single Bitbucket REST request (parity with gh/az's 30s).
/// Applied by the `reqwest` client builder; a hung request is bounded here.
const BB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Page size for the paginated PR list request.
const BB_PAGE_LIMIT: u32 = 100;

/// Hard caps on pagination so a misconfigured / hostile server can't drive an unbounded
/// loop or unbounded accumulation (the HTTP analogue of azure.rs's bounded stdout read).
const BB_MAX_PAGES: u32 = 50;
const BB_MAX_PRS: usize = 5000;

/// Max chars of an error-response body surfaced in a non-2xx error. The token lives in
/// the request header (never echoed in the body), so this can't leak it; the cap just
/// bounds the message. Counts CHARS so the slice never splits a UTF-8 boundary.
const BB_ERR_BODY_TAIL_CHARS: usize = 512;

/// `{ "user": { "name": "...", "displayName": "..." } }` — Bitbucket Server author.
#[derive(Debug, Deserialize)]
struct BbAuthor {
    #[serde(default)]
    user: Option<BbUser>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BbUser {
    /// The account username (slug). The author allowlist matches against this.
    #[serde(default)]
    name: String,
    /// Human display name — fallback when `name` is empty (parity with azure's
    /// uniqueName→displayName fallback).
    #[serde(default)]
    display_name: String,
}

/// `{ "id": 1, "slug": "...", ... }` — a repository reference on a PR's from/to ref.
#[derive(Debug, Deserialize)]
struct BbRepo {
    /// Numeric repository id; differs between fromRef and toRef for a cross-repo (fork) PR.
    #[serde(default)]
    id: Option<i64>,
}

/// `{ "displayId": "branch", "latestCommit": "sha", "repository": {..} }` — a PR ref.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BbRef {
    /// Branch name (no `refs/heads/` prefix, unlike Azure's `sourceRefName`).
    #[serde(default)]
    display_id: String,
    /// Tip commit sha of the source branch (→ `head_sha`).
    #[serde(default)]
    latest_commit: String,
    #[serde(default)]
    repository: Option<BbRepo>,
}

/// `{ "self": [ { "href": "<web url>" } ] }` — Bitbucket Server's PR web URL.
#[derive(Debug, Deserialize)]
struct BbLinks {
    /// `self` is a Rust keyword → renamed. The first href is the PR's web overview URL.
    #[serde(rename = "self", default)]
    self_links: Vec<BbLink>,
}

#[derive(Debug, Deserialize)]
struct BbLink {
    #[serde(default)]
    href: String,
}

/// One PR object from the `pull-requests` endpoint's `values` array.
#[derive(Debug, Deserialize)]
struct BbPr {
    id: u64,
    #[serde(default)]
    title: String,
    /// Draft flag (Bitbucket DC ≥ 8.x). Absent / older versions → false.
    #[serde(default)]
    draft: bool,
    #[serde(default, rename = "fromRef")]
    from_ref: Option<BbRef>,
    #[serde(default, rename = "toRef")]
    to_ref: Option<BbRef>,
    #[serde(default)]
    author: Option<BbAuthor>,
    #[serde(default)]
    links: Option<BbLinks>,
}

/// One page of the paginated `pull-requests` response.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BbPage {
    #[serde(default)]
    values: Vec<BbPr>,
    #[serde(default)]
    is_last_page: bool,
    /// Start offset of the next page (present when `isLastPage` is false).
    #[serde(default)]
    next_page_start: Option<u32>,
}

/// A discovered Bitbucket PR: its gating [`Candidate`] plus the display fields the PR
/// list shows. The Bitbucket analogue of `gh.rs`'s `GhRow` / `azure.rs`'s `AzRow` (same
/// fields, same `conflict` semantics) so the `pr` slice's view path treats all sources
/// identically.
#[derive(Debug, Clone)]
pub struct BbRow {
    pub candidate: Candidate,
    pub title: String,
    pub labels: Vec<String>,
    pub url: String,
    pub conflict: bool,
}

/// The trailing `BB_ERR_BODY_TAIL_CHARS` chars of an error body (char-counted so it never
/// splits a UTF-8 boundary), prefixed with `…` when truncated. Pure → unit-tested.
fn err_body_tail(body: &str) -> String {
    let trimmed = body.trim();
    let char_count = trimmed.chars().count();
    if char_count <= BB_ERR_BODY_TAIL_CHARS {
        return trimmed.to_string();
    }
    let tail: String = trimmed
        .chars()
        .skip(char_count - BB_ERR_BODY_TAIL_CHARS)
        .collect();
    format!("…{tail}")
}

/// Maps deserialized Bitbucket PRs into display-ready [`BbRow`]s — the pure core shared
/// by [`parse_rows`] (single-page, for tests) and [`BitbucketServer::discover_rows`]
/// (all paginated PRs).
///
/// - Effective labels come from [`labels::effective_labels`] over the PR TITLE (Bitbucket
///   has no native labels), then [`labels::classify`] gives `(kind, conflict)` — both
///   trigger tags → conflict (kept, kind "review", gh parity); neither → dropped.
/// - `id` → number; `fromRef.latestCommit` → head_sha; `fromRef.displayId` → head_ref;
///   `fromRef.repository.id` ≠ `toRef.repository.id` → is_cross_repository; `draft` →
///   is_draft; `author.user.name` (fallback `displayName`) → author.
/// - Web URL: `links.self[0].href` (server-provided), else constructed
///   `{host}/projects/{project}/repos/{repo}/pull-requests/{id}` (Bitbucket project keys
///   / repo slugs are URL-safe, so no percent-encoding is needed, unlike Azure).
fn map_rows(
    prs: Vec<BbPr>,
    host: &str,
    project: &str,
    repo: &str,
    review_label: &str,
    check_label: &str,
    label_source: LabelSource,
) -> Vec<BbRow> {
    let base = host.trim_end_matches('/');
    let mut rows = Vec::new();
    for pr in prs {
        // Bitbucket has no native labels → resolve from the title (the only viable source).
        let labels = labels::effective_labels(Vec::new(), &pr.title, label_source);
        let Some((kind, conflict)) = labels::classify(&labels, review_label, check_label) else {
            continue; // no trigger label: not a monitored PR
        };

        let from_ref = pr.from_ref;
        let to_ref = pr.to_ref;
        let head_sha = from_ref
            .as_ref()
            .map(|r| r.latest_commit.clone())
            .unwrap_or_default();
        let head_ref = from_ref
            .as_ref()
            .map(|r| r.display_id.clone())
            .unwrap_or_default();
        // Cross-repo (fork) PR ⇒ the source branch's repo differs from the target's.
        // Poll data is authoritative, so a missing repo id defaults to same-repo (false).
        let from_id = from_ref.and_then(|r| r.repository).and_then(|r| r.id);
        let to_id = to_ref.and_then(|r| r.repository).and_then(|r| r.id);
        let is_cross_repository = match (from_id, to_id) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        };
        // Author: prefer the username (`name`), fall back to `displayName`, then "".
        let author = pr
            .author
            .and_then(|a| a.user)
            .map(|u| {
                if u.name.is_empty() {
                    u.display_name
                } else {
                    u.name
                }
            })
            .unwrap_or_default();
        let url = pr
            .links
            .and_then(|l| l.self_links.into_iter().next())
            .map(|l| l.href)
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "{base}/projects/{project}/repos/{repo}/pull-requests/{}",
                    pr.id
                )
            });

        rows.push(BbRow {
            candidate: Candidate {
                number: pr.id,
                head_sha,
                head_ref,
                author,
                is_cross_repository,
                is_draft: pr.draft,
                kind: kind.to_string(),
            },
            title: pr.title,
            labels,
            url,
            conflict,
        });
    }
    rows
}

/// Parses ONE page of `pull-requests` JSON (a `{ "values": [...] }` object) into
/// display-ready [`BbRow`]s. Pure (no HTTP) so the field-mapping / classify / url-build
/// semantics are unit-tested with golden Server JSON.
pub fn parse_rows(
    page_json: &str,
    host: &str,
    project: &str,
    repo: &str,
    review_label: &str,
    check_label: &str,
    label_source: LabelSource,
) -> AppResult<Vec<BbRow>> {
    let page: BbPage = serde_json::from_str(page_json)
        .map_err(|e| AppError::new(format!("解析 Bitbucket pull-requests JSON 失败: {e}")))?;
    Ok(map_rows(
        page.values,
        host,
        project,
        repo,
        review_label,
        check_label,
        label_source,
    ))
}

/// The gating projection of discovered rows: the [`Candidate`]s, EXCLUDING conflict rows
/// (mirrors gh.rs / azure.rs, which drop both-label PRs before dispatch). The single
/// "rows → dispatch candidates" step.
fn rows_into_candidates(rows: Vec<BbRow>) -> Vec<Candidate> {
    rows.into_iter()
        .filter(|row| !row.conflict)
        .map(|row| row.candidate)
        .collect()
}

/// The Bitbucket Server / Data Center PR source backed by the REST API (AB#717).
pub struct BitbucketServer {
    /// Base host URL, e.g. `https://bitbucket.example.com` (validated URL-safe by config).
    host: String,
    /// Project key (e.g. `GOCELL`, or `~username` for a personal repo).
    project: String,
    /// Repository slug.
    repo: String,
    /// HTTP access token (PAT) → `Authorization: Bearer <token>`.
    token: String,
    review_label: String,
    check_label: String,
    label_source: LabelSource,
}

impl BitbucketServer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: String,
        project: String,
        repo: String,
        token: String,
        review_label: String,
        check_label: String,
        label_source: LabelSource,
    ) -> Self {
        Self {
            host,
            project,
            repo,
            token,
            review_label,
            check_label,
            label_source,
        }
    }

    /// GETs one page of open PRs from the REST API. Bearer-authenticated, bounded by
    /// [`BB_TIMEOUT`]; a non-2xx surfaces the status + a bounded body tail (never the
    /// token, which lives only in the request header).
    async fn fetch_page(&self, client: &reqwest::Client, start: u32) -> AppResult<BbPage> {
        let base = self.host.trim_end_matches('/');
        let url = format!(
            "{base}/rest/api/1.0/projects/{}/repos/{}/pull-requests",
            self.project, self.repo
        );
        let start_s = start.to_string();
        let limit_s = BB_PAGE_LIMIT.to_string();
        let resp = client
            .get(&url)
            .query(&[
                ("state", "OPEN"),
                ("start", start_s.as_str()),
                ("limit", limit_s.as_str()),
            ])
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| AppError::new(format!("Bitbucket pull-requests 请求失败: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            // The body may carry an error description; the token is NOT in the body.
            let body = resp.text().await.unwrap_or_default();
            return Err(AppError::new(format!(
                "Bitbucket pull-requests 失败 (HTTP {}): {}",
                status.as_u16(),
                err_body_tail(&body)
            )));
        }
        resp.json::<BbPage>()
            .await
            .map_err(|e| AppError::new(format!("解析 Bitbucket pull-requests 响应失败: {e}")))
    }

    /// Discovers all open PRs as display-ready [`BbRow`]s (conflict rows included, marked).
    /// Follows pagination (`start`/`nextPageStart` until `isLastPage`), bounded by
    /// [`BB_MAX_PAGES`] / [`BB_MAX_PRS`], then maps via [`map_rows`].
    pub async fn discover_rows(&self) -> AppResult<Vec<BbRow>> {
        let client = reqwest::Client::builder()
            .timeout(BB_TIMEOUT)
            .build()
            .map_err(|e| AppError::new(format!("无法构造 Bitbucket HTTP 客户端: {e}")))?;

        let mut all: Vec<BbPr> = Vec::new();
        let mut start = 0u32;
        for _ in 0..BB_MAX_PAGES {
            let page = self.fetch_page(&client, start).await?;
            all.extend(page.values);
            if page.is_last_page || all.len() >= BB_MAX_PRS {
                break;
            }
            match page.next_page_start {
                Some(next) => start = next,
                None => break, // not last page but no cursor → stop rather than loop
            }
        }
        Ok(map_rows(
            all,
            &self.host,
            &self.project,
            &self.repo,
            &self.review_label,
            &self.check_label,
            self.label_source,
        ))
    }
}

impl PrSource for BitbucketServer {
    /// Trait view: gating [`Candidate`]s only, excluding conflict PRs (mirrors gh.rs /
    /// azure.rs). The list UI uses [`Self::discover_rows`] for the full display fields.
    async fn discover(&self) -> AppResult<Vec<Candidate>> {
        Ok(rows_into_candidates(self.discover_rows().await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVIEW: &str = "pr-status/needs-review-again";
    const CHECK: &str = "pr-status/needs-check-fix";
    const HOST: &str = "https://bitbucket.example.com";
    const PROJECT: &str = "GOCELL";
    const REPO: &str = "myrepo";

    fn rows(json: &str) -> AppResult<Vec<BbRow>> {
        parse_rows(json, HOST, PROJECT, REPO, REVIEW, CHECK, LabelSource::Title)
    }

    fn candidates(json: &str) -> AppResult<Vec<Candidate>> {
        Ok(rows_into_candidates(rows(json)?))
    }

    #[test]
    fn parse_rows_maps_review_pr_from_title_tag_with_self_link_url() {
        // Title carries the review trigger tag; native labels do not exist on Bitbucket.
        let json = format!(
            r#"{{
                "size": 1, "isLastPage": true, "values": [
                    {{
                        "id": 12,
                        "title": "Add widget [{REVIEW}]",
                        "draft": false,
                        "fromRef": {{
                            "displayId": "feature/widget",
                            "latestCommit": "abc123",
                            "repository": {{ "id": 1, "slug": "myrepo" }}
                        }},
                        "toRef": {{ "displayId": "main", "repository": {{ "id": 1 }} }},
                        "author": {{ "user": {{ "name": "tom", "displayName": "Tom T" }} }},
                        "links": {{ "self": [
                            {{ "href": "https://bitbucket.example.com/projects/GOCELL/repos/myrepo/pull-requests/12/overview" }}
                        ] }}
                    }}
                ]
            }}"#
        );
        let r = rows(&json).expect("parses");
        assert_eq!(r.len(), 1);
        let row = &r[0];
        assert_eq!(row.candidate.number, 12);
        assert_eq!(row.candidate.head_sha, "abc123");
        assert_eq!(row.candidate.head_ref, "feature/widget"); // no refs/heads/ on Bitbucket
        assert_eq!(row.candidate.author, "tom"); // name preferred over displayName
        assert_eq!(row.candidate.kind, "review");
        assert!(!row.candidate.is_cross_repository);
        assert!(!row.candidate.is_draft);
        // Effective labels come from the title.
        assert_eq!(row.labels, vec![REVIEW.to_string()]);
        // Web URL is the server-provided self link.
        assert_eq!(
            row.url,
            "https://bitbucket.example.com/projects/GOCELL/repos/myrepo/pull-requests/12/overview"
        );
        assert!(!row.conflict);
    }

    #[test]
    fn parse_rows_constructs_url_when_self_link_absent() {
        let json = format!(
            r#"{{ "isLastPage": true, "values": [
                {{ "id": 7, "title": "Fix [{CHECK}]", "fromRef": {{ "displayId": "fix", "latestCommit": "sha" }} }}
            ] }}"#
        );
        let r = rows(&json).expect("parses");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].candidate.kind, "check");
        assert_eq!(
            r[0].url,
            "https://bitbucket.example.com/projects/GOCELL/repos/myrepo/pull-requests/7"
        );
    }

    #[test]
    fn parse_rows_trims_trailing_slash_in_constructed_url() {
        let json = r#"{ "isLastPage": true, "values": [
            { "id": 7, "title": "Fix [pr-status/needs-check-fix]", "fromRef": { "displayId": "fix" } }
        ] }"#;
        let r = parse_rows(
            json,
            "https://bitbucket.example.com/",
            PROJECT,
            REPO,
            REVIEW,
            CHECK,
            LabelSource::Title,
        )
        .expect("parses");
        assert_eq!(
            r[0].url,
            "https://bitbucket.example.com/projects/GOCELL/repos/myrepo/pull-requests/7"
        );
    }

    #[test]
    fn parse_rows_keeps_both_tag_conflict_with_review_kind() {
        // Both trigger tags in the title → conflict, kept (kind review), dropped from dispatch.
        let json = format!(
            r#"{{ "isLastPage": true, "values": [
                {{ "id": 9, "title": "[{REVIEW}][{CHECK}] both", "fromRef": {{ "displayId": "b", "latestCommit": "s" }} }}
            ] }}"#
        );
        let r = rows(&json).expect("parses");
        assert_eq!(r.len(), 1);
        assert!(r[0].conflict);
        assert_eq!(r[0].candidate.kind, "review");
        assert_eq!(r[0].labels, vec![REVIEW.to_string(), CHECK.to_string()]);
        // The gating view drops the conflict.
        assert!(candidates(&json).expect("parses").is_empty());
    }

    #[test]
    fn parse_rows_drops_pr_without_trigger_tag() {
        let json = r#"{ "isLastPage": true, "values": [
            { "id": 5, "title": "Chore: bump deps", "fromRef": { "displayId": "chore" } }
        ] }"#;
        assert!(rows(json).expect("parses").is_empty());
    }

    #[test]
    fn parse_rows_marks_cross_repo_when_repo_ids_differ() {
        let json = format!(
            r#"{{ "isLastPage": true, "values": [
                {{
                    "id": 4, "title": "Contrib [{REVIEW}]",
                    "fromRef": {{ "displayId": "c", "latestCommit": "s", "repository": {{ "id": 2 }} }},
                    "toRef": {{ "displayId": "main", "repository": {{ "id": 1 }} }}
                }}
            ] }}"#
        );
        let r = rows(&json).expect("parses");
        assert_eq!(r.len(), 1);
        assert!(r[0].candidate.is_cross_repository);
    }

    #[test]
    fn parse_rows_marks_draft_and_falls_back_to_display_name() {
        let json = format!(
            r#"{{ "isLastPage": true, "values": [
                {{
                    "id": 3, "title": "WIP [{REVIEW}]", "draft": true,
                    "fromRef": {{ "displayId": "wip", "latestCommit": "s" }},
                    "author": {{ "user": {{ "name": "", "displayName": "Only Display" }} }}
                }}
            ] }}"#
        );
        let r = rows(&json).expect("parses");
        assert_eq!(r.len(), 1);
        assert!(r[0].candidate.is_draft);
        assert_eq!(r[0].candidate.author, "Only Display");
    }

    #[test]
    fn parse_rows_empty_values_and_malformed() {
        // An explicit empty page → no rows.
        assert!(rows(r#"{ "isLastPage": true, "values": [] }"#)
            .expect("parses empty")
            .is_empty());
        // A page object with no values key → no rows (serde default).
        assert!(rows(r#"{ "isLastPage": true }"#)
            .expect("parses")
            .is_empty());
        // Genuinely malformed input (not a JSON object) is rejected.
        assert!(rows("not json").is_err());
        assert!(rows("123").is_err());
    }

    #[test]
    fn parse_rows_native_label_source_yields_nothing() {
        // Defensive: Bitbucket has no native labels, so Native mode (which config forbids
        // for Bitbucket) resolves to empty labels → never matches a trigger → no rows.
        let json = format!(
            r#"{{ "isLastPage": true, "values": [
                {{ "id": 1, "title": "Add [{REVIEW}]", "fromRef": {{ "displayId": "x" }} }}
            ] }}"#
        );
        assert!(parse_rows(
            &json,
            HOST,
            PROJECT,
            REPO,
            REVIEW,
            CHECK,
            LabelSource::Native
        )
        .expect("parses")
        .is_empty());
    }

    #[test]
    fn err_body_tail_truncates_long_body_to_end() {
        assert_eq!(err_body_tail("  not found  "), "not found");
        let long = "x".repeat(BB_ERR_BODY_TAIL_CHARS * 2);
        let out = err_body_tail(&long);
        assert!(out.starts_with('…'));
        assert_eq!(out.chars().count(), BB_ERR_BODY_TAIL_CHARS + 1);
    }
}
