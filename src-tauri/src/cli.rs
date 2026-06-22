//! `prmonitor review …` CLI subcommand (AB#1044 — CLI/Deeplink Phase 2).
//!
//! Composition-layer module (a sibling of [`crate::dispatch`]): it consumes the `review`
//! slice's local-API wire types + the `config` slice's loader, so it is composition, not a
//! slice.
//!
//! The binary is ALWAYS a THIN HTTP CLIENT over the AB#1043 local API — the request/response
//! channel that plays VS Code's `VSCODE_IPC_HOOK_CLI` / `code --wait` role: `POST /reviews`, then
//! with `--watch` poll `GET /reviews/{id}` to a terminal state and print the comment URL. The CLI
//! process NEVER becomes the GUI; exit codes follow `gh run watch` (always 0 unless `--exit-status`).
//!  - **App running** → trigger succeeds immediately.
//!  - **App not running** (connection refused) → the CLI LAUNCHES the app as a DETACHED child and
//!    keeps polling until its local API binds, then runs the same client path. Because the CLI
//!    stays a pure HTTP client (it never forwards argv via single-instance), `--watch`/`--json`/
//!    `--exit-status` work on cold start too AND no single-instance race can drop the request.
//!
//! **Governance (AB-robust).** The client REUSES the local API's `TriggerRequest` /
//! `TriggerResponse` / `StatusResponse` / `ErrorBody` structs — ONE definition, both sides
//! (Hard; the round-trip goldens live next to those structs in `local_api.rs`). The only datum
//! it must restate is the bundle identifier (to find `prmonitor.db` without a Tauri app); that
//! restatement is locked **Medium** by [`tests::app_identifier_matches_tauri_conf`].

use std::time::Duration;

use clap::{ArgGroup, Args, Parser, Subcommand};

use crate::config::service as config_service;
use crate::db::Database;
use crate::review::local_api::{ErrorBody, StatusResponse, TriggerRequest, TriggerResponse};
use crate::review::session::SessionStatus;

/// The bundle identifier (`tauri.conf.json` `identifier`). The CLI runs BEFORE any Tauri app
/// exists, so it cannot ask Tauri for `app_data_dir()`; it reconstructs the DB path as
/// `dirs::data_dir()/{APP_IDENTIFIER}/prmonitor.db` (Tauri's own convention). Restating the
/// identifier is the one unavoidable duplication — locked **Medium** by a golden test that
/// reads `tauri.conf.json` and asserts equality, so a future identifier change fails CI here.
const APP_IDENTIFIER: &str = "com.ghbvf.prmonitor";

/// `--watch` poll cadence (mirrors `gh run watch`'s steady low-frequency poll).
const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// Per-request HTTP timeout (the trigger + each poll). Generous, but a hung socket must not
/// block a CI `&&` chain forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser, Debug)]
#[command(name = "prmonitor", bin_name = "prmonitor")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Trigger a PR review on the running prmonitor app (or launch it, then trigger).
    Review(ReviewArgs),
}

/// `prmonitor review` arguments. Exactly one of `--repo` / `--project-id` identifies the project
/// (the `target` group); the rest mirror `gh run watch` ergonomics.
// `Debug` is hand-written below (not derived) so the bearer `token` is REDACTED — a future
// `{args:?}` log line must never spill the secret into stderr / a log file.
#[derive(Args, Clone)]
#[command(group = ArgGroup::new("target").required(true))]
pub struct ReviewArgs {
    /// PR / MR number (must be > 0; the trigger funnel rejects 0).
    #[arg(long)]
    pub pr: u64,
    /// Target project by `owner/name` repo (case-insensitive). One of --repo / --project-id.
    #[arg(long, group = "target")]
    pub repo: Option<String>,
    /// Target project by its configured project id. One of --repo / --project-id.
    #[arg(long = "project-id", group = "target")]
    pub project_id: Option<String>,
    /// Re-check a prior fix round (`kind=check`) instead of a full review.
    #[arg(long)]
    pub check: bool,
    /// Block until the review reaches a terminal state, then print the comment URL.
    #[arg(long)]
    pub watch: bool,
    /// Emit machine JSON. Bare `--json` prints the whole object; `--json status,commentUrl`
    /// projects those fields (gh convention).
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    pub json: Option<String>,
    /// With `--watch`, exit non-zero unless the review COMPLETED (a comment was posted).
    #[arg(long = "exit-status")]
    pub exit_status: bool,
    /// Override the local API port (else `PRMONITOR_LOCAL_API_PORT`, else saved config).
    #[arg(long)]
    pub port: Option<u16>,
    /// Override the bearer token (else `PRMONITOR_LOCAL_API_TOKEN`, else saved config).
    #[arg(long)]
    pub token: Option<String>,
}

