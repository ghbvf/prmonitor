//! Local REST API trigger source (AB#1043): a `127.0.0.1`-only axum listener that lets a
//! third party (curl / a CLI / a deeplink helper) trigger a PR review and poll for its
//! completion + comment URL. The first transport that can independently satisfy the whole
//! need — HTTP's request/response is the only channel that cleanly hands back both
//! "done" AND the comment link.
//!
//! **Separate from the webhook, never tunneled.** [`crate::pr::webhook`] binds `127.0.0.1`
//! too, but it is reached from the public internet via a Cloudflare/custom tunnel. THIS
//! listener is loopback-only and MUST never be exposed — it is the inbound trigger control
//! plane, not a public push receiver. They share no port and no tunnel.
//!
//! **Resident.** Unlike the user-toggled webhook, this listener is started once in
//! `lib.rs` `setup()` and lives for the app's lifetime. The bound PORT is read once at
//! startup (a change needs an app restart, like the webhook port); the TOKEN is read LIVE
//! from config on every request, so setting / rotating / clearing it in Settings takes
//! effect on the next request with no restart.
//!
//! **Security — `127.0.0.1` binding is NOT enough.** Two threat classes reach loopback:
//! any local process, and any website via DNS rebinding (precedent: Ollama CVE-2024-28224,
//! a localhost API made remotely exploitable by rebinding, fixed by adding Host-header
//! validation). The handler applies a layered, whitelist-not-blacklist defense, in order,
//! before any side effect — each a pure, unit-tested function (governance: all **Medium**,
//! carriers are the `#[cfg(test)]` unit tests below; no **Soft** mechanism is introduced):
//!  1. [`security::host_allowed`] — exact loopback `Host` allowlist (DNS-rebinding defense).
//!  2. [`security::origin_allowed`] — `Origin` / `Sec-Fetch-Site` anti-CSRF (a header-less
//!     non-browser client like curl is allowed; a cross-site browser request is rejected).
//!  3. [`security::verify_bearer`] — constant-time `Authorization: Bearer` compare against
//!     the live config token (`subtle::ConstantTimeEq`, the same primitive the webhook's
//!     Azure path uses); an empty configured token fail-closes EVERY request to 401 (a blank
//!     token is the "disabled" sentinel).
//!
//! The endpoints copy `gh`'s two-layer model (trigger-and-return + poll):
//!  - `POST /reviews` `{projectId|repo, pr, kind}` → `202 {id, statusUrl}` (wraps the
//!    [`crate::review::commands::trigger_review`] funnel — inheriting its kind validation,
//!    id-or-repo project resolution, and dedup).
//!  - `GET /reviews/{id}` → `200 {status, commentUrl?}` (the in-memory registry, falling
//!    through to the durable by-id read for a finished / post-restart session).

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use tauri::async_runtime::{spawn, JoinHandle};
use tauri::Manager;
use tokio::sync::oneshot;

use super::session::{SessionInfo, SessionStatus};
use crate::config::service as config_service;
use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Bounded bind retry to ride out OS socket-release lag on a rapid restart (mirrors the
/// webhook receiver's F3 handling). A final failure is logged + swallowed, never propagated.
const BIND_RETRIES: u32 = 10;
const BIND_RETRY_DELAY: Duration = Duration::from_millis(20);
/// Trigger bodies are tiny (`{projectId, pr, kind}`). Cap what an unauthenticated POST can
/// make us buffer before the auth check rejects it (the same body-cap defense the webhook
/// receiver uses, with a smaller cap — a trigger body is far smaller than a webhook payload).
const MAX_BODY_BYTES: usize = 64 * 1024;

// ===========================================================================================
// Wire types (serde camelCase — golden-locked by the wire-shape tests below; **Medium**).
// HTTP-only: these are NOT Tauri commands, so they intentionally have NO `src/types.ts` mirror —
// the camelCase contract is locked by the goldens here, not by a hand-mirrored TS interface.
// ===========================================================================================

/// `POST /reviews` request body. Exactly one of `projectId` / `repo` identifies the project;
/// both map to the trigger funnel's free-form `reference` (id-or-repo). Not
/// `deny_unknown_fields`, so an extra key is ignored — but a snake_case `project_id` is NOT
/// the camelCase `projectId` field, so it stays `None` (the wire-shape test locks this).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TriggerRequest {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    pr: u64,
    kind: String,
}

