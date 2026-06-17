//! Webhook PR-trigger source (#9): a local `POST /webhook` receiver exposed to the
//! public internet so GitHub pushes a PR event the instant a trigger label lands
//! instead of waiting for the poll interval.
//!
//! **Tunnel is decoupled from the receiver.** The receiver only ever binds
//! `127.0.0.1`, verifies the HMAC, parses, and dispatches; HOW the local port reaches
//! the public internet is a config choice ([`WebhookTunnelMode`]) the manager branches
//! on in [`WebhookManager::start`]:
//! - `quick` (default, unchanged): spawn a Cloudflare Quick Tunnel and scrape the
//!   `*.trycloudflare.com` URL.
//! - `command`: spawn a user-supplied tunnel command (`{port}` placeholder, exec'd
//!   directly — never via a shell); the public URL comes from config, not scraped.
//! - `listener`: bind only, spawn NO child; the tunnel is fully external; the public
//!   URL comes from config.
//!
//! **Push, not pull — so it does NOT implement [`super::source::PrSource`].** That
//! trait's `discover()` is pull-shaped (the scheduler asks `gh` for the current
//! list); a webhook is push-shaped (GitHub hands us one event). Per the
//! [`crate::dispatch`] doc, "a future webhook trigger calls the same `auto_dispatch`
//! with the candidates a push event yields" — that is exactly this module: the
//! handler maps a payload to a [`Candidate`] and hands it to the injected
//! [`Dispatcher`] (the composition root's gate + `auto_dispatch` closure), reusing
//! the entire vetted dispatch path with zero duplication.
//!
//! **Layering.** The axum handler is runtime-agnostic — it never names
//! `AppHandle<R>`. The autoReview gate and the static/cooldown gates (parity with
//! the poll path) live in the [`Dispatcher`] closure the root installs via
//! [`WebhookManager::set_dispatcher`] (which holds the concrete app handle), exactly
//! as [`super::scheduler::Scheduler`] does. The handler's only job is verify →
//! parse → map → hand off.
//!
//! **Security.** The endpoint is public (via the tunnel), so every request is
//! HMAC-verified (`X-Hub-Signature-256`) against the configured secret before the
//! body is even parsed; an unverified or secret-less request is rejected. The local
//! server binds `127.0.0.1` only — the raw port is never world-reachable, only the
//! cloudflared tunnel is. The app registers NO webhook in GitHub (that would be a
//! GitHub write, breaking the app's read-only `gh` surface) — the user pastes the
//! tunnel URL + secret into the repo's webhook settings by hand.

use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::Value;
use sha2::Sha256;
use tauri::async_runtime::{spawn, JoinHandle};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, Command};

use super::scheduler::Dispatcher;
use crate::error::{AppError, AppResult};
use crate::model::{Candidate, WebhookTunnelMode};

type HmacSha256 = Hmac<Sha256>;

/// cloudflared prints the assigned Quick Tunnel URL to stderr within a few seconds;
/// cap the wait so a stuck binary can't hang `start_webhook`.
const TUNNEL_URL_TIMEOUT: Duration = Duration::from_secs(20);
/// `cloudflared --version` probe budget (the install check).
const CLOUDFLARED_VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// The single path the axum receiver serves — also the suffix appended to the tunnel
/// root to form the GitHub "Payload URL". ONE source for both the route registration
/// and the URL the UI tells the user to paste; they must match or every delivery 404s.
const WEBHOOK_PATH: &str = "/webhook";

/// webhook receiver + tunnel status reported to the frontend (pr-slice-private wire
/// type; not a cross-slice contract, so it is mirrored in `src/pr/types.ts`, not
/// `model.rs` / `src/types.ts` — same placement as [`super::gh::GhStatus`]).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookStatus {
    /// Whether the local receiver + tunnel are currently running.
    pub running: bool,
    /// The public `https://*.trycloudflare.com` tunnel ROOT, when resolved. `None`
    /// while stopped or if the URL didn't appear within the timeout. This is the
    /// tunnel itself, NOT the value to paste into GitHub — see [`Self::payload_url`].
    pub public_url: Option<String>,
    /// The full GitHub "Payload URL" = [`Self::public_url`] + [`WEBHOOK_PATH`], the
    /// ONLY route the receiver serves. The UI shows/copies THIS (pasting `public_url`
    /// alone 404s every delivery). Derived in [`Self::new`] so it can't drift.
    pub payload_url: Option<String>,
    /// Whether `cloudflared` is runnable (so the UI can prompt to install it).
    pub cloudflared_installed: bool,
    /// Human-readable (Chinese) status line for the UI.
    pub message: String,
}

impl WebhookStatus {
    /// Build a status, deriving [`Self::payload_url`] from `public_url` + the route
    /// the receiver serves ([`WEBHOOK_PATH`]) as the SINGLE source — a route rename
    /// forces this format to follow (locked by `webhook_status_wire_shape_*`), so the
    /// pasted URL and the served path can never disagree. The ONLY constructor, so no
    /// caller can build a status whose `payload_url` drifts from `public_url`.
    fn new(
        running: bool,
        public_url: Option<String>,
        cloudflared_installed: bool,
        message: String,
    ) -> Self {
        let payload_url = public_url.as_ref().map(|u| format!("{u}{WEBHOOK_PATH}"));
        Self {
            running,
            public_url,
            payload_url,
            cloudflared_installed,
            message,
        }
    }
}