impl ReviewArgs {
    /// The free-form `reference` the trigger funnel resolves (id-or-repo). The clap `target`
    /// group guarantees exactly one of repo / project_id is set.
    pub fn reference(&self) -> String {
        self.repo
            .clone()
            .or_else(|| self.project_id.clone())
            .unwrap_or_default()
    }

    /// `"check"` re-runs a prior fix round; otherwise a full `"review"` (the funnel whitelists
    /// exactly these two).
    pub fn kind(&self) -> &'static str {
        if self.check {
            "check"
        } else {
            "review"
        }
    }

    /// Build the POST body — the SAME struct the server deserializes (Hard, single-source).
    fn trigger_request(&self) -> TriggerRequest {
        TriggerRequest {
            project_id: self.project_id.clone(),
            repo: self.repo.clone(),
            pr: self.pr,
            kind: self.kind().to_string(),
        }
    }
}

/// Hand-written so the bearer `token` never appears in debug output (only whether one is set).
impl std::fmt::Debug for ReviewArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewArgs")
            .field("pr", &self.pr)
            .field("repo", &self.repo)
            .field("project_id", &self.project_id)
            .field("check", &self.check)
            .field("watch", &self.watch)
            .field("json", &self.json)
            .field("exit_status", &self.exit_status)
            .field("port", &self.port)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

/// What [`parse`] resolved the process invocation to.
pub enum Invocation {
    /// `prmonitor review …` — run the CLI client (which itself launches the app on a cold start).
    Review(ReviewArgs),
    /// Anything else — boot the GUI normally.
    Gui,
}

/// Parse `argv` for the `review` subcommand. clap's strict parser only runs when `argv[1] ==
/// "review"`, so a normal GUI launch (incl. macOS bundle args like `-psn_…`) never trips it.
/// On a malformed `review` invocation clap prints usage + exits (its default), which is correct
/// for a CLI.
pub fn parse() -> Invocation {
    let is_review = std::env::args().nth(1).as_deref() == Some("review");
    if !is_review {
        return Invocation::Gui;
    }
    match Cli::parse().command {
        Some(Command::Review(args)) => Invocation::Review(args),
        None => Invocation::Gui,
    }
}

/// Cold-start retry budget: after launching the app, how long to wait for its local API to bind,
/// and how often to retry the connect. The CLI stays a thin HTTP client the whole time (it never
/// becomes the GUI), so `--watch`/`--json`/`--exit-status` work on cold start too.
const COLD_START_DEADLINE: Duration = Duration::from_secs(30);
const COLD_START_POLL: Duration = Duration::from_millis(300);

/// The result of a single trigger POST.
enum Trigger {
    /// The app accepted the trigger (202) — carries the session id + status URL.
    Ok(TriggerResponse),
    /// Connection refused — nothing is listening (the app is not running yet).
    NotRunning,
    /// A definitive failure (HTTP 4xx/5xx, parse/transport error) — exit with this code.
    Failed(i32),
}

/// Synchronous entry for [`crate::run`] — owns a single-threaded tokio runtime for the client
/// (built before any Tauri runtime exists). Returns the process exit code.
pub fn run_client_blocking(args: &ReviewArgs) -> i32 {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("无法创建运行时: {e}");
            return 1;
        }
    };
    rt.block_on(run_client(args))
}