/// `POST /reviews` success body → `202`. `statusUrl` is the absolute loopback URL the caller
/// polls (`gh run watch` analogue).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TriggerResponse {
    id: String,
    status_url: String,
}

/// `GET /reviews/{id}` success body → `200`. `status` is the session state machine value
/// (camelCase `starting`/`running`/`interrupting`/`done`/`failed`); `commentUrl` is the
/// resolved pr-review comment link, present only at a `completed` terminal (omitted otherwise,
/// matching `SessionInfo`'s `skip_serializing_if`). A poller waits for `status == "done"` with
/// a non-empty `commentUrl`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    status: SessionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    comment_url: Option<String>,
}

/// Uniform error envelope `{message}` (the same shape `AppError` serializes to, so the wire is
/// consistent whether the message came from a gate or from the trigger funnel).
#[derive(Debug, Serialize)]
struct ErrorBody {
    message: String,
}

// ===========================================================================================
// Pure security functions — the heart of AB#1043. All unit-tested (governance: **Medium**).
// ===========================================================================================

mod security {
    use subtle::ConstantTimeEq;

    /// DNS-rebinding defense (precedent: Ollama CVE-2024-28224). Accept ONLY an exact loopback
    /// `Host` (with or without the bound port) — `127.0.0.1` / `localhost` / `[::1]`. A rebind
    /// attack's request still carries the attacker's hostname (e.g. `evil.com`) in `Host` even
    /// though the socket hit `127.0.0.1`, so a non-loopback or wrong-port `Host` is rejected.
    /// Whitelist, never blacklist. Case-insensitive (host names are ASCII-case-insensitive).
    pub fn host_allowed(host: &str, port: u16) -> bool {
        let allowed = [
            "127.0.0.1".to_string(),
            format!("127.0.0.1:{port}"),
            "localhost".to_string(),
            format!("localhost:{port}"),
            "[::1]".to_string(),
            format!("[::1]:{port}"),
        ];
        allowed.iter().any(|a| a.eq_ignore_ascii_case(host))
    }

    /// Anti-CSRF. A browser `fetch`/XHR attaches `Origin` (and modern browsers `Sec-Fetch-Site`);
    /// curl / a CLI attaches NEITHER. Whitelist policy:
    ///  - `Sec-Fetch-Site` present (browser-set, unforgeable by page JS) → allow only
    ///    `same-origin` / `none`; reject `cross-site` / `same-site`.
    ///  - else `Origin` present → allow only an exact loopback origin (the bound port, or no
    ///    port); reject anything else (a malicious page's origin).
    ///  - else (no `Origin`, no `Sec-Fetch-Site`) → allow (a non-browser client; the Bearer
    ///    token + `Host` gate are its guards).
    pub fn origin_allowed(origin: Option<&str>, sec_fetch_site: Option<&str>, port: u16) -> bool {
        if let Some(sfs) = sec_fetch_site {
            return matches!(sfs, "same-origin" | "none");
        }
        match origin {
            None => true,
            Some(o) => is_loopback_origin(o, port),
        }
    }

    fn is_loopback_origin(origin: &str, port: u16) -> bool {
        const HOSTS: [&str; 3] = ["http://127.0.0.1", "http://localhost", "http://[::1]"];
        let with_port = format!(":{port}");
        HOSTS
            .iter()
            .any(|h| origin == *h || origin.strip_prefix(h).is_some_and(|rest| rest == with_port))
    }

