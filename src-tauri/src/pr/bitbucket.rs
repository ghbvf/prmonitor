//! Bitbucket Server / Data Center [`super::source::EventSourceProvider`] implementation via
//! the REST API v1.0 over HTTP (`reqwest`) (AB#717).
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
use crate::model::{
    Candidate, EventEnvelope, EventSubject, EventType, InboxDedupeKey, LabelSource, ReviewKind,
    SourceKind,
};

use super::labels;
use super::source::{pr_dedupe_key, DiscoveredEvent, EventSourceProvider};

/// Wall-clock budget for any single Bitbucket REST request (parity with gh/az's 30s).
/// Applied by the `reqwest` client builder; a hung request is bounded here.
const BB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Page size for the paginated PR list request.
const BB_PAGE_LIMIT: u32 = 100;

/// Hard caps on pagination so a misconfigured / hostile server can't drive an unbounded
/// loop or unbounded accumulation (the HTTP analogue of azure.rs's bounded stdout read).
const BB_MAX_PAGES: u32 = 50;
const BB_MAX_PRS: usize = 5000;

/// Max response body we will buffer + parse (the HTTP analogue of azure.rs's bounded
/// stdout read). A hostile / misconfigured server could stream an enormous body; the
/// bounded reader stops here (erroring) rather than allocating unboundedly. 10 MiB is far
/// above any realistic page of PR JSON.
const BB_MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

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
    #[serde(default)]
    description: String,
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
struct BbRow {
    candidate: Candidate,
    title: String,
    body: String,
    labels: Vec<String>,
    url: String,
    conflict: bool,
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

/// Reads a response body to EOF but stops once more than `limit` bytes arrive, returning
/// `Err` instead of buffering an unbounded body (F4). `content_length` is a fast pre-reject;
/// the streamed chunk loop bounds chunked / lying-length responses too. The token lives in
/// the request header, never the body, so this never risks leaking it.
async fn read_body_capped(mut resp: reqwest::Response, limit: usize) -> AppResult<Vec<u8>> {
    if resp.content_length().is_some_and(|n| n as usize > limit) {
        return Err(AppError::new("Bitbucket 响应体超过大小上限"));
    }
    let mut buf = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| AppError::new(format!("读取 Bitbucket 响应失败: {}", e.without_url())))?
    {
        buf.extend_from_slice(&chunk);
        if buf.len() > limit {
            return Err(AppError::new("Bitbucket 响应体超过大小上限"));
        }
    }
    Ok(buf)
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
    trigger_labels: &[String],
    label_source: LabelSource,
) -> Vec<BbRow> {
    let base = host.trim_end_matches('/');
    let mut rows = Vec::new();
    for pr in prs {
        // Bitbucket has no native labels → resolve from the title (the only viable source).
        let labels = labels::effective_labels(Vec::new(), &pr.title, label_source);
        if !trigger_labels.is_empty()
            && !trigger_labels
                .iter()
                .any(|wanted| labels.iter().any(|label| label == wanted))
        {
            continue;
        }

        // F2 (fail-closed): a real open PR always carries `fromRef` with a `latestCommit`
        // (→ head_sha, the dispatch key) and `displayId` (→ head_ref). A missing/empty one
        // is a malformed response — skip the PR rather than emitting an empty-head candidate
        // that would corrupt the dispatch ledger key (`{number}@{headSha}:{kind}`).
        let Some(from_ref) = pr.from_ref else {
            continue;
        };
        let head_sha = from_ref.latest_commit;
        let head_ref = from_ref.display_id;
        if head_sha.is_empty() || head_ref.is_empty() {
            continue;
        }
        // Cross-repo (fork) PR ⇒ the source branch's repo differs from the target's.
        // F3 (fail-closed): an UNKNOWN repo identity (either id absent) is treated as
        // cross-repo, so the shared `should_skip` gate drops it — we never auto-dispatch a
        // PR whose origin we can't attribute (mirrors the GitHub webhook fork fail-safe).
        let from_id = from_ref.repository.and_then(|r| r.id);
        let to_id = pr.to_ref.and_then(|r| r.repository).and_then(|r| r.id);
        let is_cross_repository = match (from_id, to_id) {
            (Some(a), Some(b)) => a != b,
            _ => true,
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
                kind: ReviewKind::Review,
            },
            title: pr.title,
            body: pr.description,
            labels,
            url,
            conflict: false,
        });
    }
    rows
}