async fn run_client(args: &ReviewArgs) -> i32 {
    let endpoint = resolve_endpoint(args);
    if endpoint.port == 0 {
        // Single non-zero error code for every failure (gh: non-zero = failed); a CI `&&` chain
        // only cares that it is not 0.
        eprintln!("本地 API 已禁用（端口为 0）；在设置中设置 localApiPort 后重试");
        return 1;
    }
    // `redirect(none)`: the local API only ever returns 2xx/4xx/5xx, never a redirect. Refusing to
    // follow 3xx hardens the bearer-token requests — a rogue/hijacked listener cannot bounce the
    // token to another host via `Location` (defense-in-depth alongside the loopback URL check).
    let client = match reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("无法创建 HTTP 客户端: {e}");
            return 1;
        }
    };
    let base = format!("http://127.0.0.1:{}", endpoint.port);

    // 1) Trigger. If the app is running, this succeeds immediately. If it is NOT running, launch it
    //    and retry-connect until its local API binds — the CLI stays a thin HTTP client throughout
    //    (it never becomes the GUI nor forwards argv via single-instance), so `--watch`/`--json`/
    //    `--exit-status` work on cold start AND no single-instance race can drop the request.
    let trigger = match post_trigger(&client, &base, &endpoint.token, args).await {
        Trigger::Ok(tr) => tr,
        Trigger::Failed(code) => return code,
        Trigger::NotRunning => {
            eprintln!("app 未运行：正在启动 app…");
            if let Err(e) = spawn_detached_gui() {
                eprintln!("启动 app 失败：{e}");
                return 1;
            }
            match await_app_then_trigger(&client, &base, &endpoint.token, args).await {
                Trigger::Ok(tr) => tr,
                Trigger::Failed(code) => return code,
                Trigger::NotRunning => {
                    eprintln!("启动 app 后本地 API 未在 {COLD_START_DEADLINE:?} 内就绪");
                    return 1;
                }
            }
        }
    };

    // 2) No --watch: print the trigger result (id + statusUrl) and return success.
    if !args.watch {
        let value = serde_json::to_value(&trigger).unwrap_or(serde_json::Value::Null);
        emit(&args.json, &value, || {
            format!("review 已触发：{}\n{}", trigger.id, trigger.status_url)
        });
        return 0;
    }

    // 3) --watch: poll the server-provided status URL to a terminal state. The URL is
    // server-provided, so before polling it WITH the bearer token, confirm it is loopback — a
    // hijacked/rogue listener must never receive the token off-box.
    if !is_loopback_http_url(&trigger.status_url) {
        eprintln!("拒绝轮询非 loopback 的 statusUrl：{}", trigger.status_url);
        return 1;
    }
    watch_to_terminal(&client, &endpoint.token, &trigger.status_url, args).await
}

/// One trigger POST. Connection-refused is reported distinctly ([`Trigger::NotRunning`]) so the
/// caller can launch the app and retry; every other failure is terminal.
async fn post_trigger(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    args: &ReviewArgs,
) -> Trigger {
    let resp = client
        .post(format!("{base}/reviews"))
        .bearer_auth(token)
        .json(&args.trigger_request())
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) if e.is_connect() => return Trigger::NotRunning,
        Err(e) => {
            eprintln!("触发请求失败: {e}");
            return Trigger::Failed(1);
        }
    };
    if !resp.status().is_success() {
        return Trigger::Failed(report_http_error(resp).await);
    }
    match resp.json::<TriggerResponse>().await {
        Ok(tr) => Trigger::Ok(tr),
        Err(e) => {
            eprintln!("解析触发响应失败: {e}");
            Trigger::Failed(1)
        }
    }
}

