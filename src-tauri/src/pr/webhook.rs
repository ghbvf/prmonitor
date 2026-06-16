//! Webhook PR-trigger source (#9): a local `POST /webhook` receiver exposed to the
//! public internet via a Cloudflare Quick Tunnel, so GitHub pushes a PR event the
//! instant a trigger label lands instead of waiting for the poll interval.
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
use crate::model::Candidate;

type HmacSha256 = Hmac<Sha256>;

/// cloudflared prints the assigned Quick Tunnel URL to stderr within a few seconds;
/// cap the wait so a stuck binary can't hang `start_webhook`.
const TUNNEL_URL_TIMEOUT: Duration = Duration::from_secs(20);
/// `cloudflared --version` probe budget (the install check).
const CLOUDFLARED_VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// webhook receiver + tunnel status reported to the frontend (pr-slice-private wire
/// type; not a cross-slice contract, so it is mirrored in `src/pr/types.ts`, not
/// `model.rs` / `src/types.ts` — same placement as [`super::gh::GhStatus`]).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookStatus {
    /// Whether the local receiver + tunnel are currently running.
    pub running: bool,
    /// The public `https://*.trycloudflare.com` URL to paste into GitHub, when
    /// resolved. `None` while stopped or if the URL didn't appear within the timeout.
    pub public_url: Option<String>,
    /// Whether `cloudflared` is runnable (so the UI can prompt to install it).
    pub cloudflared_installed: bool,
    /// Human-readable (Chinese) status line for the UI.
    pub message: String,
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

/// The live receiver + tunnel handles. `stop_inner` aborts `server_task` and
/// `_drain` (explicit — neither is aborted by `Drop`) before dropping this value;
/// `_tunnel` (the cloudflared child) is then killed via `kill_on_drop(true)` on drop.
struct WebhookRuntime {
    server_task: JoinHandle<()>,
    /// stderr-drain task for the cloudflared child (keeps the pipe from filling).
    /// Aborted in `stop_inner` for lifecycle symmetry rather than relying on the
    /// child-kill → pipe-EOF chain to end it.
    drain_task: JoinHandle<()>,
    /// Held only to keep the child alive (and `kill_on_drop` it when the runtime
    /// drops); never read after construction.
    _tunnel: Child,
    public_url: Option<String>,
}

impl WebhookManager {
    /// Install the dispatch hook (composition root, before any start).
    pub fn set_dispatcher(&self, d: Dispatcher) {
        *self.dispatcher.lock().unwrap() = Some(d);
    }

    /// Start (or restart) the local receiver + Quick Tunnel. Tears down any prior
    /// runtime first (so a config change re-binds cleanly). Returns the resolved
    /// status; a missing `cloudflared` is reported as `running: false` +
    /// `cloudflared_installed: false` rather than an error so the UI can prompt to
    /// install it.
    pub async fn start(
        &self,
        port: u16,
        secret: String,
        review_label: String,
        check_label: String,
        cloudflared_bin: String,
    ) -> AppResult<WebhookStatus> {
        let _guard = self.start_lock.lock().await;
        self.stop_inner(); // restart semantics — avoid double-bind.

        if !cloudflared_installed(&cloudflared_bin).await {
            return Ok(WebhookStatus {
                running: false,
                public_url: None,
                cloudflared_installed: false,
                message: "未找到 cloudflared，请先安装：brew install cloudflared".to_string(),
            });
        }

        let dispatcher = self
            .dispatcher
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| AppError::new("webhook dispatcher 未初始化".to_string()))?;

        // LOCAL bind only — the public path is the cloudflared tunnel; the raw port
        // is never world-reachable.
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
            .route("/webhook", post(handle_webhook))
            // Cap the public endpoint's request body. GitHub webhook payloads are well
            // under this (typically < 25 KiB); the limit bounds the memory a forged POST
            // can make us buffer before the HMAC check rejects it.
            .layer(DefaultBodyLimit::max(1024 * 1024))
            .with_state(ctx);
        let server_task = spawn(async move {
            let _ = axum::serve(listener, router.into_make_service()).await;
        });

        // If the tunnel can't start, the server task (already spawned, holding the
        // bound port) must be aborted here — otherwise it detaches and leaks the port,
        // and `self.runtime` stays `None` so a later `stop_inner` can't reap it.
        let (tunnel, drain_task, public_url) =
            match spawn_quick_tunnel(&cloudflared_bin, port).await {
                Ok(parts) => parts,
                Err(e) => {
                    server_task.abort();
                    return Err(e);
                }
            };

        *self.runtime.lock().unwrap() = Some(WebhookRuntime {
            server_task,
            drain_task,
            _tunnel: tunnel,
            public_url: public_url.clone(),
        });

        let message = match &public_url {
            Some(u) => format!("已启动，公网 URL：{u}"),
            None => "隧道已启动，但未能在超时内解析公网 URL（请查看 cloudflared 日志）".to_string(),
        };
        Ok(WebhookStatus {
            running: true,
            public_url,
            cloudflared_installed: true,
            message,
        })
    }

    /// Tear down the running receiver + tunnel (abort the server task, drop/kill the
    /// child). Sync + idempotent — safe from the app-exit `RunEvent` handler.
    fn stop_inner(&self) {
        if let Some(rt) = self.runtime.lock().unwrap().take() {
            rt.server_task.abort();
            rt.drain_task.abort();
            // rt._tunnel dropped here → kill_on_drop kills cloudflared.
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
    pub async fn status(&self, cloudflared_bin: &str) -> WebhookStatus {
        let (running, public_url) = {
            let guard = self.runtime.lock().unwrap();
            match guard.as_ref() {
                Some(rt) => (true, rt.public_url.clone()),
                None => (false, None),
            }
        };
        let installed = cloudflared_installed(cloudflared_bin).await;
        let message = if running {
            match &public_url {
                Some(u) => format!("运行中，公网 URL：{u}"),
                None => "运行中（公网 URL 尚未解析）".to_string(),
            }
        } else if installed {
            "未运行".to_string()
        } else {
            "未运行；未检测到 cloudflared（brew install cloudflared）".to_string()
        };
        WebhookStatus {
            running,
            public_url,
            cloudflared_installed: installed,
            message,
        }
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

    // Keep draining stderr for the child's lifetime so a full pipe can't stall
    // cloudflared after we stop scanning (mirrors codex's stderr drain). The handle
    // is returned so `stop_inner` can abort it; absent that, the child-kill → pipe-EOF
    // chain would still end it, but we prefer explicit teardown.
    let drain_task = spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

    Ok((child, drain_task, url))
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
        let v = serde_json::to_value(WebhookStatus {
            running: true,
            public_url: Some("https://x.trycloudflare.com".to_string()),
            cloudflared_installed: true,
            message: "ok".to_string(),
        })
        .expect("WebhookStatus serializes");
        assert!(v.get("running").is_some());
        assert!(v.get("publicUrl").is_some());
        assert!(v.get("cloudflaredInstalled").is_some());
        assert!(v.get("message").is_some());
        // snake_case forms absent — a rename would surface here.
        assert!(v.get("public_url").is_none());
        assert!(v.get("cloudflared_installed").is_none());
    }
}