/// Parses ONE page of `pull-requests` JSON (a `{ "values": [...] }` object) into
/// display-ready [`BbRow`]s. Pure (no HTTP) so the field-mapping / classify / url-build
/// semantics are unit-tested with golden Server JSON. `#[cfg(test)]`: the live path
/// (`discover_rows`) calls [`map_rows`] directly; this string-input wrapper is test-only.
#[cfg(test)]
fn parse_rows(
    page_json: &str,
    host: &str,
    project: &str,
    repo: &str,
    trigger_labels: &[String],
    label_source: LabelSource,
) -> AppResult<Vec<BbRow>> {
    let page: BbPage = serde_json::from_str(page_json)
        .map_err(|e| AppError::new(format!("解析 Bitbucket pull-requests JSON 失败: {e}")))?;
    Ok(map_rows(
        page.values,
        host,
        project,
        repo,
        trigger_labels,
        label_source,
    ))
}

/// The gating projection of discovered rows: the [`Candidate`]s, EXCLUDING conflict rows
/// (mirrors gh.rs / azure.rs). `#[cfg(test)]`: the live path now produces normalized
/// [`DiscoveredEvent`]s (AB#1070) and applies the conflict gate downstream in
/// `commands::build_view_parts`, so this gating projection survives ONLY for the unit-test
/// `candidates` helper.
#[cfg(test)]
fn rows_into_candidates(rows: Vec<BbRow>) -> Vec<Candidate> {
    rows.into_iter()
        .filter(|row| !row.conflict)
        .map(|row| row.candidate)
        .collect()
}

/// Construction inputs for [`BitbucketServer`] (AB#717 F11) — a named struct so the call
/// site reads field-by-field instead of a 7-positional-arg constructor where the same-typed
/// `String`s could silently transpose.
pub struct BitbucketSourceConfig {
    /// Base host URL, e.g. `https://bitbucket.example.com` (validated `https://` by config).
    pub host: String,
    /// Project key (e.g. `GOCELL`, or `~username` for a personal repo).
    pub project: String,
    /// Repository slug.
    pub repo: String,
    /// HTTP access token (PAT) → `Authorization: Bearer <token>`.
    pub token: String,
    pub trigger_labels: Vec<String>,
    pub label_source: LabelSource,
}

/// The Bitbucket Server / Data Center PR source backed by the REST API (AB#717).
pub struct BitbucketServer {
    host: String,
    project: String,
    repo: String,
    token: String,
    trigger_labels: Vec<String>,
    label_source: LabelSource,
}