/// After launching the app, retry [`post_trigger`] until its local API binds (the first non-refused
/// result wins — so exactly one review is triggered) or the cold-start deadline elapses.
async fn await_app_then_trigger(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    args: &ReviewArgs,
) -> Trigger {
    let mut waited = Duration::ZERO;
    loop {
        match post_trigger(client, base, token, args).await {
            Trigger::NotRunning => {
                if waited >= COLD_START_DEADLINE {
                    return Trigger::NotRunning;
                }
                tokio::time::sleep(COLD_START_POLL).await;
                waited += COLD_START_POLL;
            }
            settled => return settled,
        }
    }
}

/// Launch the GUI as a DETACHED child (this same binary with no args → the [`Invocation::Gui`] path
/// → `build_app`). stdio is nulled so the GUI's output never pollutes the CLI's stdout (which
/// carries the result); the child is not awaited, so it outlives this CLI process. single-instance
/// dedups if two cold starts race — both CLIs then connect to the one GUI's API.
fn spawn_detached_gui() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_child| ())
}

async fn watch_to_terminal(
    client: &reqwest::Client,
    token: &str,
    status_url: &str,
    args: &ReviewArgs,
) -> i32 {
    // No overall deadline — INTENTIONAL, matching `gh run watch`: a review runs as long as it
    // runs, and the caller's environment (a CI job timeout, or Ctrl-C) bounds the wait. The
    // per-request `REQUEST_TIMEOUT` still prevents a single hung socket from blocking forever.
    loop {
        // Poll FIRST (so a review that finishes quickly returns without an initial idle wait),
        // then sleep before the next round.
        let resp = match client.get(status_url).bearer_auth(token).send().await {
            Ok(r) => r,
            // Mid-watch connection loss (app quit) is a real error, NOT "never running" — report
            // it rather than silently falling back to a cold start.
            Err(e) => {
                eprintln!("轮询请求失败: {e}");
                return 1;
            }
        };
        if !resp.status().is_success() {
            return report_http_error(resp).await;
        }
        let status: StatusResponse = match resp.json().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("解析状态响应失败: {e}");
                return 1;
            }
        };
        if is_terminal(status.status) {
            let has_url = status.comment_url.is_some();
            let value = serde_json::to_value(&status).unwrap_or(serde_json::Value::Null);
            emit(&args.json, &value, || {
                human_status(status.status, status.comment_url.as_deref())
            });
            return exit_code(status.status, has_url, args.exit_status);
        }
        // Progress feedback to stderr (stdout stays reserved for the final result), so the user
        // can tell the command is waiting, not hung.
        eprintln!("⏳ 等待 review 完成（当前：{:?}）", status.status);
        tokio::time::sleep(WATCH_POLL_INTERVAL).await;
    }
}

/// Print a failed HTTP response's `{message}` body and map it to a non-zero exit code. A 4xx/5xx
/// is a usage/auth error (bad token, dedup, unknown project), NOT a review outcome, so it always
/// exits non-zero regardless of `--exit-status`.
async fn report_http_error(resp: reqwest::Response) -> i32 {
    let status = resp.status();
    let msg = resp
        .json::<ErrorBody>()
        .await
        .map(|b| b.message)
        .unwrap_or_else(|_| "（无错误详情）".to_string());
    eprintln!("请求失败（HTTP {}）：{}", status.as_u16(), msg);
    1
}

struct Endpoint {
    port: u16,
    token: String,
}

/// Resolve (port, token): flag > env > saved config (or its defaults). Never hard-fails — a
/// missing/unreadable config falls back to `AppConfig::default()` (port 8788, empty token), and
/// the POST result disambiguates: connection-refused ⇒ app not running (cold start); 401 ⇒ token
/// unset/wrong. (`port == 0` is handled by the caller as "API disabled".)
fn resolve_endpoint(args: &ReviewArgs) -> Endpoint {
    let cfg = load_saved_config().unwrap_or_default();
    let port = args.port.or_else(env_port).unwrap_or(cfg.local_api_port);
    let token = args
        .token
        .clone()
        .or_else(|| std::env::var("PRMONITOR_LOCAL_API_TOKEN").ok())
        .unwrap_or(cfg.local_api_token);
    Endpoint { port, token }
}