    /// Constant-time `Authorization: Bearer <token>` verify against the configured token
    /// (mirrors the webhook's `verify_azure_token`). An EMPTY configured token fail-closes
    /// EVERY request (a blank token is the "disabled" sentinel — never "open"). The `Bearer`
    /// scheme is matched case-insensitively (RFC 7235 §2.1). `subtle::ConstantTimeEq` leaks
    /// only the token LENGTH (the `ct_eq` length short-circuit), never its CONTENT via timing.
    pub fn verify_bearer(token: &str, header: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let Some(presented) = header
            .split_once(' ')
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, t)| t)
        else {
            return false;
        };
        presented.as_bytes().ct_eq(token.as_bytes()).into()
    }
}

/// Resolve the trigger funnel's free-form `reference` from the request: exactly one of
/// `projectId` / `repo` must be present and non-blank. Pure (unit-tested). Both / neither is a
/// client error (`400`); the resolved string flows into [`config_service`]'s id-or-repo lookup
/// (via `trigger_review`), which rejects an unknown / ambiguous reference itself.
fn resolve_reference(req: &TriggerRequest) -> AppResult<String> {
    let project_id = req
        .project_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let repo = req.repo.as_deref().map(str::trim).filter(|s| !s.is_empty());
    match (project_id, repo) {
        (Some(id), None) => Ok(id.to_string()),
        (None, Some(r)) => Ok(r.to_string()),
        (Some(_), Some(_)) => Err(AppError::new("projectId 与 repo 只能填一个")),
        (None, None) => Err(AppError::new("必须提供 projectId 或 repo 之一（非空）")),
    }
}

// ===========================================================================================
// HTTP plumbing — JSON responses without axum's `json` feature (manual serde + Content-Type).
// ===========================================================================================

fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response {
    match serde_json::to_string(body) {
        Ok(s) => (status, [(header::CONTENT_TYPE, "application/json")], s).into_response(),
        // Serializing our own small structs cannot realistically fail; degrade to a bare
        // status so this stays infallible (no `unwrap` in a request path).
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    json_response(
        status,
        &ErrorBody {
            message: message.into(),
        },
    )
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The fixed-order security gate shared by both handlers (fail-closed, cheapest checks first):
/// `Host` → `Origin`/`Sec-Fetch-Site` → live-token `Bearer`. Returns `None` when the request
/// passes; `Some(resp)` is the rejection (`403` host/origin, `401` auth) to return immediately.
/// (`Option<Response>` rather than `Result<(), Response>` so the gate doesn't carry a large
/// `Err` variant — a passing request is the empty `None`.)
fn check_request<R: tauri::Runtime>(ctx: &Ctx<R>, headers: &HeaderMap) -> Option<Response> {
    // 1. Host allowlist (DNS-rebinding). A missing / non-UTF-8 Host fails closed.
    if !security::host_allowed(header_str(headers, "host").unwrap_or(""), ctx.port) {
        return Some(error_response(
            StatusCode::FORBIDDEN,
            "Host 不被允许（仅 loopback 可访问）",
        ));
    }
    // 2. Origin / Sec-Fetch-Site (anti-CSRF).
    if !security::origin_allowed(
        header_str(headers, "origin"),
        header_str(headers, "sec-fetch-site"),
        ctx.port,
    ) {
        return Some(error_response(
            StatusCode::FORBIDDEN,
            "Origin 不被允许（跨站请求已拒绝）",
        ));
    }
    // 3. Bearer token, read LIVE from config (rotate without restart). A blank token disables
    //    the endpoint (verify_bearer fail-closes); a config load error also fail-closes to 401 —
    //    a broken config must never silently open the trigger endpoint.
    let token = match config_service::load(&ctx.app) {
        Ok(cfg) => cfg.local_api_token,
        Err(_) => {
            return Some(error_response(StatusCode::UNAUTHORIZED, "鉴权不可用"));
        }
    };
    if !security::verify_bearer(
        token.trim(),
        header_str(headers, "authorization").unwrap_or(""),
    ) {
        return Some(error_response(StatusCode::UNAUTHORIZED, "未授权"));
    }
    None
}

// ===========================================================================================
// Handlers.
// ===========================================================================================

async fn handle_create<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(resp) = check_request(&ctx, &headers) {
        return resp;
    }
    let req: TriggerRequest = match serde_json::from_slice(body.as_ref()) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, format!("请求体解析失败: {e}")),
    };
    let reference = match resolve_reference(&req) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, e.message),
    };
    // Reuse the transport-agnostic funnel directly (it is `pub` + generic over the runtime).
    // `State` is obtained from the owned app handle and lives across the await (same lifetime
    // shape `trigger_review` itself uses). A `Deduped` / bad-kind / unknown-project surfaces as
    // an `Err` → 400, per the work item.
    let state = ctx.app.state::<AppState>();
    match super::commands::trigger_review(ctx.app.clone(), state, reference, req.pr, req.kind).await
    {
        Ok(id) => {
            let status_url = format!("http://127.0.0.1:{}/reviews/{}", ctx.port, id);
            json_response(StatusCode::ACCEPTED, &TriggerResponse { id, status_url })
        }
        Err(e) => error_response(StatusCode::BAD_REQUEST, e.message),
    }
}

