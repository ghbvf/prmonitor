//! `prmonitor review …` CLI subcommand (AB#1044 — CLI/Deeplink Phase 2).
//!
//! Composition-layer module: it consumes the `review` slice's local-API wire types + the `config`
//! slice's loader, so it is composition, not a slice.
//!
//! The binary is ALWAYS a THIN HTTP CLIENT over the AB#1043 local API — the request/response
//! channel that plays VS Code's `VSCODE_IPC_HOOK_CLI` / `code --wait` role: `POST /reviews`, then
//! with `--watch` poll `GET /reviews/{id}` to a terminal state and print the comment URL. The CLI
//! process NEVER becomes the GUI; exit codes follow `gh run watch` (always 0 unless `--exit-status`).
//!  - **App running** → request succeeds immediately.
//!  - **App not running** (connection refused) → the CLI LAUNCHES the app as a DETACHED child and
//!    keeps polling until its local API binds, then runs the same client path. Because the CLI
//!    stays a pure HTTP client (it never forwards argv via single-instance), `--watch`/`--json`/
//!    `--exit-status` work on cold start too AND no single-instance race can drop the request.
//!
//! **Governance (AB-robust).** The client REUSES the local API's `ReviewRequestBody` /
//! `ReviewReceiptAccepted` / `StatusResponse` / `ErrorBody` structs — ONE definition, both sides
//! (Hard; the round-trip goldens live next to those structs in `local_api.rs`). The only datum
//! it must restate is the bundle identifier (to find `prmonitor.db` without a Tauri app); that
//! restatement is locked **Medium** by [`tests::app_identifier_matches_tauri_conf`].

use std::{future::Future, time::Duration};

use clap::{ArgGroup, Args, Parser, Subcommand};

use crate::config::service as config_service;
use crate::db::Database;
use crate::model::{
    ExternalRequestId, MessagingCardTemplate, MessagingEventEntry, MessagingSendContent,
    NotificationLevel, OutboxEntry, ReviewReceiptStatus, SendMessagingRequest,
    SendMessagingResponse, SendNotificationRequest, SendNotificationResponse,
};
use crate::review::local_api::{
    ErrorBody, ReviewReceiptAccepted, ReviewRequestBody, StatusResponse,
};

/// The bundle identifier (`tauri.conf.json` `identifier`). The CLI runs BEFORE any Tauri app
/// exists, so it cannot ask Tauri for `app_data_dir()`; it reconstructs the DB path as
/// `dirs::data_dir()/{APP_IDENTIFIER}/prmonitor.db` (Tauri's own convention). Restating the
/// identifier is the one unavoidable duplication — locked **Medium** by a golden test that
/// reads `tauri.conf.json` and asserts equality, so a future identifier change fails CI here.
const APP_IDENTIFIER: &str = "com.ghbvf.prmonitor";

/// `--watch` poll cadence (mirrors `gh run watch`'s steady low-frequency poll).
const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// Per-request HTTP timeout (the review request + each poll). Generous, but a hung socket must not
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
    /// Request a PR review from the running prmonitor app (or launch it, then request).
    Review(ReviewArgs),
    /// Enqueue a user-authored notification through the running prmonitor app.
    Notify(NotifyArgs),
    /// Send or inspect bidirectional messaging integration logs.
    Message(MessageArgs),
}

/// `prmonitor review` arguments. Exactly one of `--repo` / `--project-id` identifies the project
/// (the `target` group); the rest mirror `gh run watch` ergonomics.
// `Debug` is hand-written below (not derived) so the bearer `token` is REDACTED — a future
// `{args:?}` log line must never spill the secret into stderr / a log file.
#[derive(Args, Clone)]
#[command(group = ArgGroup::new("target").required(true))]
pub struct ReviewArgs {
    /// PR / MR number (must be > 0; the request funnel rejects 0).
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
    /// Reuse a logical request id after an ambiguous response to recover the original receipt.
    #[arg(long = "request-id", value_parser = parse_external_request_id)]
    pub request_id: Option<ExternalRequestId>,
    /// Override the local API port (else `PRMONITOR_LOCAL_API_PORT`, else saved config).
    #[arg(long)]
    pub port: Option<u16>,
    /// Override the bearer token (else `PRMONITOR_LOCAL_API_TOKEN`, else saved config).
    #[arg(long)]
    pub token: Option<String>,
}

/// `prmonitor notify` arguments. The command enqueues, then exits; provider delivery happens later
/// through the action outbox.
#[derive(Args, Clone)]
pub struct NotifyArgs {
    /// Notification title.
    #[arg(long)]
    pub title: String,
    /// Notification body text.
    #[arg(long)]
    pub body: Option<String>,
    /// Actionable URL shown with the notification.
    #[arg(long)]
    pub url: Option<String>,
    /// Severity level (`info`, `warning`, `error`). Defaults to `info`.
    #[arg(long, value_parser = parse_notification_level)]
    pub level: Option<NotificationLevel>,
    /// Optional project routing key for the outbox row.
    #[arg(long = "project-id")]
    pub project_id: Option<String>,
    /// Restrict delivery to a configured notification channel id. Repeatable.
    #[arg(long = "channel")]
    pub channel_ids: Vec<String>,
    /// Emit machine JSON.
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    pub json: Option<String>,
    /// Override the local API port (else `PRMONITOR_LOCAL_API_PORT`, else saved config).
    #[arg(long)]
    pub port: Option<u16>,
    /// Override the bearer token (else `PRMONITOR_LOCAL_API_TOKEN`, else saved config).
    #[arg(long)]
    pub token: Option<String>,
}