/// How the receiver's local port reaches the public internet — the tunnel half of a
/// [`WebhookManager::start`] call, grouped so the receiver params (port / secret /
/// labels / cloudflared_bin) and the tunnel params don't blur into one flat arg list.
/// All three fields come straight off `AppConfig` (`webhook_tunnel_mode` /
/// `webhook_tunnel_command` / `webhook_public_url`); `start` branches on `mode`:
/// `command` reads `command`, `command`/`listener` read `public_url` (`quick` ignores
/// both and scrapes the URL from cloudflared).
pub struct TunnelSpec {
    pub mode: WebhookTunnelMode,
    /// The `command`-mode tunnel command (`{port}` placeholder; whitespace-split; exec'd
    /// directly, no shell). Ignored by `quick` / `listener`.
    pub command: String,
    /// The `command`/`listener`-mode public URL root (empty → `None`). Ignored by `quick`
    /// (which scrapes the `*.trycloudflare.com` URL instead).
    pub public_url: String,
}

/// Owns the running receiver + tunnel. `&self` methods + interior mutability so it
/// lives in `AppState` (which stays `Default`), mirroring `Scheduler`/`CodexManager`.
#[derive(Default)]
pub struct WebhookManager {
    /// Installed once by the composition root (lib.rs) BEFORE any start, like
    /// [`super::scheduler::Scheduler::set_dispatcher`]. The closure applies the
    /// autoReview + static/cooldown gates and runs `auto_dispatch`, keeping the axum
    /// handler runtime-agnostic.
    dispatcher: StdMutex<Option<Dispatcher>>,
    runtime: StdMutex<Option<WebhookRuntime>>,
    /// Serializes `start` (bind + spawn + tunnel-URL await) so concurrent starts
    /// can't double-bind the port.
    start_lock: tokio::sync::Mutex<()>,
}

/// The live receiver + (optional) tunnel handles. [`Self::teardown`] aborts
/// `server_task` and (if present) `drain_task` (explicit — neither is aborted by
/// `Drop`) before dropping this value; `tunnel` (the tunnel child) is then killed via
/// `kill_on_drop(true)` on drop. `status` reaps a dead `tunnel` via `try_wait` to
/// self-heal a crashed tunnel.
///
/// `tunnel` / `drain_task` are `Option` because the `listener` mode spawns NO child
/// process (the tunnel is fully external) — both are `None` there, and `teardown` /
/// `status` treat `None` as "nothing to abort / always-running" (no crash self-heal
/// applies when there's no child to crash).
struct WebhookRuntime {
    server_task: JoinHandle<()>,
    /// stderr-drain task for the tunnel child (keeps the pipe from filling). Aborted in
    /// `teardown` for lifecycle symmetry rather than relying on the child-kill →
    /// pipe-EOF chain to end it. `None` in `listener` mode (no child to drain).
    drain_task: Option<JoinHandle<()>>,
    /// The tunnel child (cloudflared in `quick` mode, the user command in `command`
    /// mode). Kept to keep the tunnel alive (and `kill_on_drop` it on drop), and probed
    /// by `status` via `try_wait` to detect a crashed tunnel. `None` in `listener` mode
    /// (tunnel external — no child).
    tunnel: Option<Child>,
    public_url: Option<String>,
}

impl WebhookRuntime {
    /// Abort the server task + (if any) drain task; the tunnel child is then killed on
    /// drop (`kill_on_drop`) — or, on the `status` self-heal path, has already exited.
    /// A `None` `tunnel` (listener mode) drops as a no-op. The single teardown shared by
    /// `stop_inner` and `status`'s crash self-heal.
    fn teardown(self) {
        self.server_task.abort();
        if let Some(drain) = self.drain_task {
            drain.abort();
        }
        // self.tunnel dropped here → kill_on_drop kills the child (no-op if already
        // exited, or if None in listener mode).
    }
}

impl WebhookManager {
    /// Install the dispatch hook (composition root, before any start).
    pub fn set_dispatcher(&self, d: Dispatcher) {
        *self.dispatcher.lock().unwrap() = Some(d);
    }

    /// Start (or restart) the local receiver + (per-`mode`) tunnel. Tears down any prior
    /// runtime first (so a config change re-binds cleanly). Returns the resolved status.
    ///
    /// Branches on [`WebhookTunnelMode`]:
    /// - `Quick` (unchanged): require `cloudflared` (a missing binary short-circuits to
    ///   `running: false` + `cloudflared_installed: false` rather than an error, so the
    ///   UI can prompt to install it), bind, spawn the Quick Tunnel, scrape the
    ///   `*.trycloudflare.com` URL.
    /// - `Command`: bind, spawn `tunnel_command` (split on whitespace, `{port}` →
    ///   actual port, exec'd directly — no shell), `public_url` from config (`None` if
    ///   `public_url` is empty). Does NOT require cloudflared.
    /// - `Listener`: bind only, spawn no child; `public_url` from config. Does NOT
    ///   require cloudflared.
    pub async fn start(
        &self,
        port: u16,
        secret: String,
        review_label: String,
        check_label: String,
        cloudflared_bin: String,
        tunnel: TunnelSpec,
    ) -> AppResult<WebhookStatus> {
        let TunnelSpec {
            mode,
            command: tunnel_command,
            public_url,
        } = tunnel;
        let _guard = self.start_lock.lock().await;
        self.stop_inner(); // restart semantics — avoid double-bind.

        // Only the `quick` mode owns/depends on cloudflared; check it up front there so a
        // missing binary short-circuits BEFORE we bind. `command` / `listener` never
        // touch cloudflared (their tunnel is the user command / fully external).
        if mode == WebhookTunnelMode::Quick && !cloudflared_installed(&cloudflared_bin).await {
            return Ok(WebhookStatus::new(
                false,
                None,
                false,
                "未找到 cloudflared，请先安装：brew install cloudflared".to_string(),
            ));
        }

        let dispatcher = self
            .dispatcher
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| AppError::new("webhook dispatcher 未初始化".to_string()))?;

        // LOCAL bind only — the public path is the tunnel; the raw port is never
        // world-reachable. Shared by all three modes.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| AppError::new(format!("webhookPort 监听失败（端口 {port}）：{e}")))?;