/// GET status resolution (AB#1043, codex F1): an in-memory hit short-circuits (the durable
/// thunk never runs); otherwise the durable read's result passes through UNCHANGED. A
/// persistence `Err` (DB locked / IO / schema) must reach the caller as a `500`, NEVER be
/// folded into a not-found `404` — a poller must not read "status service unavailable" as
/// "this id does not exist". Pure; unit-tested (the prior `.ok().flatten()` swallowed the Err).
fn resolve_session(
    in_memory: Option<SessionInfo>,
    durable: impl FnOnce() -> AppResult<Option<SessionInfo>>,
) -> AppResult<Option<SessionInfo>> {
    match in_memory {
        Some(info) => Ok(Some(info)),
        None => durable(),
    }
}

async fn handle_status<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(resp) = check_request(&ctx, &headers) {
        return resp;
    }
    // In-memory first (live / just-finished), then the durable by-id read (finished long ago /
    // after a restart). The POST's `id` is the codex thread id, so this key matches exactly.
    let lookup = resolve_session(ctx.app.state::<AppState>().sessions.get(&id), || {
        super::history_store::get_session(ctx.app.state::<Database>().inner(), &id)
    });
    match lookup {
        Ok(Some(info)) => json_response(
            StatusCode::OK,
            &StatusResponse {
                status: info.status,
                comment_url: info.comment_url,
            },
        ),
        // Do not echo the caller-supplied `id` back into the message (avoid reflecting
        // untrusted path input into a body a downstream tool might log/render); the caller
        // already knows which id it polled from the request line.
        Ok(None) => error_response(StatusCode::NOT_FOUND, "未找到指定的 review 会话"),
        // A durable-lookup failure is "service unavailable", not "id absent" — surface 500
        // (without echoing the internal error detail) so the poller doesn't misread it as 404.
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "查询 review 会话失败"),
    }
}

// ===========================================================================================
// Manager — resident listener lifecycle (mirrors `WebhookManager`'s `&self` + interior mut).
// ===========================================================================================

/// Shared handler state. Holds the app handle (to reach `trigger_review` / the registry / the
/// live config) + the bound port (for `statusUrl`). It does NOT snapshot the token — that is
/// read live per request.
struct Ctx<R: tauri::Runtime> {
    app: tauri::AppHandle<R>,
    port: u16,
}

struct LocalApiRuntime {
    server_task: JoinHandle<()>,
    /// Graceful-shutdown signal; firing it lets `axum::serve` drop the listener before the
    /// task ends (so a future re-bind on the same port is clean, as in the webhook receiver).
    shutdown: oneshot::Sender<()>,
}