#[derive(Args, Clone, Debug)]
pub struct MessageArgs {
    #[command(subcommand)]
    pub command: MessageCommand,
}

#[derive(Subcommand, Clone)]
pub enum MessageCommand {
    /// Enqueue an active messaging send.
    Send(MessageSendArgs),
    /// Enqueue a non-interactive Feishu information card.
    SendCard(MessageSendCardArgs),
    /// List received messaging events.
    Events(MessageLogArgs),
    /// List messaging send/reply outbox rows.
    Sends(MessageLogArgs),
}

#[derive(Args, Clone)]
pub struct MessageSendArgs {
    #[arg(long = "integration-id")]
    pub integration_id: String,
    #[arg(long = "conversation-id")]
    pub conversation_id: String,
    #[arg(long)]
    pub text: String,
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    pub json: Option<String>,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub token: Option<String>,
}

#[derive(Args, Clone)]
pub struct MessageSendCardArgs {
    #[arg(long = "integration-id")]
    pub integration_id: String,
    #[arg(long = "conversation-id")]
    pub conversation_id: String,
    #[arg(long)]
    pub title: String,
    /// Markdown card body.
    #[arg(long)]
    pub text: String,
    #[arg(long, value_parser = parse_messaging_card_template)]
    pub template: MessagingCardTemplate,
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    pub json: Option<String>,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub token: Option<String>,
}

#[derive(Args, Clone)]
pub struct MessageLogArgs {
    #[arg(long = "integration-id")]
    pub integration_id: Option<String>,
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    pub json: Option<String>,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub token: Option<String>,
}

impl ReviewArgs {
    /// The free-form `reference` the request funnel resolves (id-or-repo). The clap `target`
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
    fn review_request_body(&self, request_id: ExternalRequestId) -> ReviewRequestBody {
        ReviewRequestBody {
            project_id: self.project_id.clone(),
            repo: self.repo.clone(),
            pr: self.pr,
            kind: self.kind().parse().expect("CLI kind is sealed"),
            request_id,
        }
    }
}

impl NotifyArgs {
    fn notification_request(&self) -> SendNotificationRequest {
        SendNotificationRequest {
            level: self.level,
            title: self.title.clone(),
            body: self.body.clone(),
            url: self.url.clone(),
            project_id: self.project_id.clone(),
            channel_ids: self.channel_ids.clone(),
        }
    }
}

impl MessageSendArgs {
    fn send_request(&self) -> SendMessagingRequest {
        SendMessagingRequest {
            integration_id: self.integration_id.clone(),
            conversation_id: self.conversation_id.clone(),
            content: MessagingSendContent::Text {
                text: self.text.clone(),
            },
            request_id: new_request_id().into_inner(),
        }
    }
}

impl MessageSendCardArgs {
    fn send_request(&self) -> SendMessagingRequest {
        SendMessagingRequest {
            integration_id: self.integration_id.clone(),
            conversation_id: self.conversation_id.clone(),
            content: MessagingSendContent::Card {
                title: self.title.clone(),
                text: self.text.clone(),
                template: self.template,
            },
            request_id: new_request_id().into_inner(),
        }
    }
}

fn parse_notification_level(value: &str) -> Result<NotificationLevel, String> {
    value.parse()
}

fn parse_messaging_card_template(value: &str) -> Result<MessagingCardTemplate, String> {
    value.parse()
}

fn parse_external_request_id(value: &str) -> Result<ExternalRequestId, String> {
    ExternalRequestId::parse(value)
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
            .field("request_id", &self.request_id)
            .field("port", &self.port)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

/// Hand-written so the bearer `token` and user-authored `body` never appear in debug output.
impl std::fmt::Debug for NotifyArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotifyArgs")
            .field("title", &self.title)
            .field("body", &self.body.as_ref().map(|_| "[REDACTED]"))
            .field("url", &self.url)
            .field("level", &self.level)
            .field("project_id", &self.project_id)
            .field("channel_ids", &self.channel_ids)
            .field("json", &self.json)
            .field("port", &self.port)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl std::fmt::Debug for MessageSendArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageSendArgs")
            .field("integration_id", &self.integration_id)
            .field("conversation_id", &self.conversation_id)
            .field("text", &"[REDACTED]")
            .field("json", &self.json)
            .field("port", &self.port)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl std::fmt::Debug for MessageSendCardArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageSendCardArgs")
            .field("integration_id", &self.integration_id)
            .field("conversation_id", &self.conversation_id)
            .field("title", &"[REDACTED]")
            .field("text", &"[REDACTED]")
            .field("template", &self.template)
            .field("json", &self.json)
            .field("port", &self.port)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl std::fmt::Debug for MessageLogArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageLogArgs")
            .field("integration_id", &self.integration_id)
            .field("json", &self.json)
            .field("port", &self.port)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl std::fmt::Debug for MessageCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageCommand::Send(args) => f.debug_tuple("Send").field(args).finish(),
            MessageCommand::SendCard(args) => f.debug_tuple("SendCard").field(args).finish(),
            MessageCommand::Events(args) => f.debug_tuple("Events").field(args).finish(),
            MessageCommand::Sends(args) => f.debug_tuple("Sends").field(args).finish(),
        }
    }
}

/// What [`parse`] resolved the process invocation to.
pub enum Invocation {
    /// `prmonitor review …` — run the CLI client (which itself launches the app on a cold start).
    Review(ReviewArgs),
    /// `prmonitor notify …` — enqueue a notification through the same local API client path.
    Notify(NotifyArgs),
    Message(MessageArgs),
    /// Anything else — boot the GUI normally.
    Gui,
}

