//! Read-only pr-review comment URL resolver (AB#1042).
//!
//! The trigger funnel ([`super::session::finalize_turn`]) needs the URL of the `pm:`
//! pr-review comment the review subprocess posted, to hand back to a third-party trigger
//! (CLI/deeplink, future). The app itself NEVER writes comments — the governance backstop
//! (`crate::dispatch`'s `app_code_uses_no_gh_write_subcommands`) scans all of `src` for
//! gh write subcommands. Reading is allowed, so this resolves the URL source-kind-aware:
//!
//! - [`SourceKind::Github`]: shell out to read-only `gh pr view <pr> --repo <repo> --json
//!   comments` (mirroring `pr::gh`'s subprocess discipline — `Command::new` + a wall-clock
//!   timeout + `kill_on_drop(true)` + a bounded read), then [`pick_last_pm_comment_url`]
//!   picks the last `pm:` comment's URL from the JSON.
//! - [`SourceKind::Azure`]: no API call — [`azure_pr_url`] purely constructs the PR-level
//!   web URL (a comment thread URL would need a thread-id API round-trip we deliberately
//!   skip; the PR URL is the agreed Azure answer).
//! - [`SourceKind::Bitbucket`]: `None` (no resolver yet).
//!
//! The `match SourceKind` is EXHAUSTIVE (Hard carrier): a new variant fails to compile
//! until its resolution rule is added here.

use std::process::Stdio;

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::model::SourceKind;

/// Wall-clock budget for the single read-only `gh pr view` call. Mirrors `pr::gh`'s
/// `GH_TIMEOUT`: a hung subprocess (network stall, auth prompt) is bounded here, and
/// `kill_on_drop(true)` kills the child if the timed-out future is dropped.
const GH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Max `gh pr view` stdout we will read + parse. A PR's comment JSON is tiny in practice;
/// this bounds memory against a hostile / runaway stream (mirrors `pr::azure`'s bounded
/// read) BEFORE `serde_json::from_str`. 10 MiB is far above any realistic comment body set.
const GH_MAX_STDOUT_BYTES: usize = 10 * 1024 * 1024;

/// The `pm:` comment prefix the pr-review skill posts (the turn prompt instructs it to post
/// the `pm:pr-review` comment — see `super::session::review_prompt`). We match on the `pm:`
/// prefix (trimmed) so a future `pm:`-prefixed variant still resolves.
const PM_COMMENT_PREFIX: &str = "pm:";

/// Resolve the URL of this review's pr-review comment, source-kind-aware (AB#1042). Returns
/// `None` when the source has no resolver (Bitbucket) or the lookup yields nothing — the
/// funnel treats `None` as "no URL", never an error. EXHAUSTIVE `match SourceKind` (Hard
/// carrier): a new variant fails to compile until its arm is added.
///
/// `pub(super)` so only the review slice's funnel calls it (the slice boundary).
pub(super) async fn resolve_comment_url(
    source_kind: SourceKind,
    repo: &str,
    azure_org: &str,
    azure_project: &str,
    pr: u64,
) -> Option<String> {
    match source_kind {
        SourceKind::Github => {
            // Read-only `gh pr view` — never a write subcommand (governance backstop).
            let json = run_gh_pr_view(repo, pr).await?;
            pick_last_pm_comment_url(&json)
        }
        // Azure: the PR-level web URL, purely constructed (no thread-id API round-trip).
        SourceKind::Azure => Some(azure_pr_url(azure_org, azure_project, repo, pr)),
        // No Bitbucket resolver yet — the funnel records no URL.
        SourceKind::Bitbucket => None,
    }
}

/// One comment from `gh pr view --json comments` output (`{ "body": ..., "url": ... }`;
/// other fields ignored). `#[serde(default)]` so a missing field degrades to empty rather
/// than failing the whole parse.
#[derive(Debug, Deserialize)]
struct RawComment {
    #[serde(default)]
    body: String,
    #[serde(default)]
    url: String,
}