impl BitbucketServer {
    pub fn new(cfg: BitbucketSourceConfig) -> Self {
        Self {
            host: cfg.host,
            project: cfg.project,
            repo: cfg.repo,
            token: cfg.token,
            trigger_labels: cfg.trigger_labels,
            label_source: cfg.label_source,
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
            // `without_url()` strips the request URL from the error Display: a transport
            // error otherwise embeds the full REST URL (host/project/repo) into a message
            // that surfaces to the frontend. The token lives in the header, never the URL,
            // so it is not at risk here — this just avoids leaking the endpoint shape.
            .map_err(|e| {
                AppError::new(format!(
                    "Bitbucket pull-requests 请求失败: {}",
                    e.without_url()
                ))
            })?;

        let status = resp.status();
        if !status.is_success() {
            // The body may carry an error description; the token is NOT in the body. Read it
            // bounded (best-effort — an empty body just yields a status-only message).
            let code = status.as_u16();
            let body = read_body_capped(resp, BB_MAX_BODY_BYTES)
                .await
                .unwrap_or_default();
            let text = String::from_utf8_lossy(&body);
            return Err(AppError::new(format!(
                "Bitbucket pull-requests 失败 (HTTP {code}): {}",
                err_body_tail(&text)
            )));
        }
        let body = read_body_capped(resp, BB_MAX_BODY_BYTES).await?;
        serde_json::from_slice::<BbPage>(&body)
            .map_err(|e| AppError::new(format!("解析 Bitbucket pull-requests 响应失败: {e}")))
    }

    /// Discovers all open PRs as display-ready [`BbRow`]s (conflict rows included, marked).
    /// Follows pagination (`start`/`nextPageStart` until `isLastPage`), bounded by
    /// [`BB_MAX_PAGES`] / [`BB_MAX_PRS`], then maps via [`map_rows`].
    async fn discover_rows(&self) -> AppResult<Vec<BbRow>> {
        let client = reqwest::Client::builder()
            .timeout(BB_TIMEOUT)
            // Defense-in-depth alongside the config-time `https://` check: never send the
            // Bearer PAT over a plaintext `http://` connection even if a bad host slips through.
            .https_only(true)
            .build()
            .map_err(|e| AppError::new(format!("无法构造 Bitbucket HTTP 客户端: {e}")))?;

        // F5 (fail-closed): the discovery list must be COMPLETE or an error — a partial list
        // treated as authoritative silently drops PRs (missed dispatches / a PR that looks
        // "gone"). So hitting either cap WITHOUT reaching `isLastPage`, or a missing pagination
        // cursor, is an error — never a silent truncated `Ok`.
        let mut all: Vec<BbPr> = Vec::new();
        let mut start = 0u32;
        let mut completed = false;
        for _ in 0..BB_MAX_PAGES {
            let page = self.fetch_page(&client, start).await?;
            let is_last = page.is_last_page;
            all.extend(page.values);
            if is_last {
                completed = true;
                break;
            }
            if all.len() >= BB_MAX_PRS {
                return Err(AppError::new(format!(
                    "Bitbucket 开放 PR 数超过上限 {BB_MAX_PRS}（无法完整发现，请收窄监控仓库）"
                )));
            }
            // Bitbucket guarantees `nextPageStart` whenever `isLastPage` is false; its absence
            // means a truncated / malformed response.
            match page.next_page_start {
                Some(next) => start = next,
                None => {
                    return Err(AppError::new(
                        "Bitbucket 分页响应异常：isLastPage=false 但缺少 nextPageStart",
                    ))
                }
            }
        }
        if !completed {
            return Err(AppError::new(format!(
                "Bitbucket 分页页数超过上限 {BB_MAX_PAGES}（未达 isLastPage，无法完整发现）"
            )));
        }
        Ok(map_rows(
            all,
            &self.host,
            &self.project,
            &self.repo,
            &self.trigger_labels,
            self.label_source,
        ))
    }
}

/// Builds a [`DiscoveredEvent`] from one display-ready [`BbRow`] (AB#1070): the normalized
/// AB#1079 [`Event`] is constructed from the SAME already-parsed locals the row holds,
/// alongside the row's gating [`Candidate`] and `conflict` flag. `repo` is the impl's bare
/// monitored repo slug. `project_id` / `received_at_epoch` are left at zero/empty values —
/// the source produces the content envelope; the future inbox (AB#1065) stamps the ingress
/// context (it can't be known here). Always a `PullRequest` event (the only class a PR source
/// emits).
fn row_into_event(row: BbRow, repo: &str) -> DiscoveredEvent {
    let event = EventEnvelope::observation(
        // Wire literal "bitbucket" matches `SourceKind::Bitbucket`'s serde string (format single-sourced).
        InboxDedupeKey::new(pr_dedupe_key(
            "bitbucket",
            repo,
            row.candidate.number,
            &row.candidate.head_sha,
        ))
        .expect("discovery dedupe key is non-empty"),
        SourceKind::Bitbucket,
        "discovery",
        repo,
        EventType::PullRequest,
        EventSubject {
            number: Some(row.candidate.number),
            title: row.title.clone(),
            body: row.body.clone(),
            labels: row.labels.clone(),
            url: row.url.clone(),
        },
        0,
    )
    .expect("discovery event is valid");
    DiscoveredEvent {
        event,
        candidate: row.candidate,
        conflict: row.conflict,
    }
}

impl EventSourceProvider for BitbucketServer {
    /// Discovers open PRs (conflict rows included, marked) as normalized AB#1079
    /// [`DiscoveredEvent`]s (AB#1070): the live producer. The display-ready rows from
    /// [`Self::discover_rows`] are each mapped to a `PullRequest` [`Event`] + gating
    /// [`Candidate`] via [`row_into_event`].
    async fn discover_events(&self) -> AppResult<Vec<DiscoveredEvent>> {
        Ok(self
            .discover_rows()
            .await?
            .into_iter()
            .map(|row| row_into_event(row, &self.repo))
            .collect())
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

    fn trigger_labels() -> Vec<String> {
        vec![REVIEW.to_string(), CHECK.to_string()]
    }

    fn rows(json: &str) -> AppResult<Vec<BbRow>> {
        parse_rows(
            json,
            HOST,
            PROJECT,
            REPO,
            &trigger_labels(),
            LabelSource::Title,
        )
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
                        "description": "Widget details",
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
        assert_eq!(row.candidate.kind, crate::model::ReviewKind::Review);
        assert_eq!(row.body, "Widget details");
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

        // AB#1070: the same row maps to a normalized `Event` (PullRequest) on a
        // `DiscoveredEvent`, built from the SAME parsed locals, while the gating
        // `Candidate` rides along unchanged.
        let de = row_into_event(r[0].clone(), REPO);
        let observation = de.event.as_observation().unwrap();
        assert_eq!(de.event.source(), SourceKind::Bitbucket);
        assert_eq!(observation.event_type, EventType::PullRequest);
        assert_eq!(observation.subject.number, Some(de.candidate.number));
        assert_eq!(observation.subject.title, row.title);
        assert_eq!(observation.subject.url, row.url);
        assert_eq!(observation.subject.labels, row.labels);
        assert_eq!(observation.subject.body, "Widget details");
        // dedupe_key is the exact inbox idempotency-key seed (format single-sourced).
        assert_eq!(
            de.event.dedupe_key().as_str(),
            "bitbucket:pullRequest:myrepo#12@abc123"
        );
        assert!(!de.conflict);
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
        assert_eq!(r[0].candidate.kind, crate::model::ReviewKind::Review);
        assert_eq!(
            r[0].url,
            "https://bitbucket.example.com/projects/GOCELL/repos/myrepo/pull-requests/7"
        );
    }

    #[test]
    fn parse_rows_trims_trailing_slash_in_constructed_url() {
        let json = r#"{ "isLastPage": true, "values": [
            { "id": 7, "title": "Fix [pr-status/needs-check-fix]", "fromRef": { "displayId": "fix", "latestCommit": "sha" } }
        ] }"#;
        let r = parse_rows(
            json,
            "https://bitbucket.example.com/",
            PROJECT,
            REPO,
            &trigger_labels(),
            LabelSource::Title,
        )
        .expect("parses");
        assert_eq!(
            r[0].url,
            "https://bitbucket.example.com/projects/GOCELL/repos/myrepo/pull-requests/7"
        );
    }