/// Parse `argv` for the `review` subcommand. clap's strict parser only runs when `argv[1] ==
/// "review"`, so a normal GUI launch (incl. macOS bundle args like `-psn_…`) never trips it.
/// On a malformed `review` invocation clap prints usage + exits (its default), which is correct
/// for a CLI.
pub fn parse() -> Invocation {
    let first = std::env::args().nth(1);
    let is_cli = matches!(first.as_deref(), Some("review" | "notify" | "message"));
    if !is_cli {
        return Invocation::Gui;
    }
    match Cli::parse().command {
        Some(Command::Review(args)) => Invocation::Review(args),
        Some(Command::Notify(args)) => Invocation::Notify(args),
        Some(Command::Message(args)) => Invocation::Message(args),
        None => Invocation::Gui,
    }
}

/// Cold-start retry budget: after launching the app, how long to wait for its local API to bind,
/// and how often to retry the connect. The CLI stays a thin HTTP client the whole time (it never
/// becomes the GUI), so `--watch`/`--json`/`--exit-status` work on cold start too.
const COLD_START_DEADLINE: Duration = Duration::from_secs(30);
const COLD_START_POLL: Duration = Duration::from_millis(300);
const AMBIGUOUS_REQUEST_RETRIES: usize = 3;
const AMBIGUOUS_REQUEST_RETRY_DELAY: Duration = Duration::from_millis(300);