/// `gh pr view --json comments` top-level shape (`{ "comments": [ ... ] }`).
#[derive(Debug, Deserialize)]
struct RawPrView {
    #[serde(default)]
    comments: Vec<RawComment>,
}

/// Picks the URL of the LAST `pm:` pr-review comment in `gh pr view --json comments` output
/// (AB#1042) — the most recent review leaves its comment last, and a check turn appends a
/// new one. Pure (no `gh` call) so the multi-comment / no-pm / empty cases are unit-tested.
///
/// Returns `None` when the JSON doesn't parse, there are no comments, or none is a `pm:`
/// comment (whitespace-trimmed prefix match) with a non-empty URL.
fn pick_last_pm_comment_url(json: &str) -> Option<String> {
    let view: RawPrView = serde_json::from_str(json).ok()?;
    // `rfind` scans from the back for the LAST comment that is a non-empty `pm:` comment.
    view.comments
        .into_iter()
        .rfind(|c| c.body.trim_start().starts_with(PM_COMMENT_PREFIX) && !c.url.is_empty())
        .map(|c| c.url)
}

/// The Azure DevOps PR-level web URL (AB#1042), constructed purely — no `az`/API call. Each
/// path segment is percent-encoded (mirroring `pr::azure`'s `encode_path_segment`): config
/// allows spaces / unicode in org/project/repo (separate argv, no URL parsing), so a raw
/// interpolation would be malformed. `pullrequest` (no space) is intentional and does NOT
/// match the governance backstop's `pr-review`-with-a-space denylist.
fn azure_pr_url(org: &str, project: &str, repo: &str, pr: u64) -> String {
    format!(
        "https://dev.azure.com/{}/{}/_git/{}/pullrequest/{}",
        encode_path_segment(org),
        encode_path_segment(project),
        encode_path_segment(repo),
        pr
    )
}

/// Percent-encodes one URL PATH segment. A review-slice-local copy of `pr::azure`'s encoder
/// (the `pr` one is module-private, and the review slice must not import a `pr` internal):
/// each UTF-8 byte passes through iff RFC 3986 "unreserved" (`A-Za-z0-9-._~`), else `%XX`
/// (uppercase hex). A path segment never contains `/`, so `/` correctly encodes to `%2F`.
fn encode_path_segment(segment: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(segment.len());
    for &byte in segment.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0xf) as usize] as char);
        }
    }
    out
}