        let ctx = Arc::new(WebhookCtx {
            secret,
            review_label,
            check_label,
            dispatcher,
        });
        let router = Router::new()
            .route(WEBHOOK_PATH, post(handle_webhook))
            // Cap the public endpoint's request body. GitHub webhook payloads are well
            // under this (typically < 25 KiB); the limit bounds the memory a forged POST
            // can make us buffer before the HMAC check rejects it.
            .layer(DefaultBodyLimit::max(1024 * 1024))
            .with_state(ctx);
        let server_task = spawn(async move {
            let _ = axum::serve(listener, router.into_make_service()).await;
        });

        // Configured public URL for the non-scraping modes (empty → None, so the status
        // honestly reports "no URL yet" rather than a blank string).
        let configured_url = (!public_url.trim().is_empty()).then(|| public_url.trim().to_string());

        // Resolve the tunnel per mode. On a spawn error the server task (already holding
        // the bound port) MUST be aborted here — otherwise it detaches, leaks the port,
        // and `self.runtime` stays `None` so a later `stop_inner` can't reap it.
        let (tunnel, drain_task, resolved_url): (
            Option<Child>,
            Option<JoinHandle<()>>,
            Option<String>,
        ) = match mode {
            WebhookTunnelMode::Quick => match spawn_quick_tunnel(&cloudflared_bin, port).await {
                Ok((child, drain, url)) => (Some(child), Some(drain), url),
                Err(e) => {
                    server_task.abort();
                    return Err(e);
                }
            },
            WebhookTunnelMode::Command => match spawn_custom_tunnel(&tunnel_command, port) {
                Ok((child, drain)) => (Some(child), Some(drain), configured_url),
                Err(e) => {
                    server_task.abort();
                    return Err(e);
                }
            },
            // No child: bind-only. The tunnel is external; the URL is whatever the
            // user configured.
            WebhookTunnelMode::Listener => (None, None, configured_url),
        };

        *self.runtime.lock().unwrap() = Some(WebhookRuntime {
            server_task,
            drain_task,
            tunnel,
            public_url: resolved_url.clone(),
        });

        let message = match (mode, &resolved_url) {
            (WebhookTunnelMode::Quick, Some(u)) => format!("已启动，公网 URL：{u}"),
            (WebhookTunnelMode::Quick, None) => {
                "隧道已启动，但未能在超时内解析公网 URL（请查看 cloudflared 日志）".to_string()
            }
            (WebhookTunnelMode::Command, Some(u)) => format!("已启动自定义隧道，公网 URL：{u}"),
            (WebhookTunnelMode::Command, None) => {
                "已启动自定义隧道（未配置 webhookPublicUrl，无法显示公网 URL）".to_string()
            }
            (WebhookTunnelMode::Listener, Some(u)) => format!("接收端已监听，公网 URL：{u}"),
            (WebhookTunnelMode::Listener, None) => {
                "接收端已监听（隧道外置，未配置 webhookPublicUrl）".to_string()
            }
        };
        // `cloudflared_installed` is always `true` on this success path: `quick` mode
        // only reaches here past the install short-circuit above, and the non-quick
        // modes don't use cloudflared (reporting `true` keeps the UI's "install
        // cloudflared" prompt from firing spuriously for a mode that doesn't need it).
        Ok(WebhookStatus::new(true, resolved_url, true, message))
    }

    /// Tear down the running receiver + tunnel (abort the server task, drop/kill the
    /// child). Sync + idempotent — safe from the app-exit `RunEvent` handler.
    fn stop_inner(&self) {
        if let Some(rt) = self.runtime.lock().unwrap().take() {
            rt.teardown();
        }
    }

    /// Explicit stop (the `stop_webhook` command).
    pub fn stop(&self) {
        self.stop_inner();
    }

    /// App-shutdown cleanup (wired to `RunEvent::Exit` in `lib.rs`, like
    /// `CodexManager::shutdown`) so the cloudflared child never outlives the app.
    pub fn shutdown(&self) {
        self.stop_inner();
    }

    /// Current status (running + URL) plus a fresh `cloudflared` install probe.
    ///
    /// Self-heals a crashed tunnel: a `runtime: Some` whose tunnel child has exited is a
    /// DEAD tunnel that would otherwise still report `running: true`. We probe the child
    /// with `try_wait` (non-blocking — no await held across the `StdMutex`), and on an
    /// exited child take the runtime + tear it down (mirroring `stop_inner`) and report
    /// not-running. Mirrors codex's "drop a dead resident process" self-heal
    /// (`engines/codex/process.rs::kill_and_reap`).
    ///
    /// `listener` mode has NO tunnel child (`tunnel: None`), so there is nothing to
    /// crash and nothing to self-heal — it stays `running` until an explicit stop.
    pub async fn status(&self, cloudflared_bin: &str) -> WebhookStatus {
        let mut crashed = false;
        let (running, public_url) = {
            let mut guard = self.runtime.lock().unwrap();
            // Probe liveness + snapshot the URL in one borrow, then release it so the
            // self-heal `take()` below can re-borrow the guard mutably. A `None` tunnel
            // (listener mode) has no child to probe → `None` try_wait result = "alive".
            let probe = guard.as_mut().map(|rt| {
                let wait = rt.tunnel.as_mut().map(Child::try_wait);
                (wait, rt.public_url.clone())
            });
            match probe {
                // Tunnel child exited → dead tunnel: take + teardown, report not-running.
                Some((Some(Ok(Some(_exit))), _)) => {
                    crashed = true;
                    if let Some(dead) = guard.take() {
                        dead.teardown();
                    }
                    (false, None)
                }
                // Alive: a live child (`Some(Ok(None))`), a transient wait Err
                // (`Some(Err(_))` — best-effort: stay running, the next probe retries),
                // or no child at all (`None`, listener mode). Never tear down a healthy
                // (or childless) tunnel.
                Some((_, url)) => (true, url),
                None => (false, None),
            }
        };
        let installed = cloudflared_installed(cloudflared_bin).await;
        let message = if running {
            match &public_url {
                Some(u) => format!("运行中，公网 URL：{u}"),
                None => "运行中（公网 URL 尚未解析）".to_string(),
            }
        } else if crashed {
            "隧道已退出（cloudflared 进程已结束），请重新启动 Webhook".to_string()
        } else if installed {
            "未运行".to_string()
        } else {
            "未运行；未检测到 cloudflared（brew install cloudflared）".to_string()
        };
        WebhookStatus::new(running, public_url, installed, message)
    }
}