/// The result of a single review-request POST.
enum ReviewRequestAttempt {
    /// The app accepted the durable request (202) — carries the receipt id + status URL.
    Accepted(ReviewReceiptAccepted),
    /// Connection refused — nothing is listening (the app is not running yet).
    NotRunning,
    /// The server may have persisted the request, but the client did not receive a usable response.
    Ambiguous(String),
    /// A definitive failure (HTTP 4xx/5xx, parse/transport error) — exit with this code.
    Rejected(i32),
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

/// Synchronous entry for `prmonitor notify`.
pub fn run_notify_client_blocking(args: &NotifyArgs) -> i32 {
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
    rt.block_on(run_notify_client(args))
}

pub fn run_message_client_blocking(args: &MessageArgs) -> i32 {
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
    rt.block_on(run_message_client(args))
}

async fn run_client(args: &ReviewArgs) -> i32 {
    let endpoint = resolve_endpoint(args.port, args.token.clone());
    if endpoint.port == 0 {
        // Single non-zero error code for every failure (gh: non-zero = failed); a CI `&&` chain
        // only cares that it is not 0.
        eprintln!("本地 API 已禁用（端口为 0）；请在「设置 → 远程访问」中为 local-api 监听器设置端口并启用后重试");
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
    let base = endpoint.base_url();
    // One id for the logical CLI request, reused across cold-start/reconnect retries. Generating
    // inside the POST helper would turn a transport retry into a second durable review request.
    let request_id = args.request_id.clone().unwrap_or_else(new_request_id);

    // 1) Submit. If the app is running, this succeeds immediately. If it is NOT running, launch it
    //    and retry-connect until its local API binds — the CLI stays a thin HTTP client throughout
    //    (it never becomes the GUI nor forwards argv via single-instance), so `--watch`/`--json`/
    //    `--exit-status` work on cold start AND no single-instance race can drop the request.
    let accepted = match submit_review_request(&client, &base, &endpoint.token, args, &request_id)
        .await
    {
        ReviewRequestAttempt::Accepted(response) => response,
        ReviewRequestAttempt::Rejected(code) => return code,
        ReviewRequestAttempt::Ambiguous(_) => unreachable!("ambiguity is settled by retry wrapper"),
        ReviewRequestAttempt::NotRunning => {
            eprintln!("app 未运行：正在启动 app…");
            if let Err(e) = spawn_detached_gui() {
                eprintln!("启动 app 失败：{e}");
                return 1;
            }
            match await_app_then_request(&client, &base, &endpoint.token, args, &request_id).await {
                ReviewRequestAttempt::Accepted(response) => response,
                ReviewRequestAttempt::Rejected(code) => return code,
                ReviewRequestAttempt::Ambiguous(_) => {
                    unreachable!("ambiguity is settled by retry wrapper")
                }
                ReviewRequestAttempt::NotRunning => {
                    eprintln!("启动 app 后本地 API 未在 {COLD_START_DEADLINE:?} 内就绪");
                    return 1;
                }
            }
        }
    };

    // 2) No --watch: print the accepted receipt (id + statusUrl) and return success.
    if !args.watch {
        let value = serde_json::to_value(&accepted).unwrap_or(serde_json::Value::Null);
        emit(&args.json, &value, || {
            format!(
                "review 已入队（receipt {}）：\n{}",
                accepted.receipt_id.get(),
                accepted.status_url
            )
        });
        return 0;
    }

    // 3) --watch: poll the server-provided status URL to a terminal state. The URL is
    // server-provided, so before polling it WITH the bearer token, confirm it is loopback — a
    // hijacked/rogue listener must never receive the token off-box.
    if !is_loopback_http_url(&accepted.status_url) {
        eprintln!("拒绝轮询非 loopback 的 statusUrl：{}", accepted.status_url);
        return 1;
    }
    watch_to_terminal(&client, &endpoint.token, &accepted.status_url, args).await
}

async fn run_notify_client(args: &NotifyArgs) -> i32 {
    let endpoint = resolve_endpoint(args.port, args.token.clone());
    if endpoint.port == 0 {
        eprintln!("本地 API 已禁用（端口为 0）；请在「设置 → 远程访问」中为 local-api 监听器设置端口并启用后重试");
        return 1;
    }
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
    let base = endpoint.base_url();
    match post_notification(&client, &base, &endpoint.token, args).await {
        NotifyPost::Ok(response) => {
            let value = serde_json::to_value(&response).unwrap_or(serde_json::Value::Null);
            emit(&args.json, &value, || {
                format!("notification 已入队：{:?}", response.outbox_ids)
            });
            0
        }
        NotifyPost::Failed(code) => code,
        NotifyPost::NotRunning => {
            eprintln!("app 未运行：正在启动 app…");
            if let Err(e) = spawn_detached_gui() {
                eprintln!("启动 app 失败：{e}");
                return 1;
            }
            let mut waited = Duration::ZERO;
            loop {
                match post_notification(&client, &base, &endpoint.token, args).await {
                    NotifyPost::NotRunning => {
                        if waited >= COLD_START_DEADLINE {
                            eprintln!("启动 app 后本地 API 未在 {COLD_START_DEADLINE:?} 内就绪");
                            return 1;
                        }
                        tokio::time::sleep(COLD_START_POLL).await;
                        waited += COLD_START_POLL;
                    }
                    NotifyPost::Ok(response) => {
                        let value =
                            serde_json::to_value(&response).unwrap_or(serde_json::Value::Null);
                        emit(&args.json, &value, || {
                            format!("notification 已入队：{:?}", response.outbox_ids)
                        });
                        return 0;
                    }
                    NotifyPost::Failed(code) => return code,
                }
            }
        }
    }
}

async fn run_message_client(args: &MessageArgs) -> i32 {
    match &args.command {
        MessageCommand::Send(send) => run_message_send_client(send).await,
        MessageCommand::SendCard(send) => run_message_send_card_client(send).await,
        MessageCommand::Events(logs) => run_message_events_client(logs).await,
        MessageCommand::Sends(logs) => run_message_sends_client(logs).await,
    }
}

async fn run_message_send_client(args: &MessageSendArgs) -> i32 {
    run_message_send_request(
        args.send_request(),
        args.port,
        args.token.clone(),
        &args.json,
    )
    .await
}

async fn run_message_send_card_client(args: &MessageSendCardArgs) -> i32 {
    run_message_send_request(
        args.send_request(),
        args.port,
        args.token.clone(),
        &args.json,
    )
    .await
}

async fn run_message_send_request(
    request: SendMessagingRequest,
    port: Option<u16>,
    token: Option<String>,
    json: &Option<String>,
) -> i32 {
    let endpoint = resolve_endpoint(port, token);
    if endpoint.port == 0 {
        eprintln!("本地 API 已禁用（端口为 0）；请在「设置 → 远程访问」中为 local-api 监听器设置端口并启用后重试");
        return 1;
    }
    let client = match cli_http_client() {
        Ok(c) => c,
        Err(code) => return code,
    };
    let base = endpoint.base_url();
    match with_message_cold_start(|| post_message_send(&client, &base, &endpoint.token, &request))
        .await
    {
        Ok(response) => {
            let value = serde_json::to_value(&response).unwrap_or(serde_json::Value::Null);
            emit(json, &value, || {
                format!("message 已入队：{}", response.outbox_id)
            });
            0
        }
        Err(code) => code,
    }
}

async fn run_message_events_client(args: &MessageLogArgs) -> i32 {
    let endpoint = resolve_endpoint(args.port, args.token.clone());
    if endpoint.port == 0 {
        eprintln!("本地 API 已禁用（端口为 0）；请在「设置 → 远程访问」中为 local-api 监听器设置端口并启用后重试");
        return 1;
    }
    let client = match cli_http_client() {
        Ok(c) => c,
        Err(code) => return code,
    };
    let base = endpoint.base_url();
    match with_message_cold_start(|| get_message_events(&client, &base, &endpoint.token, args))
        .await
    {
        Ok(entries) => {
            let value = serde_json::to_value(&entries).unwrap_or(serde_json::Value::Null);
            emit(&args.json, &value, || format_message_events(&entries));
            0
        }
        Err(code) => code,
    }
}

async fn run_message_sends_client(args: &MessageLogArgs) -> i32 {
    let endpoint = resolve_endpoint(args.port, args.token.clone());
    if endpoint.port == 0 {
        eprintln!("本地 API 已禁用（端口为 0）；请在「设置 → 远程访问」中为 local-api 监听器设置端口并启用后重试");
        return 1;
    }
    let client = match cli_http_client() {
        Ok(c) => c,
        Err(code) => return code,
    };
    let base = endpoint.base_url();
    match with_message_cold_start(|| get_message_sends(&client, &base, &endpoint.token, args)).await
    {
        Ok(entries) => {
            let value = serde_json::to_value(&entries).unwrap_or(serde_json::Value::Null);
            emit(&args.json, &value, || format_message_sends(&entries));
            0
        }
        Err(code) => code,
    }
}

fn cli_http_client() -> Result<reqwest::Client, i32> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| {
            eprintln!("无法创建 HTTP 客户端: {e}");
            1
        })
}