/// Runs the read-only `gh pr view <pr> --repo <repo> --json comments`, returning raw stdout
/// JSON, or `None` on any failure (gh missing, not authed, timeout, non-zero exit, oversize)
/// — the funnel degrades a failed resolve to "no URL", never an error. Mirrors `pr::gh`'s
/// subprocess discipline: `Command::new` + [`GH_TIMEOUT`] + `kill_on_drop(true)` + a bounded
/// stdout read.
async fn run_gh_pr_view(repo: &str, pr: u64) -> Option<String> {
    let pr_str = pr.to_string();
    let mut cmd = Command::new("gh");
    // `gh pr view` is READ-ONLY (the governance scan forbids only write subcommands).
    cmd.args(["pr", "view", &pr_str, "--repo", repo, "--json", "comments"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take()?;
    // Bound the read so a runaway stream can't exhaust memory; the whole call is under one
    // timeout (a dropped future + `kill_on_drop` kills the child).
    let read = tokio::time::timeout(GH_TIMEOUT, read_bounded(stdout, GH_MAX_STDOUT_BYTES)).await;
    let bytes = match read {
        Ok(Ok(bytes)) => bytes,
        // Timeout or a bounded-read error → no URL (best-effort).
        _ => return None,
    };
    let status = child.wait().await.ok()?;
    if !status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Reads `reader` to EOF but stops once more than `limit` bytes have arrived, returning
/// `None` rather than allocating unboundedly (mirrors `pr::azure`'s `read_bounded`). The
/// bound is applied at READ time, so memory never grows past ~`limit`. Generic over
/// [`AsyncRead`] so it is unit-tested with an in-memory `&[u8]`.
async fn read_bounded<R: AsyncRead + Unpin>(mut reader: R, limit: usize) -> Result<Vec<u8>, ()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        let n = reader.read(&mut chunk).await.map_err(|_| ())?;
        if n == 0 {
            return Ok(buf); // EOF within the limit.
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > limit {
            return Err(()); // over the bound → give up (memory stays ~limit).
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_last_pm_comment_url_takes_the_last_pm_comment() {
        // Two pm: comments (an earlier review + a later check) → the LAST one's url wins;
        // a non-pm comment is ignored regardless of position.
        let json = r#"{
            "comments": [
                { "body": "pm:pr-review round 1", "url": "https://gh/c/1" },
                { "body": "looks good 👍",        "url": "https://gh/c/2" },
                { "body": "pm:pr-review round 2", "url": "https://gh/c/3" }
            ]
        }"#;
        assert_eq!(
            pick_last_pm_comment_url(json),
            Some("https://gh/c/3".to_string())
        );
    }

    #[test]
    fn pick_last_pm_comment_url_none_without_pm_comment() {
        // No comment carries the `pm:` prefix → None.
        let json = r#"{
            "comments": [
                { "body": "first",  "url": "https://gh/c/1" },
                { "body": "second", "url": "https://gh/c/2" }
            ]
        }"#;
        assert_eq!(pick_last_pm_comment_url(json), None);
    }

    #[test]
    fn pick_last_pm_comment_url_none_on_empty_or_unparsable() {
        // Empty comments array → None.
        assert_eq!(pick_last_pm_comment_url(r#"{ "comments": [] }"#), None);
        // Missing `comments` key → defaults to empty → None.
        assert_eq!(pick_last_pm_comment_url(r#"{}"#), None);
        // Unparsable JSON → None (best-effort; never an error).
        assert_eq!(pick_last_pm_comment_url("not json"), None);
        // A pm: comment with an empty url is skipped (no URL to return).
        assert_eq!(
            pick_last_pm_comment_url(r#"{ "comments": [ { "body": "pm:x", "url": "" } ] }"#),
            None
        );
    }

    #[test]
    fn azure_pr_url_builds_pr_level_url_and_encodes_segments() {
        // Plain segments pass through.
        assert_eq!(
            azure_pr_url("myorg", "myproject", "myrepo", 12),
            "https://dev.azure.com/myorg/myproject/_git/myrepo/pullrequest/12"
        );
        // Spaces / unicode in project & repo (allowed by config) are percent-encoded so the
        // URL stays well-formed (golden: a space → %20, é → %C3%A9).
        assert_eq!(
            azure_pr_url("my org", "my project", "café", 7),
            "https://dev.azure.com/my%20org/my%20project/_git/caf%C3%A9/pullrequest/7"
        );
    }

    #[tokio::test]
    async fn read_bounded_accepts_up_to_limit_and_rejects_over() {
        let exact = [b'a'; 100];
        assert_eq!(
            read_bounded(&exact[..], 100).await.expect("at limit"),
            exact
        );
        let over = [b'b'; 101];
        assert!(
            read_bounded(&over[..], 100).await.is_err(),
            "over the bound"
        );
        let empty: [u8; 0] = [];
        assert!(read_bounded(&empty[..], 100)
            .await
            .expect("empty")
            .is_empty());
    }

    /// Bitbucket has no resolver → `None` regardless of args (the exhaustive `match`'s
    /// Bitbucket arm). The Azure arm's pure construction is covered by `azure_pr_url`
    /// above; the GitHub arm shells out (not unit-tested without a `gh` binary).
    #[tokio::test]
    async fn resolve_comment_url_bitbucket_is_none() {
        assert_eq!(
            resolve_comment_url(SourceKind::Bitbucket, "repo", "", "", 7).await,
            None
        );
        // Azure resolves to the constructed PR URL (no CLI call), so it is testable here too.
        assert_eq!(
            resolve_comment_url(SourceKind::Azure, "myrepo", "myorg", "myproject", 5).await,
            Some("https://dev.azure.com/myorg/myproject/_git/myrepo/pullrequest/5".to_string())
        );
    }
}