/// Shared, runtime-agnostic state for the axum handler.
struct WebhookCtx {
    secret: String,
    review_label: String,
    check_label: String,
    dispatcher: Dispatcher,
}

/// `POST /webhook`. Verify the GitHub HMAC, map a `pull_request` payload to a
/// [`Candidate`], and hand it to the dispatcher off the request path (so GitHub gets
/// a fast 2xx). Non-`pull_request` events (e.g. the `ping` GitHub sends on setup)
/// are acknowledged without acting.
async fn handle_webhook(
    State(ctx): State<Arc<WebhookCtx>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    // A missing header or a non-UTF-8 value both collapse to "" — equivalent to an
    // absent signature, which `verify_signature`'s `strip_prefix("sha256=")` gate then
    // rejects (fail-closed). The public endpoint never acts on an unverified request.
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !verify_signature(&ctx.secret, &body, signature) {
        return StatusCode::UNAUTHORIZED;
    }

    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if event != "pull_request" {
        return StatusCode::OK;
    }

    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST,
    };

    if let Some(candidate) = payload_to_candidate(&payload, &ctx.review_label, &ctx.check_label) {
        // Detached: the dispatcher future is `Send + 'static`; the gates + review
        // start run independently of this response.
        let dispatcher = ctx.dispatcher.clone();
        drop(spawn(dispatcher(vec![candidate])));
    }
    StatusCode::OK
}

/// Constant-time verify of a GitHub `X-Hub-Signature-256` header (`sha256=<hex>`)
/// against `HMAC-SHA256(secret, body)`. An empty secret, a malformed header, or a
/// mismatch all fail closed (the public endpoint must never accept an unsigned POST).
/// Pure — unit-tested without a server.
fn verify_signature(secret: &str, body: &[u8], header: &str) -> bool {
    if secret.is_empty() {
        return false;
    }
    let Some(hex_digest) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_digest) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// Map a GitHub `pull_request` webhook payload to a dispatchable [`Candidate`], or