async fn with_message_cold_start<T, F, Fut>(mut attempt: F) -> Result<T, i32>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = MessageAttempt<T>>,
{
    let mut waited = Duration::ZERO;
    loop {
        match attempt().await {
            MessageAttempt::NotRunning if waited == Duration::ZERO => {
                eprintln!("app 未运行：正在启动 app…");
                if let Err(e) = spawn_detached_gui() {
                    eprintln!("启动 app 失败：{e}");
                    return Err(1);
                }
            }
            MessageAttempt::NotRunning => {
                if waited >= COLD_START_DEADLINE {
                    eprintln!("启动 app 后本地 API 未在 {COLD_START_DEADLINE:?} 内就绪");
                    return Err(1);
                }
            }
            MessageAttempt::Ok(value) => return Ok(value),
            MessageAttempt::Failed(code) => return Err(code),
        }
        tokio::time::sleep(COLD_START_POLL).await;
        waited += COLD_START_POLL;
    }
}

enum MessageAttempt<T> {
    Ok(T),
    NotRunning,
    Failed(i32),
}

async fn post_message_send(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    request: &SendMessagingRequest,
) -> MessageAttempt<SendMessagingResponse> {
    let resp = client
        .post(format!("{base}/messaging/send"))
        .bearer_auth(token)
        .json(request)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) if e.is_connect() => return MessageAttempt::NotRunning,
        Err(e) => {
            eprintln!("消息发送请求失败: {e}");
            return MessageAttempt::Failed(1);
        }
    };
    if !resp.status().is_success() {
        return MessageAttempt::Failed(report_http_error(resp).await);
    }
    match resp.json::<SendMessagingResponse>().await {
        Ok(response) => MessageAttempt::Ok(response),
        Err(e) => {
            eprintln!("解析消息发送响应失败: {e}");
            MessageAttempt::Failed(1)
        }
    }
}

async fn get_message_events(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    args: &MessageLogArgs,
) -> MessageAttempt<Vec<MessagingEventEntry>> {
    let mut req = client
        .get(format!("{base}/messaging/events"))
        .bearer_auth(token);
    if let Some(id) = args.integration_id.as_deref() {
        req = req.query(&[("integrationId", id)]);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) if e.is_connect() => return MessageAttempt::NotRunning,
        Err(e) => {
            eprintln!("消息接收日志请求失败: {e}");
            return MessageAttempt::Failed(1);
        }
    };
    if !resp.status().is_success() {
        return MessageAttempt::Failed(report_http_error(resp).await);
    }
    match resp.json::<Vec<MessagingEventEntry>>().await {
        Ok(entries) => MessageAttempt::Ok(entries),
        Err(e) => {
            eprintln!("解析消息接收日志响应失败: {e}");
            MessageAttempt::Failed(1)
        }
    }
}

async fn get_message_sends(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    args: &MessageLogArgs,
) -> MessageAttempt<Vec<OutboxEntry>> {
    let mut req = client
        .get(format!("{base}/messaging/sends"))
        .bearer_auth(token);
    if let Some(id) = args.integration_id.as_deref() {
        req = req.query(&[("integrationId", id)]);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) if e.is_connect() => return MessageAttempt::NotRunning,
        Err(e) => {
            eprintln!("消息发送日志请求失败: {e}");
            return MessageAttempt::Failed(1);
        }
    };
    if !resp.status().is_success() {
        return MessageAttempt::Failed(report_http_error(resp).await);
    }
    match resp.json::<Vec<OutboxEntry>>().await {
        Ok(entries) => MessageAttempt::Ok(entries),
        Err(e) => {
            eprintln!("解析消息发送日志响应失败: {e}");
            MessageAttempt::Failed(1)
        }
    }
}

enum NotifyPost {
    Ok(SendNotificationResponse),
    NotRunning,
    Failed(i32),
}

async fn post_notification(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    args: &NotifyArgs,
) -> NotifyPost {
    let resp = client
        .post(format!("{base}/notifications"))
        .bearer_auth(token)
        .json(&args.notification_request())
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) if e.is_connect() => return NotifyPost::NotRunning,
        Err(e) => {
            eprintln!("通知请求失败: {e}");
            return NotifyPost::Failed(1);
        }
    };
    if !resp.status().is_success() {
        return NotifyPost::Failed(report_http_error(resp).await);
    }
    match resp.json::<SendNotificationResponse>().await {
        Ok(response) => NotifyPost::Ok(response),
        Err(e) => {
            eprintln!("解析通知响应失败: {e}");
            NotifyPost::Failed(1)
        }
    }
}

/// One review-request POST. Connection-refused is distinct so the caller can launch the app.
/// Transport/response-decode failures are ambiguous: the durable insert may already have committed.
async fn post_review_request_once(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    args: &ReviewArgs,
    request_id: &ExternalRequestId,
) -> ReviewRequestAttempt {
    let resp = client
        .post(format!("{base}/reviews"))
        .bearer_auth(token)
        .header("x-prmonitor-client", "cli")
        .json(&args.review_request_body(request_id.clone()))
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) if e.is_connect() => return ReviewRequestAttempt::NotRunning,
        Err(e) => {
            return ReviewRequestAttempt::Ambiguous(format!("review 请求响应不确定: {e}"));
        }
    };
    if !resp.status().is_success() {
        return ReviewRequestAttempt::Rejected(report_http_error(resp).await);
    }
    match resp.json::<ReviewReceiptAccepted>().await {
        Ok(response) => ReviewRequestAttempt::Accepted(response),
        Err(e) => ReviewRequestAttempt::Ambiguous(format!("解析 review receipt 响应失败: {e}")),
    }
}