    #[test]
    fn parse_rows_keeps_pr_with_multiple_trigger_tags() {
        // Trigger tags are rule-interest filters only; action kind is decided by rules.
        let json = format!(
            r#"{{ "isLastPage": true, "values": [
                {{ "id": 9, "title": "[{REVIEW}][{CHECK}] both", "fromRef": {{ "displayId": "b", "latestCommit": "s" }} }}
            ] }}"#
        );
        let r = rows(&json).expect("parses");
        assert_eq!(r.len(), 1);
        assert!(!r[0].conflict);
        assert_eq!(r[0].candidate.kind, crate::model::ReviewKind::Review);
        assert_eq!(r[0].labels, vec![REVIEW.to_string(), CHECK.to_string()]);
        assert_eq!(candidates(&json).expect("parses").len(), 1);
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
    fn parse_rows_skips_pr_with_missing_or_empty_head_fields() {
        // F2 (fail-closed): a trigger-tagged PR missing `fromRef`, or with an empty
        // `latestCommit` / `displayId`, is malformed → skipped (no empty-head candidate that
        // would corrupt the dispatch ledger key).
        let no_from_ref = format!(
            r#"{{ "isLastPage": true, "values": [
                {{ "id": 1, "title": "X [{REVIEW}]" }}
            ] }}"#
        );
        assert!(rows(&no_from_ref).expect("parses").is_empty());

        let empty_commit = format!(
            r#"{{ "isLastPage": true, "values": [
                {{ "id": 2, "title": "X [{REVIEW}]", "fromRef": {{ "displayId": "b", "latestCommit": "" }} }}
            ] }}"#
        );
        assert!(rows(&empty_commit).expect("parses").is_empty());

        let empty_branch = format!(
            r#"{{ "isLastPage": true, "values": [
                {{ "id": 3, "title": "X [{REVIEW}]", "fromRef": {{ "displayId": "", "latestCommit": "sha" }} }}
            ] }}"#
        );
        assert!(rows(&empty_branch).expect("parses").is_empty());
    }

    #[test]
    fn parse_rows_unknown_repo_identity_is_cross_repo_fail_closed() {
        // F3 (fail-closed): a trigger-tagged PR whose repo identity is unknown (no repository
        // id on either ref) is treated as cross-repo, so the shared `should_skip` gate drops it.
        let json = format!(
            r#"{{ "isLastPage": true, "values": [
                {{ "id": 4, "title": "X [{REVIEW}]", "fromRef": {{ "displayId": "b", "latestCommit": "sha" }} }}
            ] }}"#
        );
        let r = rows(&json).expect("parses");
        assert_eq!(r.len(), 1);
        assert!(
            r[0].candidate.is_cross_repository,
            "unknown repo identity → fail-closed cross-repo"
        );
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
            &trigger_labels(),
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