fn env_port() -> Option<u16> {
    std::env::var("PRMONITOR_LOCAL_API_PORT")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Read the saved `AppConfig` from `prmonitor.db` WITHOUT a Tauri app (read-only, no migration —
/// see [`Database::open_readonly_at`]). `None` on any failure (no db / locked / parse); the
/// caller falls back to defaults.
fn load_saved_config() -> Option<crate::config::model::AppConfig> {
    let path = dirs::data_dir()?.join(APP_IDENTIFIER).join("prmonitor.db");
    let db = Database::open_readonly_at(&path).ok()?;
    config_service::load_db(&db).ok()
}

/// Print either projected JSON (when `--json[=fields]` is set) or the human fallback.
fn emit(json_opt: &Option<String>, value: &serde_json::Value, human: impl FnOnce() -> String) {
    match json_opt {
        None => println!("{}", human()),
        Some(fields) => println!("{}", render_json(value, fields)),
    }
}

/// Render the `--json` output: a bare `--json` (empty fields) prints the whole compact object;
/// otherwise project the requested comma-separated fields (gh `--json` convention; an absent
/// field renders as `null`). Pure — unit-tested.
pub(crate) fn render_json(value: &serde_json::Value, fields: &str) -> String {
    let fields = fields.trim();
    if fields.is_empty() {
        return value.to_string();
    }
    // Build the object directly (not via `serde_json::Map`, which sorts keys without the
    // `preserve_order` feature) so the output keeps the user's requested field order; key + value
    // go through `serde_json::to_string` for correct quoting/escaping.
    let parts: Vec<String> = fields
        .split(',')
        .filter_map(|f| {
            let f = f.trim();
            if f.is_empty() {
                return None;
            }
            let val = value.get(f).cloned().unwrap_or(serde_json::Value::Null);
            let key = serde_json::to_string(f).ok()?;
            let val = serde_json::to_string(&val).ok()?;
            Some(format!("{key}:{val}"))
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

fn human_status(status: SessionStatus, comment_url: Option<&str>) -> String {
    match (status, comment_url) {
        (SessionStatus::Done, Some(url)) => format!("✓ review 完成：{url}"),
        (SessionStatus::Done, None) => "⚠ review 结束但未生成评论链接（可能被中断）".to_string(),
        (SessionStatus::Failed, _) => "✗ review 失败".to_string(),
        // Unreachable: only Done/Failed are terminal, but stay total.
        _ => format!("review 状态：{status:?}"),
    }
}

/// Whether `url` is a plain-HTTP loopback URL — the ONLY shape the local API emits
/// (`http://127.0.0.1:{port}/…`). Guards the bearer-token poll: a server-provided status URL
/// pointing off-box (a rogue/hijacked listener) must never receive the token.
///
/// PARSE the authority — a `starts_with` prefix check is fooled by userinfo, since the real host
/// of `http://127.0.0.1:8788@evil.com/…` is `evil.com` (codex F1). Reject any non-`http` scheme,
/// any embedded user/password, and any non-loopback host (which also rejects look-alikes like
/// `127.0.0.1.evil.com`). Pure — unit-tested incl. the userinfo bypass.
pub(crate) fn is_loopback_http_url(url: &str) -> bool {
    let parsed = match reqwest::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return false,
    };
    parsed.scheme() == "http"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && matches!(
            parsed.host_str(),
            Some("127.0.0.1") | Some("localhost") | Some("[::1]")
        )
}

/// Terminal = the review reached an end state (`done` or `failed`); polling stops. Pure.
pub(crate) fn is_terminal(status: SessionStatus) -> bool {
    matches!(status, SessionStatus::Done | SessionStatus::Failed)
}

/// `gh run watch` exit semantics. Without `--exit-status`, ALWAYS 0 (the trigger/poll succeeded
/// as a command). With `--exit-status`, 0 ONLY for a COMPLETED review — `Done` AND a comment URL
/// was posted; an interrupted (`Done` + no URL) or `Failed` review exits non-zero so a CI `&&`
/// chain stops. A rare completed-but-URL-unresolved review is a (documented) false negative. Pure.
pub(crate) fn exit_code(
    status: SessionStatus,
    has_comment_url: bool,
    exit_status_flag: bool,
) -> i32 {
    if !exit_status_flag {
        return 0;
    }
    match status {
        SessionStatus::Done if has_comment_url => 0,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_review(argv: &[&str]) -> Result<ReviewArgs, clap::Error> {
        match Cli::try_parse_from(argv)?.command {
            Some(Command::Review(a)) => Ok(a),
            None => panic!("expected a review subcommand"),
        }
    }

    #[test]
    fn parses_repo_target_and_defaults() {
        let a = parse_review(&["prmonitor", "review", "--pr", "7", "--repo", "owner/name"])
            .expect("valid");
        assert_eq!(a.pr, 7);
        assert_eq!(a.repo.as_deref(), Some("owner/name"));
        assert_eq!(a.project_id, None);
        assert!(!a.check && !a.watch && !a.exit_status);
        assert_eq!(a.json, None);
        assert_eq!(a.kind(), "review");
        assert_eq!(a.reference(), "owner/name");
    }

    #[test]
    fn project_id_target_and_check_flag() {
        let a = parse_review(&[
            "prmonitor",
            "review",
            "--pr",
            "3",
            "--project-id",
            "p1",
            "--check",
        ])
        .expect("valid");
        assert_eq!(a.reference(), "p1");
        assert_eq!(a.kind(), "check");
        let body = a.trigger_request();
        assert_eq!(body.project_id.as_deref(), Some("p1"));
        assert_eq!(body.repo, None);
        assert_eq!(body.pr, 3);
        assert_eq!(body.kind, "check");
    }

    #[test]
    fn target_group_requires_exactly_one() {
        // Neither repo nor project-id → error.
        assert!(parse_review(&["prmonitor", "review", "--pr", "7"]).is_err());
        // Both → error (the group is single-select).
        assert!(parse_review(&[
            "prmonitor",
            "review",
            "--pr",
            "7",
            "--repo",
            "o/n",
            "--project-id",
            "p1",
        ])
        .is_err());
    }

    #[test]
    fn json_flag_bare_vs_fields() {
        let bare = parse_review(&[
            "prmonitor",
            "review",
            "--pr",
            "7",
            "--repo",
            "o/n",
            "--json",
        ])
        .expect("valid");
        assert_eq!(bare.json.as_deref(), Some(""));
        let fields = parse_review(&[
            "prmonitor",
            "review",
            "--pr",
            "7",
            "--repo",
            "o/n",
            "--json",
            "status,commentUrl",
        ])
        .expect("valid");
        assert_eq!(fields.json.as_deref(), Some("status,commentUrl"));
    }

    #[test]
    fn render_json_projects_or_dumps() {
        let v = serde_json::json!({"status": "done", "commentUrl": "https://x/c"});
        // Bare → whole compact object.
        let whole = render_json(&v, "");
        assert!(whole.contains("\"status\":\"done\""));
        assert!(whole.contains("\"commentUrl\":\"https://x/c\""));
        // Field projection keeps order + only requested keys.
        assert_eq!(
            render_json(&v, "status,commentUrl"),
            "{\"status\":\"done\",\"commentUrl\":\"https://x/c\"}"
        );
        assert_eq!(render_json(&v, "status"), "{\"status\":\"done\"}");
        // Absent field → null.
        assert_eq!(render_json(&v, "missing"), "{\"missing\":null}");
        // All-empty field list (e.g. `--json ,`) → an empty object, not a crash.
        assert_eq!(render_json(&v, ","), "{}");
    }

    #[test]
    fn pr_zero_parses_at_cli_and_is_left_to_the_funnel() {
        // clap accepts `--pr 0` (no value_parser bound) ON PURPOSE: pr>0 is validated by the
        // single `trigger_review` funnel (`validate_pr_number`), so the CLI does not duplicate
        // that rule. A 0 reaches the server, which rejects it with a 400 → non-zero exit.
        let a =
            parse_review(&["prmonitor", "review", "--pr", "0", "--repo", "o/n"]).expect("valid");
        assert_eq!(a.pr, 0);
    }

    #[test]
    fn is_terminal_only_done_and_failed() {
        assert!(is_terminal(SessionStatus::Done));
        assert!(is_terminal(SessionStatus::Failed));
        assert!(!is_terminal(SessionStatus::Starting));
        assert!(!is_terminal(SessionStatus::Running));
        assert!(!is_terminal(SessionStatus::Interrupting));
    }

    #[test]
    fn exit_code_follows_gh_run_watch() {
        // Without --exit-status: always 0, even on failure.
        assert_eq!(exit_code(SessionStatus::Failed, false, false), 0);
        assert_eq!(exit_code(SessionStatus::Done, false, false), 0);
        // With --exit-status: 0 only for a completed review (Done + URL).
        assert_eq!(exit_code(SessionStatus::Done, true, true), 0);
        // Done without a URL = interrupted → non-zero.
        assert_eq!(exit_code(SessionStatus::Done, false, true), 1);
        assert_eq!(exit_code(SessionStatus::Failed, false, true), 1);
        // Failed is non-zero even if a URL is somehow present (only `Done` can succeed).
        assert_eq!(exit_code(SessionStatus::Failed, true, true), 1);
    }

    #[test]
    fn debug_redacts_token() {
        let a = parse_review(&[
            "prmonitor",
            "review",
            "--pr",
            "7",
            "--repo",
            "o/n",
            "--token",
            "supersecret",
        ])
        .expect("valid");
        let dbg = format!("{a:?}");
        assert!(
            !dbg.contains("supersecret"),
            "token must not appear in Debug: {dbg}"
        );
        assert!(dbg.contains("[REDACTED]"), "token field redacted: {dbg}");
    }

    #[test]
    fn loopback_url_guard() {
        assert!(is_loopback_http_url("http://127.0.0.1:8788/reviews/th-1"));
        assert!(is_loopback_http_url("http://localhost:8788/reviews/th-1"));
        assert!(is_loopback_http_url("http://[::1]:8788/reviews/th-1"));
        // Off-box / scheme / lookalike hosts are rejected (the token must never go there).
        assert!(!is_loopback_http_url("http://evil.com/reviews/th-1"));
        assert!(!is_loopback_http_url("https://127.0.0.1:8788/reviews/th-1"));
        assert!(!is_loopback_http_url(
            "http://127.0.0.1.evil.com/reviews/th-1"
        ));
        // codex F1: userinfo bypass — the real authority host is `evil.com`, NOT the prefix. A
        // `starts_with` prefix check passed these; the URL parser must reject them.
        assert!(!is_loopback_http_url(
            "http://127.0.0.1:8788@evil.com/reviews/th-1"
        ));
        assert!(!is_loopback_http_url(
            "http://127.0.0.1@evil.com/reviews/th-1"
        ));
        assert!(!is_loopback_http_url(
            "http://user:pass@127.0.0.1:8788/reviews/th-1"
        ));
        assert!(!is_loopback_http_url("not a url"));
    }

    /// **Medium** carrier: the restated bundle identifier must equal `tauri.conf.json`'s
    /// `identifier`, or the CLI resolves the WRONG `prmonitor.db` and silently reads no config.
    #[test]
    fn app_identifier_matches_tauri_conf() {
        let conf = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tauri.conf.json"))
            .expect("read tauri.conf.json");
        let v: serde_json::Value = serde_json::from_str(&conf).expect("parse tauri.conf.json");
        assert_eq!(v["identifier"].as_str(), Some(APP_IDENTIFIER));
    }
}