/// Resident local REST API listener handle, lives in [`crate::state::AppState`]. `Default`
/// (no listener until [`start`](Self::start)); methods take `&self`.
#[derive(Default)]
pub struct LocalApiManager {
    runtime: StdMutex<Option<LocalApiRuntime>>,
}

impl LocalApiManager {
    /// Start the resident listener (called once from `lib.rs` `setup()`). Reads the bound port
    /// once; `port == 0` means "off" (no bind). The bind + serve run in a spawned task whose
    /// final bind failure is LOGGED + SWALLOWED — a port clash must never crash the desktop app
    /// (setup does not await this), and the feature is opt-in via the curl client anyway.
    pub fn start<R: tauri::Runtime>(&self, app: tauri::AppHandle<R>) {
        // Idempotent: a second `start` (e.g. a future setup refactor) must NOT spawn a second
        // listener and orphan the first's task — one resident listener for the app's life.
        if self.runtime.lock().unwrap().is_some() {
            return;
        }
        let port = match config_service::load(&app) {
            Ok(cfg) => cfg.local_api_port,
            Err(e) => {
                eprintln!("本地 API：读取配置失败，跳过启动：{e}");
                return;
            }
        };
        if port == 0 {
            return;
        }
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let ctx = Arc::new(Ctx {
            app: app.clone(),
            port,
        });
        let server_task = spawn(async move {
            let listener = {
                let mut bound = None;
                let mut last_err = None;
                for attempt in 0..BIND_RETRIES {
                    match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                        Ok(l) => {
                            bound = Some(l);
                            break;
                        }
                        Err(e) => {
                            last_err = Some(e);
                            if attempt + 1 < BIND_RETRIES {
                                tokio::time::sleep(BIND_RETRY_DELAY).await;
                            }
                        }
                    }
                }
                match bound {
                    Some(l) => l,
                    None => {
                        // Defensive: `last_err` is `Some` whenever a bind was attempted and
                        // failed (BIND_RETRIES > 0), but degrade gracefully rather than panic
                        // in this detached task if that invariant ever changes.
                        let reason = last_err
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "no bind attempt (BIND_RETRIES is 0)".to_string());
                        eprintln!("本地 API：端口 {port} 监听失败，已跳过（{reason}）");
                        return;
                    }
                }
            };
            let router = Router::new()
                .route("/reviews", post(handle_create::<R>))
                .route("/reviews/:id", get(handle_status::<R>))
                .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
                .with_state(ctx);
            let _ = axum::serve(listener, router.into_make_service())
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        *self.runtime.lock().unwrap() = Some(LocalApiRuntime {
            server_task,
            shutdown: shutdown_tx,
        });
    }