/// Retry ambiguous request outcomes with the SAME request id. The server's durable request-id
/// dedupe makes this safe when the first POST committed but its response was lost.
async fn submit_review_request(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    args: &ReviewArgs,
    request_id: &ExternalRequestId,
) -> ReviewRequestAttempt {
    for attempt in 0..=AMBIGUOUS_REQUEST_RETRIES {
        match post_review_request_once(client, base, token, args, request_id).await {
            ReviewRequestAttempt::Ambiguous(message) if attempt < AMBIGUOUS_REQUEST_RETRIES => {
                eprintln!(
                    "{message}；使用同一 requestId 重试（{}/{AMBIGUOUS_REQUEST_RETRIES}）",
                    attempt + 1
                );
                tokio::time::sleep(AMBIGUOUS_REQUEST_RETRY_DELAY).await;
            }
            ReviewRequestAttempt::Ambiguous(message) => {
                eprintln!(
                    "{message}；重试已耗尽。请求可能已经入队，请用相同参数加 --request-id={} 重试以恢复原 receipt。",
                    request_id.as_str()
                );
                return ReviewRequestAttempt::Rejected(1);
            }
            settled => return settled,
        }
    }
    unreachable!("bounded retry loop always returns")
}

/// After launching the app, retry [`submit_review_request`] until its local API binds (the first non-refused
/// result wins — so exactly one durable request is submitted) or the cold-start deadline elapses.
async fn await_app_then_request(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    args: &ReviewArgs,
    request_id: &ExternalRequestId,
) -> ReviewRequestAttempt {
    let mut waited = Duration::ZERO;
    loop {
        match submit_review_request(client, base, token, args, request_id).await {
            ReviewRequestAttempt::NotRunning => {
                if waited >= COLD_START_DEADLINE {
                    return ReviewRequestAttempt::NotRunning;
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
                human_status(
                    status.status,
                    status.comment_url.as_deref(),
                    status.error.as_deref(),
                )
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
    base_path: String,
}

impl Endpoint {
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port, self.base_path)
    }
}

/// Resolve (port, token): flag > env > saved config (or its defaults). Never hard-fails — a
/// missing/unreadable config falls back to `AppConfig::default()` (port 8788, empty token), and
/// the POST result disambiguates: connection-refused ⇒ app not running (cold start); 401 ⇒ token
/// unset/wrong. (`port == 0` is handled by the caller as "API disabled".) The port now comes from
/// the local-api `listeners[]` entry via `config::service::local_api_port` (AB#1225 single source).
fn resolve_endpoint(port: Option<u16>, token: Option<String>) -> Endpoint {
    let cfg = load_saved_config().unwrap_or_default();
    let port = port
        .or_else(env_port)
        .unwrap_or(crate::config::service::local_api_port(&cfg));
    let base_path = crate::config::service::local_api_path(&cfg);
    let token = token
        .or_else(|| std::env::var("PRMONITOR_LOCAL_API_TOKEN").ok())
        .unwrap_or(cfg.local_api_token);
    Endpoint {
        port,
        token,
        base_path,
    }
}

fn new_request_id() -> ExternalRequestId {
    ExternalRequestId::parse(uuid::Uuid::new_v4().simple().to_string())
        .expect("UUID v4 simple form is 32 lowercase hexadecimal characters")
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

fn format_message_events(entries: &[MessagingEventEntry]) -> String {
    if entries.is_empty() {
        return "messaging events: 0".to_string();
    }
    let mut lines = vec![format!("messaging events: {}", entries.len())];
    for entry in entries.iter().take(20) {
        lines.push(format!(
            "#{} {} {} {} {}",
            entry.id,
            entry.status.as_wire(),
            entry.event.provider.as_wire(),
            entry.event.integration_id,
            compact_text(&entry.event.text)
        ));
    }
    lines.join("\n")
}

fn format_message_sends(entries: &[OutboxEntry]) -> String {
    if entries.is_empty() {
        return "messaging sends: 0".to_string();
    }
    let mut lines = vec![format!("messaging sends: {}", entries.len())];
    for entry in entries.iter().take(20) {
        let error = entry
            .last_error
            .as_deref()
            .map(|e| format!(" error={}", compact_text(e)))
            .unwrap_or_default();
        lines.push(format!(
            "#{} {:?} {:?} attempts={} {}{}",
            entry.id, entry.kind, entry.status, entry.attempt_count, entry.summary, error
        ));
    }
    lines.join("\n")
}

fn compact_text(value: &str) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.len() <= 80 {
        value
    } else {
        let mut end = 80;
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &value[..end])
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

fn human_status(
    status: ReviewReceiptStatus,
    comment_url: Option<&str>,
    error: Option<&str>,
) -> String {
    match (status, comment_url) {
        (ReviewReceiptStatus::Done, Some(url)) => format!("✓ review 完成：{url}"),
        (ReviewReceiptStatus::Done, None) => {
            "⚠ review 结束但未生成评论链接（可能被中断）".to_string()
        }
        (ReviewReceiptStatus::Failed, _) => error
            .map(|message| format!("✗ review 失败：{message}"))
            .unwrap_or_else(|| "✗ review 失败".to_string()),
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
pub(crate) fn is_terminal(status: ReviewReceiptStatus) -> bool {
    matches!(
        status,
        ReviewReceiptStatus::Done | ReviewReceiptStatus::Failed
    )
}

/// `gh run watch` exit semantics. Without `--exit-status`, ALWAYS 0 (the trigger/poll succeeded
/// as a command). With `--exit-status`, 0 ONLY for a COMPLETED review — `Done` AND a comment URL
/// was posted; an interrupted (`Done` + no URL) or `Failed` review exits non-zero so a CI `&&`
/// chain stops. A rare completed-but-URL-unresolved review is a (documented) false negative. Pure.
pub(crate) fn exit_code(
    status: ReviewReceiptStatus,
    has_comment_url: bool,
    exit_status_flag: bool,
) -> i32 {
    if !exit_status_flag {
        return 0;
    }
    match status {
        ReviewReceiptStatus::Done if has_comment_url => 0,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_review(argv: &[&str]) -> Result<ReviewArgs, clap::Error> {
        match Cli::try_parse_from(argv)?.command {
            Some(Command::Review(a)) => Ok(a),
            _ => panic!("expected a review subcommand"),
        }
    }

    fn parse_notify(argv: &[&str]) -> Result<NotifyArgs, clap::Error> {
        match Cli::try_parse_from(argv)?.command {
            Some(Command::Notify(a)) => Ok(a),
            _ => panic!("expected a notify subcommand"),
        }
    }

    fn parse_message(argv: &[&str]) -> Result<MessageArgs, clap::Error> {
        match Cli::try_parse_from(argv)?.command {
            Some(Command::Message(a)) => Ok(a),
            _ => panic!("expected a message subcommand"),
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
        let body = a.review_request_body(
            ExternalRequestId::parse("0123456789abcdef0123456789abcdef").expect("request id"),
        );
        assert_eq!(body.project_id.as_deref(), Some("p1"));
        assert_eq!(body.repo, None);
        assert_eq!(body.pr, 3);
        assert_eq!(body.kind, crate::model::ReviewKind::Check);
        assert_eq!(body.request_id.as_str(), "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn parses_reusable_request_id_for_cross_process_recovery() {
        let a = parse_review(&[
            "prmonitor",
            "review",
            "--pr",
            "3",
            "--project-id",
            "p1",
            "--request-id",
            "0123456789abcdef0123456789abcdef",
        ])
        .expect("valid reusable request id");
        assert_eq!(
            a.request_id.as_ref().map(ExternalRequestId::as_str),
            Some("0123456789abcdef0123456789abcdef")
        );

        assert!(parse_review(&[
            "prmonitor",
            "review",
            "--pr",
            "3",
            "--project-id",
            "p1",
            "--request-id",
            "not-a-request-id",
        ])
        .is_err());
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
    fn parses_message_send_request_and_redacts_debug() {
        let args = parse_message(&[
            "prmonitor",
            "message",
            "send",
            "--integration-id",
            "wx",
            "--conversation-id",
            "c1",
            "--text",
            "secret message",
            "--token",
            "local-token",
        ])
        .expect("valid");
        let MessageCommand::Send(send) = args.command else {
            panic!("expected send");
        };
        let request = send.send_request();
        assert_eq!(request.integration_id, "wx");
        assert_eq!(request.conversation_id, "c1");
        assert_eq!(
            request.content,
            MessagingSendContent::Text {
                text: "secret message".to_string()
            }
        );
        assert_eq!(request.request_id.len(), 32);
        assert!(request
            .request_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        let debug = format!("{send:?}");
        assert!(!debug.contains("secret message"));
        assert!(!debug.contains("local-token"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn parses_message_send_card_contract_and_redacts_debug() {
        let args = parse_message(&[
            "prmonitor",
            "message",
            "send-card",
            "--integration-id",
            "fs",
            "--conversation-id",
            "oc_123",
            "--title",
            "Task stopped",
            "--text",
            "private markdown body",
            "--template",
            "orange",
            "--json",
            "--token",
            "local-token",
        ])
        .expect("valid");
        let MessageCommand::SendCard(send) = args.command else {
            panic!("expected send-card");
        };
        let request = send.send_request();
        assert_eq!(
            request.content,
            MessagingSendContent::Card {
                title: "Task stopped".to_string(),
                text: "private markdown body".to_string(),
                template: MessagingCardTemplate::Orange,
            }
        );
        assert_eq!(send.json.as_deref(), Some(""));
        let debug = format!("{send:?}");
        assert!(!debug.contains("Task stopped"));
        assert!(!debug.contains("private markdown body"));
        assert!(!debug.contains("local-token"));
        assert!(debug.contains("[REDACTED]"));

        assert!(parse_message(&[
            "prmonitor",
            "message",
            "send-card",
            "--integration-id",
            "fs",
            "--conversation-id",
            "oc_123",
            "--title",
            "title",
            "--text",
            "body",
            "--template",
            "red",
        ])
        .is_err());
    }

    #[test]
    fn generated_request_ids_match_external_contract_and_do_not_repeat() {
        let first = new_request_id();
        let second = new_request_id();
        assert_ne!(first, second);
        for id in [first, second] {
            assert_eq!(id.as_str().len(), 32);
            assert!(id
                .as_str()
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        }
    }

    #[tokio::test]
    async fn ambiguous_review_response_retries_with_the_same_request_id() {
        use axum::body::Bytes;
        use axum::extract::State;
        use axum::http::{header, StatusCode};
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct Seen {
            bodies: Mutex<Vec<serde_json::Value>>,
        }

        async fn reviews(State(seen): State<Arc<Seen>>, body: Bytes) -> Response {
            let value: serde_json::Value = serde_json::from_slice(&body).expect("request json");
            let attempt = {
                let mut bodies = seen.bodies.lock().expect("bodies");
                bodies.push(value);
                bodies.len()
            };
            if attempt == 1 {
                return (
                    StatusCode::ACCEPTED,
                    [(header::CONTENT_TYPE, "application/json")],
                    "not-json",
                )
                    .into_response();
            }
            (
                StatusCode::ACCEPTED,
                [(header::CONTENT_TYPE, "application/json")],
                r#"{"receiptId":17,"statusUrl":"http://127.0.0.1:8788/reviews/17"}"#,
            )
                .into_response()
        }

        let seen = Arc::new(Seen::default());
        let app = Router::new()
            .route("/reviews", post(reviews))
            .with_state(Arc::clone(&seen));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });

        let args =
            parse_review(&["prmonitor", "review", "--pr", "7", "--repo", "o/r"]).expect("args");
        let request_id =
            ExternalRequestId::parse("0123456789abcdef0123456789abcdef").expect("request id");
        let client = reqwest::Client::new();
        let result = submit_review_request(
            &client,
            &format!("http://{addr}"),
            "token",
            &args,
            &request_id,
        )
        .await;
        assert!(matches!(
            result,
            ReviewRequestAttempt::Accepted(ReviewReceiptAccepted { receipt_id, .. })
                if receipt_id.get() == 17
        ));
        let bodies = seen.bodies.lock().expect("bodies");
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["requestId"], request_id.as_str());
        assert_eq!(bodies[1]["requestId"], request_id.as_str());
        server.abort();
    }

    #[test]
    fn message_log_args_redact_token_in_debug() {
        let args = parse_message(&[
            "prmonitor",
            "message",
            "events",
            "--integration-id",
            "wx",
            "--token",
            "local-token",
        ])
        .expect("valid");
        let debug = format!("{args:?}");
        assert!(!debug.contains("local-token"));
        assert!(debug.contains("[REDACTED]"));
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
        // external ingress constructor, so the CLI does not duplicate that rule. A 0 reaches the
        // server, which rejects it with a 400 → non-zero exit.
        let a =
            parse_review(&["prmonitor", "review", "--pr", "0", "--repo", "o/n"]).expect("valid");
        assert_eq!(a.pr, 0);
    }

    #[test]
    fn is_terminal_only_done_and_failed() {
        assert!(is_terminal(ReviewReceiptStatus::Done));
        assert!(is_terminal(ReviewReceiptStatus::Failed));
        assert!(!is_terminal(ReviewReceiptStatus::Received));
        assert!(!is_terminal(ReviewReceiptStatus::Queued));
        assert!(!is_terminal(ReviewReceiptStatus::Blocked));
        assert!(!is_terminal(ReviewReceiptStatus::Starting));
        assert!(!is_terminal(ReviewReceiptStatus::Running));
        assert!(!is_terminal(ReviewReceiptStatus::Interrupting));
    }

    #[test]
    fn human_failed_status_includes_receipt_error() {
        assert_eq!(
            human_status(
                ReviewReceiptStatus::Failed,
                None,
                Some("executor unavailable")
            ),
            "✗ review 失败：executor unavailable"
        );
        assert_eq!(
            human_status(ReviewReceiptStatus::Failed, None, None),
            "✗ review 失败"
        );
    }

    #[test]
    fn exit_code_follows_gh_run_watch() {
        // Without --exit-status: always 0, even on failure.
        assert_eq!(exit_code(ReviewReceiptStatus::Failed, false, false), 0);
        assert_eq!(exit_code(ReviewReceiptStatus::Done, false, false), 0);
        // With --exit-status: 0 only for a completed review (Done + URL).
        assert_eq!(exit_code(ReviewReceiptStatus::Done, true, true), 0);
        // Done without a URL = interrupted → non-zero.
        assert_eq!(exit_code(ReviewReceiptStatus::Done, false, true), 1);
        assert_eq!(exit_code(ReviewReceiptStatus::Failed, false, true), 1);
        // Failed is non-zero even if a URL is somehow present (only `Done` can succeed).
        assert_eq!(exit_code(ReviewReceiptStatus::Failed, true, true), 1);
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
    fn parses_notify_request_and_repeated_channels() {
        let a = parse_notify(&[
            "prmonitor",
            "notify",
            "--title",
            "Deploy done",
            "--body",
            "Build 42 finished",
            "--url",
            "https://example.com/build/42",
            "--level",
            "warning",
            "--project-id",
            "p1",
            "--channel",
            "desktop",
            "--channel",
            "slack-main",
            "--json",
        ])
        .expect("valid");
        let body = a.notification_request();
        assert_eq!(body.title, "Deploy done");
        assert_eq!(body.body.as_deref(), Some("Build 42 finished"));
        assert_eq!(body.url.as_deref(), Some("https://example.com/build/42"));
        assert_eq!(body.level, Some(NotificationLevel::Warning));
        assert_eq!(body.project_id.as_deref(), Some("p1"));
        assert_eq!(body.channel_ids, vec!["desktop", "slack-main"]);
        assert_eq!(a.json.as_deref(), Some(""));
    }

    #[test]
    fn notify_rejects_unknown_level_and_redacts_debug() {
        assert!(parse_notify(&[
            "prmonitor",
            "notify",
            "--title",
            "Deploy done",
            "--level",
            "critical",
        ])
        .is_err());
        let a = parse_notify(&[
            "prmonitor",
            "notify",
            "--title",
            "Deploy done",
            "--body",
            "private body",
            "--token",
            "supersecret",
        ])
        .expect("valid");
        let dbg = format!("{a:?}");
        assert!(
            !dbg.contains("private body"),
            "body must be redacted: {dbg}"
        );
        assert!(
            !dbg.contains("supersecret"),
            "token must be redacted: {dbg}"
        );
        assert!(
            dbg.contains("[REDACTED]"),
            "redaction marker present: {dbg}"
        );
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