/// `None` when it carries no single trigger label. Conflict (BOTH trigger labels)
/// drops here, mirroring the poll path's discovery-stage conflict skip; the
/// remaining gates (cross-repo / draft / author / cooldown) are applied downstream
/// by the dispatcher closure via [`super::discover::should_skip`] /
/// [`super::discover::cooldown_skip`], so this stays a pure parse+map (the
/// `is_draft` / `is_cross_repository` flags it extracts are what those gates read).
/// Pure — unit-tested without a server.
fn payload_to_candidate(
    payload: &Value,
    review_label: &str,
    check_label: &str,
) -> Option<Candidate> {
    let pr = payload.get("pull_request")?;

    // Parity with the poll path's `--state open` (`gh.rs`): only an OPEN PR is a
    // dispatch candidate. A closed/merged PR still carrying a trigger label (a
    // `closed` delivery, or a label touched post-merge) must NOT start a review — the
    // poll path never lists closed PRs; this is the push-path equivalent. Missing /
    // non-`"open"` state fails safe to no candidate.
    if pr.get("state").and_then(Value::as_str) != Some("open") {
        return None;
    }

    let label_names: Vec<&str> = pr
        .get("labels")?
        .as_array()?
        .iter()
        .filter_map(|l| l.get("name").and_then(Value::as_str))
        .collect();
    let has_review = label_names.contains(&review_label);
    let has_check = label_names.contains(&check_label);
    let kind = match (has_review, has_check) {
        (true, true) => return None, // conflict — both trigger labels (poll path skips too)
        (true, false) => "review",
        (false, true) => "check",
        (false, false) => return None,
    };

    let number = pr.get("number")?.as_u64()?;
    let head = pr.get("head")?;
    let head_sha = head.get("sha")?.as_str()?.to_string();
    let head_ref = head.get("ref")?.as_str()?.to_string();
    let author = pr
        .get("user")
        .and_then(|u| u.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let is_draft = pr.get("draft").and_then(Value::as_bool).unwrap_or(false);

    // Fork PR ⇒ head repo differs from base repo. Missing repo info (e.g. a deleted
    // fork) is treated as cross-repo (fail safe): `should_skip` then drops it, so the
    // app never runs codex against untrusted fork code it can't attribute.
    let full_name = |repo: Option<&Value>| {
        repo.and_then(|r| r.get("full_name"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let head_repo = full_name(head.get("repo"));
    let base_repo = full_name(pr.get("base").and_then(|b| b.get("repo")));
    let is_cross_repository = match (head_repo, base_repo) {
        (Some(h), Some(b)) => h != b,
        _ => true,
    };

    Some(Candidate {
        number,
        head_sha,
        head_ref,
        author,
        is_cross_repository,
        is_draft,
        kind: kind.to_string(),
    })
}

/// Probe whether `cloudflared` is runnable (`cloudflared --version`). Never errors;
/// any failure (missing binary, non-zero exit, timeout) → false. Read-only.
async fn cloudflared_installed(bin: &str) -> bool {
    let mut cmd = Command::new(bin);
    cmd.arg("--version").kill_on_drop(true);
    matches!(
        tokio::time::timeout(CLOUDFLARED_VERSION_TIMEOUT, cmd.output()).await,
        Ok(Ok(out)) if out.status.success()
    )
}

/// Spawn a Cloudflare Quick Tunnel for `http://127.0.0.1:<port>` and capture the
/// assigned `https://*.trycloudflare.com` URL from cloudflared's stderr. The child
/// is `kill_on_drop(true)`, so the manager's runtime drop kills the tunnel. Returns
/// `(child, drain_task, Some(url))`, or `…None` if the URL didn't appear within the
/// timeout (the tunnel may still come up; the UI can re-query status). The caller
/// owns the returned `drain_task` and aborts it on stop (lifecycle symmetry).
///
/// `bin` is exec'd directly by `tokio::process::Command::new` — NOT via a shell — so
/// a `cloudflared_bin` path with spaces/special chars is treated as one program name,
/// never word-split or shell-interpreted (no command injection). `port` is a `u16`
/// formatted into a fixed arg, also unable to inject.
async fn spawn_quick_tunnel(
    bin: &str,
    port: u16,
) -> AppResult<(Child, JoinHandle<()>, Option<String>)> {
    let mut cmd = Command::new(bin);
    cmd.args([
        "tunnel",
        "--no-autoupdate",
        "--url",
        &format!("http://127.0.0.1:{port}"),
    ])
    // cloudflared logs (incl. the URL banner) go to stderr; stdout stays unused →
    // null it so an unread pipe can't ever block the child.
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::new(format!("无法启动 cloudflared：{e}")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::new("cloudflared stderr 不可用".to_string()))?;
    let mut lines = BufReader::new(stderr).lines();

    let url = tokio::time::timeout(TUNNEL_URL_TIMEOUT, scan_for_url(&mut lines))
        .await
        .ok()
        .flatten();

    // Distinguish "URL still coming up" from "child already dead". The timeout
    // collapses both into `None`, but a child that EXITED before printing a URL is a
    // failed tunnel — fail fast rather than hand back a dead child the manager would
    // report as `running`. `try_wait` is non-blocking; a live-but-slow child stays
    // `Ok((.., None))` (the tunnel may still resolve, and `status` re-probes liveness).
    if url.is_none() {
        if let Ok(Some(exit)) = child.try_wait() {
            return Err(AppError::new(format!(
                "cloudflared 在解析公网 URL 前已退出（{exit}）；请检查 cloudflared 日志"
            )));
        }
    }

    // Keep draining stderr for the child's lifetime so a full pipe can't stall
    // cloudflared after we stop scanning (mirrors codex's stderr drain). The handle
    // is returned so `stop_inner` can abort it; absent that, the child-kill → pipe-EOF
    // chain would still end it, but we prefer explicit teardown.
    let drain_task = spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

    Ok((child, drain_task, url))
}

/// Tokenize a user-supplied tunnel command into `(program, args)`, substituting the
/// literal `{port}` placeholder in EACH token with the actual listen `port`.
///
/// Splits on ASCII whitespace (so quoting / shell metacharacters carry NO meaning):
/// the result is exec'd directly via [`Command::new`] in [`spawn_custom_tunnel`], never
/// handed to a shell, so a token can't word-split further or inject (`command` mode
/// keeps the same anti-injection property `spawn_quick_tunnel` has for `bin`). Returns
/// `None` when the command is blank (no program token) — the caller turns that into an
/// `AppError` (defense in depth; `validate` rejects an empty command for this mode
/// upstream). Pure — unit-tested.
fn build_tunnel_command_argv(command: &str, port: u16) -> Option<(String, Vec<String>)> {
    let port = port.to_string();
    let mut tokens = command
        .split_whitespace()
        .map(|tok| tok.replace("{port}", &port));
    let program = tokens.next()?;
    let args: Vec<String> = tokens.collect();
    Some((program, args))
}

/// Spawn a user-supplied tunnel command (`command` mode) bridging the public internet
/// to `http://127.0.0.1:<port>`. Mirrors [`spawn_quick_tunnel`]'s child handling
/// (`kill_on_drop(true)` so the runtime drop kills it; stdout nulled + stderr drained so
/// a full pipe can't stall the child), but does NOT scan for a URL — the public URL in
/// `command` mode comes from config, not the child's output. Returns `(child,
/// drain_task)`; the caller owns the drain task and aborts it on stop.
///
/// The command is tokenized by [`build_tunnel_command_argv`] (`{port}` substituted) and
/// exec'd DIRECTLY via [`Command::new`] — NOT via a shell — so no token is word-split
/// or shell-interpreted (same anti-injection property as `spawn_quick_tunnel`'s `bin`).
/// A blank command is an `AppError` (defense in depth; `validate` already rejects it
/// upstream for this mode).
fn spawn_custom_tunnel(command: &str, port: u16) -> AppResult<(Child, JoinHandle<()>)> {
    let (program, args) = build_tunnel_command_argv(command, port).ok_or_else(|| {
        AppError::new("webhookTunnelCommand 不能为空（command 模式需填隧道命令）".to_string())
    })?;

    let mut cmd = Command::new(&program);
    cmd.args(&args)
        // stdout unused → null it; stderr piped + drained so an unread pipe can't block.
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::new(format!("无法启动自定义隧道命令（{program}）：{e}")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::new("自定义隧道命令 stderr 不可用".to_string()))?;
    let mut lines = BufReader::new(stderr).lines();
    let drain_task = spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

    Ok((child, drain_task))
}

/// Read cloudflared's stderr line-by-line until a trycloudflare URL appears (or EOF).
async fn scan_for_url(lines: &mut Lines<BufReader<ChildStderr>>) -> Option<String> {
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(url) = extract_trycloudflare_url(&line) {
            return Some(url);
        }
    }
    None
}

/// Extract a `https://*.trycloudflare.com` URL from one cloudflared log line (it
/// prints the URL inside a box drawn with `|`). Pure — unit-tested.
fn extract_trycloudflare_url(line: &str) -> Option<String> {
    line.split(|c: char| c.is_whitespace() || c == '|')
        .map(str::trim)
        .find(|tok| tok.starts_with("https://") && tok.contains(".trycloudflare.com"))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn verify_signature_accepts_correct_hmac() {
        let body = br#"{"action":"labeled"}"#;
        assert!(verify_signature(
            "topsecret",
            body,
            &sign("topsecret", body)
        ));
    }

    #[test]
    fn verify_signature_rejects_tampered_body_wrong_secret_and_bad_header() {
        let body = br#"{"action":"labeled"}"#;
        let good = sign("topsecret", body);
        // Tampered body.
        assert!(!verify_signature(
            "topsecret",
            br#"{"action":"opened"}"#,
            &good
        ));
        // Wrong secret.
        assert!(!verify_signature("other", body, &good));
        // Missing `sha256=` prefix.
        assert!(!verify_signature("topsecret", body, "deadbeef"));
        // Non-hex digest.
        assert!(!verify_signature("topsecret", body, "sha256=zzzz"));
        // Empty secret fails closed even with a structurally valid header.
        assert!(!verify_signature("", body, &sign("", body)));
    }

    fn pr_payload(labels: &[&str], extra: serde_json::Value) -> Value {
        let label_objs: Vec<Value> = labels
            .iter()
            .map(|n| serde_json::json!({ "name": n }))
            .collect();
        let mut pr = serde_json::json!({
            "number": 42,
            "state": "open",
            "draft": false,
            "labels": label_objs,
            "user": { "login": "octocat" },
            "head": { "sha": "abc123", "ref": "feature", "repo": { "full_name": "owner/repo" } },
            "base": { "repo": { "full_name": "owner/repo" } },
        });
        if let (Value::Object(pr_map), Value::Object(extra_map)) = (&mut pr, extra) {
            for (k, v) in extra_map {
                pr_map.insert(k, v);
            }
        }
        serde_json::json!({ "action": "labeled", "pull_request": pr })
    }

    #[test]
    fn payload_to_candidate_maps_review_label() {
        let p = pr_payload(&["needs-review"], serde_json::json!({}));
        let c = payload_to_candidate(&p, "needs-review", "needs-check").expect("review candidate");
        assert_eq!(c.number, 42);
        assert_eq!(c.kind, "review");
        assert_eq!(c.head_sha, "abc123");
        assert_eq!(c.head_ref, "feature");
        assert_eq!(c.author, "octocat");
        assert!(!c.is_draft);
        assert!(!c.is_cross_repository);
    }

    #[test]
    fn payload_to_candidate_maps_check_label() {
        let p = pr_payload(&["needs-check"], serde_json::json!({}));
        let c = payload_to_candidate(&p, "needs-review", "needs-check").expect("check candidate");
        assert_eq!(c.kind, "check");
    }

    #[test]
    fn payload_to_candidate_skips_conflict_and_no_trigger_label() {
        // Both trigger labels → conflict → None (mirrors the poll path).
        let both = pr_payload(&["needs-review", "needs-check"], serde_json::json!({}));
        assert!(payload_to_candidate(&both, "needs-review", "needs-check").is_none());
        // No trigger label → None.
        let none = pr_payload(&["unrelated"], serde_json::json!({}));
        assert!(payload_to_candidate(&none, "needs-review", "needs-check").is_none());
    }

    #[test]
    fn payload_to_candidate_skips_non_open_pr() {
        // A closed/merged PR carrying a trigger label must NOT dispatch (parity with
        // the poll path's `--state open`). closed / merged / missing state → None.
        let closed = pr_payload(&["needs-review"], serde_json::json!({ "state": "closed" }));
        assert!(payload_to_candidate(&closed, "needs-review", "needs-check").is_none());
        let merged = pr_payload(&["needs-review"], serde_json::json!({ "state": "merged" }));
        assert!(payload_to_candidate(&merged, "needs-review", "needs-check").is_none());
        // Defensive: a payload with no `state` field fails safe to no candidate.
        let no_state = pr_payload(&["needs-review"], serde_json::json!({ "state": null }));
        assert!(payload_to_candidate(&no_state, "needs-review", "needs-check").is_none());
        // Sanity: the default helper payload IS open and still maps.
        let open = pr_payload(&["needs-review"], serde_json::json!({}));
        assert!(payload_to_candidate(&open, "needs-review", "needs-check").is_some());
    }

    #[test]
    fn payload_to_candidate_preserves_draft_and_fork_flags_for_downstream_gates() {
        // draft + fork flags are PRESERVED (not dropped here) — the dispatcher's
        // should_skip applies them. A draft fork PR still maps to a candidate; the
        // gate, not the parse, decides to skip it.
        let draft = pr_payload(&["needs-review"], serde_json::json!({ "draft": true }));
        assert!(
            payload_to_candidate(&draft, "needs-review", "needs-check")
                .unwrap()
                .is_draft
        );

        let fork = pr_payload(
            &["needs-review"],
            serde_json::json!({ "head": { "sha": "s", "ref": "r", "repo": { "full_name": "forker/repo" } } }),
        );
        assert!(
            payload_to_candidate(&fork, "needs-review", "needs-check")
                .unwrap()
                .is_cross_repository
        );
    }

    #[test]
    fn payload_to_candidate_treats_missing_repo_as_cross_repo() {
        // A deleted-fork head with no repo info → fail safe to cross-repo (skipped
        // downstream), never run codex against unattributable code.
        let p = pr_payload(
            &["needs-review"],
            serde_json::json!({ "head": { "sha": "s", "ref": "r", "repo": null } }),
        );
        assert!(
            payload_to_candidate(&p, "needs-review", "needs-check")
                .unwrap()
                .is_cross_repository
        );
    }

    #[test]
    fn payload_to_candidate_none_without_pull_request() {
        let p = serde_json::json!({ "action": "labeled" });
        assert!(payload_to_candidate(&p, "needs-review", "needs-check").is_none());
    }

    #[test]
    fn extract_trycloudflare_url_from_boxed_log_line() {
        let line = "2024-01-01T00:00:00Z INF |  https://random-words-here.trycloudflare.com  |";
        assert_eq!(
            extract_trycloudflare_url(line).as_deref(),
            Some("https://random-words-here.trycloudflare.com")
        );
        assert_eq!(extract_trycloudflare_url("INF Starting tunnel"), None);
    }

    // Wire-shape lock for `WebhookStatus` — the webhook commands' front/back wire
    // type, mirrored in `src/pr/types.ts` (Medium carrier per ai-robust.md; a field
    // rename would otherwise drift the TS mirror silently). Same pattern as
    // `gh_status_wire_shape_is_camel_case`.
    #[test]
    fn webhook_status_wire_shape_is_camel_case() {
        let v = serde_json::to_value(WebhookStatus::new(
            true,
            Some("https://x.trycloudflare.com".to_string()),
            true,
            "ok".to_string(),
        ))
        .expect("WebhookStatus serializes");
        assert!(v.get("running").is_some());
        assert!(v.get("publicUrl").is_some());
        assert!(v.get("cloudflaredInstalled").is_some());
        assert!(v.get("message").is_some());
        // payloadUrl is derived from publicUrl + the served route (WEBHOOK_PATH). A
        // route rename that forgets to follow surfaces here — the URL the UI tells the
        // user to paste must equal the path the receiver actually serves.
        assert_eq!(
            v.get("payloadUrl").and_then(Value::as_str),
            Some("https://x.trycloudflare.com/webhook")
        );
        // snake_case forms absent — a rename would surface here.
        assert!(v.get("public_url").is_none());
        assert!(v.get("payload_url").is_none());
        assert!(v.get("cloudflared_installed").is_none());
        // public_url None ⇒ payload_url None (no suffix on nothing).
        assert!(WebhookStatus::new(false, None, false, String::new())
            .payload_url
            .is_none());
    }

    /// F3 self-heal: a `runtime: Some` whose cloudflared child has exited must flip
    /// `status` to not-running (rather than the old "运行中（公网 URL 尚未解析）" for a
    /// dead tunnel). CI-safe — `true` exits 0 on darwin + Linux, no custom test bin.
    #[tokio::test]
    async fn status_self_heals_when_tunnel_child_exited() {
        let mut child = Command::new("true")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn `true`");
        let _ = child.wait().await; // ensure it has exited before we probe.

        let mgr = WebhookManager::default();
        *mgr.runtime.lock().unwrap() = Some(WebhookRuntime {
            server_task: spawn(async {}),
            drain_task: Some(spawn(async {})),
            tunnel: Some(child),
            public_url: Some("https://x.trycloudflare.com".to_string()),
        });

        // Bogus install bin → the probe returns false fast (no real cloudflared in CI);
        // the assertion is the self-heal flip, independent of install state.
        let s = mgr.status("prmonitor-no-such-cloudflared").await;
        assert!(!s.running, "a dead tunnel child must flip running → false");
        assert!(s.public_url.is_none());
        assert!(s.payload_url.is_none());
        assert!(
            s.message.contains("退出"),
            "message reports the crash: {}",
            s.message
        );
        // Runtime was taken (self-healed) → a second status is a clean not-running.
        assert!(mgr.runtime.lock().unwrap().is_none());
    }

    /// F3 fail-fast: a cloudflared that exits BEFORE printing a URL is a failed tunnel
    /// → `Err`, not `Ok((.., None))` (which the manager would report as running). CI-
    /// safe — `false` exits 1 immediately on darwin + Linux.
    #[tokio::test]
    async fn spawn_quick_tunnel_errs_when_child_exits_without_url() {
        let r = spawn_quick_tunnel("false", 0).await;
        let msg = r.expect_err("child exiting before a URL must Err").message;
        assert!(
            msg.contains("已退出"),
            "error reports the early exit: {msg}"
        );
    }

    /// Pure tokenization lock for `command` mode: split on whitespace, substitute every
    /// literal `{port}`, first token = program, rest = args. Exec'd directly (no shell),
    /// so this is the whole parse surface.
    #[test]
    fn build_tunnel_command_argv_splits_and_substitutes_port() {
        let (prog, args) = build_tunnel_command_argv(
            "cloudflared tunnel run --url http://127.0.0.1:{port} my-tunnel",
            8787,
        )
        .expect("non-empty command");
        assert_eq!(prog, "cloudflared");
        assert_eq!(
            args,
            vec![
                "tunnel".to_string(),
                "run".to_string(),
                "--url".to_string(),
                "http://127.0.0.1:8787".to_string(),
                "my-tunnel".to_string(),
            ]
        );

        // `{port}` substituted even when it is the whole token, and multiple
        // occurrences across tokens are all replaced.
        let (prog, args) = build_tunnel_command_argv("ngrok http {port} --log {port}", 9000)
            .expect("non-empty command");
        assert_eq!(prog, "ngrok");
        assert_eq!(
            args,
            vec![
                "http".to_string(),
                "9000".to_string(),
                "--log".to_string(),
                "9000".to_string(),
            ]
        );

        // Extra whitespace collapses (split_whitespace), and a port-only program token
        // still substitutes.
        let (prog, args) =
            build_tunnel_command_argv("  proxy-{port}   --to   {port}  ", 80).expect("non-empty");
        assert_eq!(prog, "proxy-80");
        assert_eq!(args, vec!["--to".to_string(), "80".to_string()]);

        // Blank command → None (the caller turns this into an AppError; validate also
        // rejects it upstream for command mode).
        assert!(build_tunnel_command_argv("", 8787).is_none());
        assert!(build_tunnel_command_argv("   ", 8787).is_none());
    }

    /// `command` mode `start` → `status`: spawns the user command (no URL scrape) and
    /// reports the CONFIGURED `public_url` (not a scraped one). CI-safe — `sleep` is a
    /// long-lived child on darwin + Linux, so the tunnel stays "alive" for the probe.
    #[tokio::test]
    async fn command_mode_start_reports_configured_public_url() {
        let mgr = WebhookManager::default();
        mgr.set_dispatcher(Arc::new(|_| Box::pin(async {})));

        // port 0 → OS picks a free port; `{port}` substitutes into the (harmless) sleep
        // args. cloudflared_bin is bogus on purpose — command mode must NOT require it.
        let s = mgr
            .start(
                0,
                "shh".to_string(),
                "review".to_string(),
                "check".to_string(),
                "prmonitor-no-such-cloudflared".to_string(),
                TunnelSpec {
                    mode: WebhookTunnelMode::Command,
                    command: "sleep 30 {port}".to_string(),
                    public_url: "https://my.example.com".to_string(),
                },
            )
            .await
            .expect("command-mode start succeeds without cloudflared");

        assert!(s.running);
        assert_eq!(s.public_url.as_deref(), Some("https://my.example.com"));
        assert_eq!(
            s.payload_url.as_deref(),
            Some("https://my.example.com/webhook")
        );

        // status() re-reports the configured URL while the child is alive (no self-heal).
        let s2 = mgr.status("prmonitor-no-such-cloudflared").await;
        assert!(s2.running);
        assert_eq!(s2.public_url.as_deref(), Some("https://my.example.com"));

        mgr.stop();
    }

    /// `command` mode with a child that exits IMMEDIATELY (`true`) self-heals on the next
    /// `status` exactly like quick mode — the `Option<Child>` probe still flips a dead
    /// tunnel to not-running.
    #[tokio::test]
    async fn command_mode_self_heals_when_child_exits() {
        let mgr = WebhookManager::default();
        mgr.set_dispatcher(Arc::new(|_| Box::pin(async {})));

        let s = mgr
            .start(
                0,
                "shh".to_string(),
                "review".to_string(),
                "check".to_string(),
                "bogus".to_string(),
                TunnelSpec {
                    mode: WebhookTunnelMode::Command,
                    command: "true".to_string(), // exits 0 immediately
                    public_url: String::new(),
                },
            )
            .await
            .expect("start spawns the (short-lived) child");
        assert!(s.running);
        // Empty public_url → None.
        assert!(s.public_url.is_none());

        // Give the child a moment to exit, then status must self-heal to not-running.
        let mut child = Command::new("true").kill_on_drop(true).spawn().unwrap();
        let _ = child.wait().await;
        let s2 = mgr.status("bogus").await;
        assert!(
            !s2.running,
            "an exited command-mode child flips running → false"
        );
        assert!(mgr.runtime.lock().unwrap().is_none());
    }

    /// `listener` mode spawns NO child (`tunnel: None`): it stays running across
    /// repeated `status` probes (no crash self-heal — there's no child to crash) and
    /// reports the configured `public_url`. The bogus cloudflared bin proves listener
    /// mode doesn't require it.
    #[tokio::test]
    async fn listener_mode_has_no_child_and_does_not_self_heal() {
        let mgr = WebhookManager::default();
        mgr.set_dispatcher(Arc::new(|_| Box::pin(async {})));

        let s = mgr
            .start(
                0,
                "shh".to_string(),
                "review".to_string(),
                "check".to_string(),
                "prmonitor-no-such-cloudflared".to_string(),
                TunnelSpec {
                    mode: WebhookTunnelMode::Listener,
                    command: String::new(),
                    public_url: "https://external.example.com".to_string(),
                },
            )
            .await
            .expect("listener-mode start succeeds without cloudflared");
        assert!(s.running);
        assert_eq!(
            s.public_url.as_deref(),
            Some("https://external.example.com")
        );

        // No tunnel child → runtime carries `tunnel: None` / `drain_task: None`.
        {
            let guard = mgr.runtime.lock().unwrap();
            let rt = guard.as_ref().expect("runtime present");
            assert!(rt.tunnel.is_none(), "listener mode spawns no tunnel child");
            assert!(rt.drain_task.is_none(), "listener mode has no drain task");
        }

        // Probe repeatedly: a childless runtime never self-heals to not-running.
        for _ in 0..3 {
            let st = mgr.status("prmonitor-no-such-cloudflared").await;
            assert!(st.running, "listener mode stays running across probes");
            assert_eq!(
                st.public_url.as_deref(),
                Some("https://external.example.com")
            );
        }
        assert!(
            mgr.runtime.lock().unwrap().is_some(),
            "listener runtime is never self-heal-taken"
        );

        mgr.stop();
    }

    /// `command` mode with a blank command is a defensive `AppError` at `start` (validate
    /// rejects it upstream, but `start` must not silently spawn nothing).
    #[tokio::test]
    async fn command_mode_blank_command_errs() {
        let mgr = WebhookManager::default();
        mgr.set_dispatcher(Arc::new(|_| Box::pin(async {})));

        let r = mgr
            .start(
                0,
                "shh".to_string(),
                "review".to_string(),
                "check".to_string(),
                "bogus".to_string(),
                TunnelSpec {
                    mode: WebhookTunnelMode::Command,
                    command: "   ".to_string(),
                    public_url: String::new(),
                },
            )
            .await;
        assert!(r.is_err(), "blank command-mode command must Err");
    }
}