    /// App-shutdown cleanup (wired to `RunEvent::Exit` in `lib.rs`, like the other managers):
    /// fire the graceful-shutdown signal + abort the task so the listener never outlives the app.
    /// Sync best-effort (the exit handler can't await).
    pub fn shutdown(&self) {
        if let Some(rt) = self.runtime.lock().unwrap().take() {
            let _ = rt.shutdown.send(());
            rt.server_task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::security::{host_allowed, origin_allowed, verify_bearer};
    use super::*;

    // --- host_allowed (DNS-rebinding defense; Medium) ---------------------------------------

    #[test]
    fn host_allowed_accepts_loopback_with_and_without_port() {
        for h in [
            "127.0.0.1",
            "127.0.0.1:8788",
            "localhost",
            "localhost:8788",
            "[::1]",
            "[::1]:8788",
            "LOCALHOST:8788", // case-insensitive
        ] {
            assert!(host_allowed(h, 8788), "should accept {h}");
        }
    }

    #[test]
    fn host_allowed_rejects_non_loopback_wrong_port_and_empty() {
        for h in [
            "evil.com",
            "evil.com:8788",
            "127.0.0.1.evil.com", // rebinding-style suffix
            "127.0.0.1:9999",     // wrong port
            "0.0.0.0:8788",
            "192.168.1.5:8788",
            "", // missing / non-UTF-8 Host → fail closed
        ] {
            assert!(!host_allowed(h, 8788), "should reject {h}");
        }
    }

    // --- origin_allowed (anti-CSRF; Medium) -------------------------------------------------

    #[test]
    fn origin_allowed_passes_header_less_client() {
        // curl / CLI: no Origin, no Sec-Fetch-Site → allowed (the token + Host gate guard it).
        assert!(origin_allowed(None, None, 8788));
    }

    #[test]
    fn origin_allowed_honors_sec_fetch_site() {
        assert!(origin_allowed(None, Some("same-origin"), 8788));
        assert!(origin_allowed(None, Some("none"), 8788));
        assert!(!origin_allowed(None, Some("cross-site"), 8788));
        assert!(!origin_allowed(None, Some("same-site"), 8788));
        // Sec-Fetch-Site wins even when an Origin is also present.
        assert!(!origin_allowed(
            Some("http://127.0.0.1:8788"),
            Some("cross-site"),
            8788
        ));
    }

    #[test]
    fn origin_allowed_falls_back_to_loopback_origin() {
        assert!(origin_allowed(Some("http://127.0.0.1:8788"), None, 8788));
        assert!(origin_allowed(Some("http://localhost:8788"), None, 8788));
        assert!(origin_allowed(Some("http://127.0.0.1"), None, 8788)); // no port
        assert!(!origin_allowed(Some("http://evil.com"), None, 8788));
        assert!(!origin_allowed(Some("http://127.0.0.1:9999"), None, 8788)); // wrong port
        assert!(!origin_allowed(Some("https://127.0.0.1:8788"), None, 8788)); // https scheme
    }

    // --- verify_bearer (fail-closed auth; Medium — the "no token → reject" golden) ----------

    #[test]
    fn verify_bearer_empty_token_rejects_everything() {
        // The charter-mandated "no token → reject" golden: a blank configured token denies ALL.
        assert!(!verify_bearer("", "Bearer whatever"));
        assert!(!verify_bearer("", ""));
        assert!(!verify_bearer("", "Bearer "));
    }

    #[test]
    fn verify_bearer_accepts_correct_token_any_scheme_case() {
        assert!(verify_bearer("s3cret-token", "Bearer s3cret-token"));
        assert!(verify_bearer("s3cret-token", "bearer s3cret-token")); // case-insensitive scheme
        assert!(verify_bearer("s3cret-token", "BEARER s3cret-token"));
    }

    #[test]
    fn verify_bearer_rejects_wrong_missing_and_malformed() {
        assert!(!verify_bearer("s3cret-token", "Bearer wrong"));
        assert!(!verify_bearer("s3cret-token", "s3cret-token")); // no scheme
        assert!(!verify_bearer("s3cret-token", "Basic s3cret-token")); // wrong scheme
        assert!(!verify_bearer("s3cret-token", "")); // missing header
        assert!(!verify_bearer("s3cret-token", "Bearer")); // no token part
    }

    // --- resolve_reference ------------------------------------------------------------------

    fn req(project_id: Option<&str>, repo: Option<&str>) -> TriggerRequest {
        TriggerRequest {
            project_id: project_id.map(str::to_string),
            repo: repo.map(str::to_string),
            pr: 7,
            kind: "review".to_string(),
        }
    }

    #[test]
    fn resolve_reference_accepts_exactly_one() {
        assert_eq!(resolve_reference(&req(Some("p1"), None)).unwrap(), "p1");
        assert_eq!(
            resolve_reference(&req(None, Some("owner/name"))).unwrap(),
            "owner/name"
        );
    }

    #[test]
    fn resolve_reference_rejects_both_and_neither() {
        assert!(resolve_reference(&req(Some("p1"), Some("owner/name"))).is_err());
        assert!(resolve_reference(&req(None, None)).is_err());
        // Blank counts as absent → both blank is "neither".
        assert!(resolve_reference(&req(Some("  "), Some(""))).is_err());
        // One blank + one set resolves to the set one.
        assert_eq!(resolve_reference(&req(Some("  "), Some("r"))).unwrap(), "r");
    }

    // --- wire-shape goldens (serde camelCase contract; Medium) ------------------------------

    #[test]
    fn trigger_request_deserializes_camel_case_only() {
        let req: TriggerRequest = serde_json::from_value(
            serde_json::json!({"projectId": "p1", "pr": 9, "kind": "check"}),
        )
        .expect("camelCase body deserializes");
        assert_eq!(req.project_id.as_deref(), Some("p1"));
        assert_eq!(req.pr, 9);
        assert_eq!(req.kind, "check");

        // A snake_case `project_id` is NOT the camelCase field — it is ignored, leaving None.
        // This locks the wire contract (a rename to snake_case would surface here).
        let snake: TriggerRequest = serde_json::from_value(
            serde_json::json!({"project_id": "p1", "pr": 9, "kind": "check"}),
        )
        .expect("unknown key ignored");
        assert_eq!(snake.project_id, None);
    }

    #[test]
    fn trigger_response_wire_shape_is_camel_case() {
        let v = serde_json::to_value(&TriggerResponse {
            id: "th-1".to_string(),
            status_url: "http://127.0.0.1:8788/reviews/th-1".to_string(),
        })
        .expect("serializes");
        assert!(v.get("id").is_some());
        assert!(v.get("statusUrl").is_some());
        assert!(v.get("status_url").is_none());
    }

    #[test]
    fn status_response_wire_shape_and_optional_comment_url() {
        // Terminal with a URL: camelCase `commentUrl` present, `status` is the camelCase enum.
        let done = serde_json::to_value(&StatusResponse {
            status: SessionStatus::Done,
            comment_url: Some("https://x/c".to_string()),
        })
        .expect("serializes");
        assert_eq!(done["status"], "done");
        assert_eq!(done["commentUrl"], "https://x/c");
        assert!(done.get("comment_url").is_none());

        // Non-terminal with no URL: `commentUrl` is OMITTED (skip_serializing_if), not null.
        let running = serde_json::to_value(&StatusResponse {
            status: SessionStatus::Running,
            comment_url: None,
        })
        .expect("serializes");
        assert_eq!(running["status"], "running");
        assert!(running.get("commentUrl").is_none());
    }

    // --- resolve_session (codex F1: durable Err must not fold into 404) ----------------------

    fn sample_info(thread: &str) -> SessionInfo {
        SessionInfo {
            project_id: "p1".to_string(),
            thread_id: thread.to_string(),
            turn_id: String::new(),
            pr_number: 7,
            kind: "review".to_string(),
            status: SessionStatus::Done,
            created_at_epoch: 0,
            comment_url: None,
        }
    }

    #[test]
    fn resolve_session_in_memory_hit_short_circuits_durable() {
        // An in-memory hit must NOT touch the durable store (the thunk panics if run).
        let got = resolve_session(Some(sample_info("t1")), || {
            panic!("durable lookup must not run on an in-memory hit")
        });
        assert!(matches!(got, Ok(Some(info)) if info.thread_id == "t1"));
    }

    #[test]
    fn resolve_session_durable_error_is_not_folded_into_not_found() {
        // The codex F1 regression lock: a durable-lookup Err must PROPAGATE (→ 500), never
        // collapse to Ok(None) (→ 404). The prior `.ok().flatten()` swallowed it.
        let got = resolve_session(None, || Err(AppError::new("db locked")));
        assert!(got.is_err(), "durable Err must propagate, not fold to None");
    }

    #[test]
    fn resolve_session_durable_some_and_none_pass_through() {
        assert!(matches!(
            resolve_session(None, || Ok(Some(sample_info("t2")))),
            Ok(Some(_))
        ));
        assert!(matches!(resolve_session(None, || Ok(None)), Ok(None)));
    }
}
