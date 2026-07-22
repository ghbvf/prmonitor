//! Local REST API review-request source (AB#1043): an axum listener that lets a
//! third party (curl / a CLI / a remote browser client) request a PR review and poll for its
//! completion + comment URL. The first transport that can independently satisfy the whole
//! need — HTTP's request/response is the only channel that cleanly hands back both
//! "done" AND the comment link.
//!
//! **Separate from the webhook, never tunneled.** [`crate::pr::webhook`] binds `127.0.0.1`
//! too, but it is reached from the public internet via a Cloudflare/custom tunnel. THIS
//! local listener is loopback-only; a Remote Access entrypoint may mount this same typed router
//! behind its own gate. It is the inbound review-request control plane, not a public push receiver.
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
//! The endpoints copy `gh`'s two-layer model (submit-and-return + poll):
//!  - `POST /reviews` `{projectId|repo, pr, skill_key, requestId}` →
//!    `202 {receiptId, statusUrl}` (durably inserts one external review intent).
//!  - `GET /reviews/{receiptId}` → the durable queue/session aggregate, including states before
//!    a thread exists and after an app restart.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use tauri::Manager;
use tokio_stream::wrappers::BroadcastStream;

#[cfg(test)]
use super::session::{SessionInfo, SessionStatus};
use crate::config::service as config_service;
use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::events::{ReviewEvent, StreamEvent};
use crate::model::{
    ExternalRequestId, ExternalTriggerOrigin, ReviewReceiptId, ReviewReceiptSnapshot,
    ReviewReceiptStatus, SendNotificationRequest,
};
use crate::state::AppState;

const RECEIPT_QUERY_FAILED_MESSAGE: &str = "查询 review receipt 失败";

/// Review-request bodies are tiny (`{projectId, pr, skill_key, requestId}`). Cap what an unauthenticated POST can
/// make us buffer before the auth check rejects it (the same body-cap defense the webhook
/// receiver uses, with a smaller cap — this body is far smaller than a webhook payload).
const MAX_BODY_BYTES: usize = 64 * 1024;

// ===========================================================================================
// Wire types (serde camelCase — golden-locked by the wire-shape tests below; **Medium**).
// HTTP-only: these are NOT Tauri commands, so they intentionally have NO `src/types.ts` mirror —
// the camelCase contract is locked by the goldens here, not by a hand-mirrored TS interface.
// ===========================================================================================

/// `POST /reviews` request body. Exactly one of `projectId` / `repo` identifies the project;
/// both map to the request funnel's free-form `reference` (id-or-repo). Not
/// `deny_unknown_fields`, so an extra key is ignored — but a snake_case `project_id` is NOT
/// the camelCase `projectId` field, so it stays `None` (the wire-shape test locks this).
// `pub(crate)` + both `Serialize` and `Deserialize` so the AB#1044 CLI client builds + SENDS the
// very SAME struct the server RECEIVES — ONE definition, both sides (governance: Hard, zero
// drift; the round-trip goldens below lock both directions). `skip_serializing_if` on the
// optional project refs keeps the client's outbound body to the one ref it set.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReviewRequestBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) repo: Option<String>,
    pub(crate) pr: u64,
    /// Only `""` or `"--check"` — skill identity is resolved from project rules server-side.
    #[serde(default)]
    pub(crate) extra_args: String,
    pub(crate) request_id: ExternalRequestId,
}

/// `POST /reviews` success body → `202`. `statusUrl` is absolute loopback for a local client and
/// relative to the mounted LocalApi base path for a remote browser client.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewReceiptAccepted {
    pub(crate) receipt_id: ReviewReceiptId,
    pub(crate) status_url: String,
}

/// Shared durable receipt snapshot used by HTTP and the CLI client.
pub(crate) type StatusResponse = ReviewReceiptSnapshot;

/// Uniform error envelope `{message}` (the same shape `AppError` serializes to, so the wire is
/// consistent whether the message came from a gate or from the request funnel).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ErrorBody {
    pub(crate) message: String,
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

/// Resolve the request funnel's free-form `reference` from the request: exactly one of
/// `projectId` / `repo` must be present and non-blank. Pure (unit-tested). Both / neither is a
/// client error (`400`); the resolved string flows into the external ingress's id-or-repo lookup.
fn resolve_reference(req: &ReviewRequestBody) -> AppResult<String> {
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
    if ctx.remote_entrypoint_id.is_none() {
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
    }
    // 3. Bearer token, read LIVE from config (rotate without restart). A blank token disables
    //    the endpoint (verify_bearer fail-closes); a config load error also fail-closes to 401 —
    //    a broken config must never silently open the review-request endpoint.
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
    let req: ReviewRequestBody = match serde_json::from_slice(body.as_ref()) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, format!("请求体解析失败: {e}")),
    };
    let reference = match resolve_reference(&req) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, e.message),
    };
    let origin = if ctx.remote_entrypoint_id.is_some() {
        ExternalTriggerOrigin::RemoteWeb
    } else if header_str(&headers, "x-prmonitor-client") == Some("cli") {
        ExternalTriggerOrigin::Cli
    } else {
        ExternalTriggerOrigin::Http
    };
    let state = ctx.app.state::<AppState>();
    if let Err(message) = crate::model::SkillInvocation::validate_extra_args(&req.extra_args) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }
    match state.external_review.submit(
        reference,
        req.pr,
        req.extra_args,
        req.request_id,
        origin,
        false,
    ) {
        Ok(receipt_id) => {
            let status_url = receipt_status_url(
                ctx.port,
                &ctx.base_path,
                ctx.remote_entrypoint_id.is_some(),
                receipt_id,
            );
            json_response(
                StatusCode::ACCEPTED,
                &ReviewReceiptAccepted {
                    receipt_id,
                    status_url,
                },
            )
        }
        Err(e) => error_response(StatusCode::BAD_REQUEST, e.message),
    }
}

fn receipt_status_url(
    port: u16,
    base_path: &str,
    remote: bool,
    receipt_id: ReviewReceiptId,
) -> String {
    let path = format!("{base_path}/reviews/{}", receipt_id.get());
    if remote {
        path
    } else {
        format!("http://127.0.0.1:{port}{path}")
    }
}

async fn handle_notify<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(resp) = check_request(&ctx, &headers) {
        return resp;
    }
    let req: SendNotificationRequest = match serde_json::from_slice(body.as_ref()) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, format!("请求体解析失败: {e}")),
    };
    let state = ctx.app.state::<AppState>();
    match state.notification_sender.send(req, None) {
        Ok(response) => json_response(StatusCode::ACCEPTED, &response),
        Err(e) => error_response(StatusCode::BAD_REQUEST, e.message),
    }
}

/// GET status resolution (AB#1043, codex F1): an in-memory hit short-circuits (the durable
/// thunk never runs); otherwise the durable read's result passes through UNCHANGED. A
/// persistence `Err` (DB locked / IO / schema) must reach the caller as a `500`, NEVER be
/// folded into a not-found `404` — a poller must not read "status service unavailable" as
/// "this id does not exist". Pure; unit-tested (the prior `.ok().flatten()` swallowed the Err).
#[cfg(test)]
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
    Path(id): Path<i64>,
) -> Response {
    if let Some(resp) = check_request(&ctx, &headers) {
        return resp;
    }
    let receipt_id = match ReviewReceiptId::new(id) {
        Ok(id) => id,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "receiptId 必须为正整数"),
    };
    let state = ctx.app.state::<AppState>();
    match state
        .external_review
        .get(ctx.app.state::<Database>().inner(), receipt_id)
    {
        Ok(snapshot) => json_response(StatusCode::OK, &snapshot),
        Err(e) if e.message.contains("不存在") || e.message.contains("not found") => {
            error_response(StatusCode::NOT_FOUND, "未找到指定的 review receipt")
        }
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            RECEIPT_QUERY_FAILED_MESSAGE,
        ),
    }
}

// ===========================================================================================
// SSE stream endpoint (AB#1072 / #1373): `GET /reviews/{id}/stream` — live review deltas +
// terminal status for ONE review id, served from the realtime stream bus (`crate::stream`). The
// live analogue of the `GET /reviews/{id}` poll; same fixed-order three-layer gate. The pure
// helpers AND the `review_event_stream` builder below are unit-tested; only the thin
// `handle_stream` gate/lookup wiring is exercised end-to-end manually (curl).
// ===========================================================================================

/// The owning `thread_id` of a session-scoped [`ReviewEvent`], or `None` for the session-less
/// [`ReviewEvent::DispatchError`]. The exhaustive `match` keeps this honest if a variant is added.
fn review_thread_id(event: &ReviewEvent) -> Option<&str> {
    match event {
        ReviewEvent::MessageDelta { thread_id, .. }
        | ReviewEvent::ReasoningDelta { thread_id, .. }
        | ReviewEvent::TurnCompleted { thread_id, .. }
        | ReviewEvent::Error { thread_id, .. } => Some(thread_id),
        ReviewEvent::DispatchError { .. } => None,
    }
}

/// Whether `event` is THIS id's terminal `turnCompleted` — the cue to close the SSE stream after
/// yielding it.
#[cfg(test)]
fn is_terminal_review(event: &ReviewEvent, id: &str) -> bool {
    matches!(event, ReviewEvent::TurnCompleted { thread_id, .. } if thread_id == id)
}

/// Build a synthetic terminal [`StreamEvent`] for a session that ALREADY finished before the
/// client connected: the bus does not replay past events, so the handler emits this ONE event from
/// the durable status and closes rather than hanging on keep-alive. `Done`→`"completed"`,
/// `Failed`→`"failed"` — the durable [`SessionInfo::status`] does not retain the raw codex
/// `interrupted` wire string (only the live `CompletionOutcome` does), so `Done` maps to
/// `completed`; the precise wire status is a live-stream detail while the `commentUrl` is the
/// payload that matters on reconnect.
#[cfg(test)]
fn terminal_stream_event(info: &SessionInfo) -> StreamEvent {
    let status = match info.status {
        SessionStatus::Done => "completed",
        SessionStatus::Failed => "failed",
        // Non-terminal never reaches here (the caller gates on Done/Failed); map defensively.
        SessionStatus::Starting | SessionStatus::Running | SessionStatus::Interrupting => "running",
    };
    StreamEvent::Review(ReviewEvent::TurnCompleted {
        project_id: info.project_id.clone(),
        thread_id: info.thread_id.clone(),
        status: status.to_string(),
        comment_url: info.comment_url.clone(),
    })
}

/// Serialize one [`StreamEvent`] as an SSE `data:` frame. `Event::json_data` needs axum's `json`
/// feature (off here — we run `default-features = false`), so serialize with `serde_json` (already
/// a dep) and set the `data:` field directly: compact JSON is single-line, valid for an SSE frame.
/// Serialization never fails for `StreamEvent`, but degrade to a tiny error frame rather than panic
/// the stream task — the `Infallible` item error keeps the axum `Sse` response from short-circuiting.
fn sse_data<T: Serialize>(event: &T) -> Result<SseEvent, std::convert::Infallible> {
    let payload = serde_json::to_string(event).unwrap_or_else(|_| {
        r#"{"domain":"review","kind":"dispatchError","projectId":"","message":"stream serialize error"}"#
            .to_string()
    });
    Ok(SseEvent::default().data(payload))
}

/// Build the live SSE event stream for one review `id` from a bus subscription: yields each of this
/// id's review events (filtering out other ids / the action domain) and CLOSES right AFTER this
/// id's terminal `turnCompleted` (`unfold` yields the terminal then ends on the next poll WITHOUT
/// waiting for another upstream event — a `scan`/`take_while` could not close until the next
/// matching event arrived). Runtime-free (no `tauri`) so it is `#[tokio::test]`-covered below.
///
/// `on_lag` is the fail-safe for a slow consumer (reviewer P2): if the broadcast ring overflows,
/// the dropped batch MAY have contained this id's terminal `turnCompleted` — without recovery the
/// stream would block forever (the bus `Sender` lives for the app's lifetime, so `rx.next()` never
/// returns `None`) and the connection would hang under keep-alive. On a lag, `on_lag()` re-reads the
/// durable status: a terminal session yields ONE synthetic terminal event and the stream closes; a
/// still-running one returns `None` and streaming continues (best-effort — the next live terminal
/// still closes it). The lag is also logged (parity with the session pump's lag breadcrumb).
#[cfg(test)]
fn review_event_stream<F>(
    rx: tokio::sync::broadcast::Receiver<StreamEvent>,
    id: String,
    on_lag: F,
) -> impl futures::Stream<Item = StreamEvent>
where
    F: Fn() -> Option<StreamEvent> + Send + 'static,
{
    stream::unfold(
        (BroadcastStream::new(rx), id, on_lag, false),
        |(mut rx, id, on_lag, done)| async move {
            if done {
                return None;
            }
            loop {
                match rx.next().await {
                    None => return None, // bus closed (app shutting down)
                    Some(Err(lagged)) => {
                        // The dropped batch may have held THIS id's terminal — re-check the durable
                        // status so a lagged client still gets a close instead of hanging forever.
                        eprintln!("SSE stream（{id}）滞后：{lagged}（按当前会话状态决定是否关闭）");
                        match on_lag() {
                            Some(term) => return Some((term, (rx, id, on_lag, true))),
                            None => continue,
                        }
                    }
                    Some(Ok(StreamEvent::Review(r)))
                        if review_thread_id(&r) == Some(id.as_str()) =>
                    {
                        let terminal = is_terminal_review(&r, &id);
                        return Some((StreamEvent::Review(r), (rx, id, on_lag, terminal)));
                    }
                    Some(Ok(_)) => continue, // another id / the action domain — not this stream
                }
            }
        },
    )
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ReceiptStreamEvent {
    Receipt {
        receipt: ReviewReceiptSnapshot,
    },
    Review {
        #[serde(rename = "receiptId")]
        receipt_id: ReviewReceiptId,
        event: ReviewEvent,
    },
    Error {
        #[serde(rename = "receiptId")]
        receipt_id: ReviewReceiptId,
        message: String,
    },
}

fn receipt_event_stream<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    rx: tokio::sync::broadcast::Receiver<StreamEvent>,
    receipt_id: ReviewReceiptId,
) -> impl futures::Stream<Item = ReceiptStreamEvent> {
    stream::unfold(
        (
            app,
            BroadcastStream::new(rx),
            receipt_id,
            None::<ReviewReceiptSnapshot>,
            false,
        ),
        |(app, mut rx, receipt_id, last, done)| async move {
            if done {
                return None;
            }
            loop {
                let state = app.state::<AppState>();
                let current = match state
                    .external_review
                    .get(app.state::<Database>().inner(), receipt_id)
                {
                    Ok(snapshot) => snapshot,
                    Err(_) => {
                        return Some((
                            ReceiptStreamEvent::Error {
                                receipt_id,
                                message: RECEIPT_QUERY_FAILED_MESSAGE.to_string(),
                            },
                            (app, rx, receipt_id, last, true),
                        ))
                    }
                };
                if last.as_ref() != Some(&current) {
                    let terminal = matches!(
                        current.status,
                        ReviewReceiptStatus::Done | ReviewReceiptStatus::Failed
                    );
                    return Some((
                        ReceiptStreamEvent::Receipt {
                            receipt: current.clone(),
                        },
                        (app, rx, receipt_id, Some(current), terminal),
                    ));
                }

                let Some(thread_id) = current.thread_id.as_deref() else {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    continue;
                };
                match tokio::time::timeout(std::time::Duration::from_millis(500), rx.next()).await {
                    Ok(Some(Ok(StreamEvent::Review(event))))
                        if review_thread_id(&event) == Some(thread_id) =>
                    {
                        return Some((
                            ReceiptStreamEvent::Review { receipt_id, event },
                            (app, rx, receipt_id, last, false),
                        ));
                    }
                    Ok(Some(_)) | Err(_) => continue,
                    Ok(None) => {
                        return Some((
                            ReceiptStreamEvent::Error {
                                receipt_id,
                                message: "review event stream 已关闭".to_string(),
                            },
                            (app, rx, receipt_id, last, true),
                        ));
                    }
                }
            }
        },
    )
}

async fn handle_stream<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if let Some(resp) = check_request(&ctx, &headers) {
        return resp;
    }
    let receipt_id = match ReviewReceiptId::new(id) {
        Ok(id) => id,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "receiptId 必须为正整数"),
    };
    let state = ctx.app.state::<AppState>();
    let rx = state.stream.subscribe();
    match state
        .external_review
        .get(ctx.app.state::<Database>().inner(), receipt_id)
    {
        Ok(_) => {}
        Err(e) if e.message.contains("不存在") || e.message.contains("not found") => {
            return error_response(StatusCode::NOT_FOUND, "未找到指定的 review receipt")
        }
        Err(_) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                RECEIPT_QUERY_FAILED_MESSAGE,
            )
        }
    }
    let live = receipt_event_stream(ctx.app.clone(), rx, receipt_id).map(|ev| sse_data(&ev));
    Sse::new(live)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ===========================================================================================
// Router builder — the local-api router, mounted by the listener supervisor (AB#1225).
// ===========================================================================================

/// Shared handler state. Holds the app handle (to reach external ingress / live config) + the
/// bound port (for `statusUrl`). It does NOT snapshot the token — that is
/// read live per request. `pub(crate)` so [`crate::remote::supervisor`] constructs it when it
/// binds the local-api port from a `config.listeners[]` entry.
pub(crate) struct Ctx<R: tauri::Runtime> {
    pub(crate) app: tauri::AppHandle<R>,
    pub(crate) port: u16,
    pub(crate) base_path: String,
    pub(crate) remote_entrypoint_id: Option<String>,
}

/// Build the local-api router for an already-resolved [`Ctx`]. Extracted (AB#1225) from the
/// former resident `LocalApiManager::start` so the listener supervisor (`crate::remote`) can
/// mount this SAME router — handlers + the 3-layer security gate ([`check_request`]) + the live
/// per-request token read are unchanged — on the port it binds. The local-api runtime is now
/// driven by `config.listeners[]` (single source of truth), not a resident manager.
pub(crate) fn build_router<R: tauri::Runtime>(ctx: Arc<Ctx<R>>) -> Router {
    Router::new()
        .route("/reviews", post(handle_create::<R>))
        .route("/notifications", post(handle_notify::<R>))
        .route("/reviews/:id", get(handle_status::<R>))
        .route("/reviews/:id/stream", get(handle_stream::<R>))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(ctx)
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

    fn req(project_id: Option<&str>, repo: Option<&str>) -> ReviewRequestBody {
        ReviewRequestBody {
            project_id: project_id.map(str::to_string),
            repo: repo.map(str::to_string),
            pr: 7,
            extra_args: String::new(),
            request_id: ExternalRequestId::parse("0123456789abcdef0123456789abcdef")
                .expect("request id"),
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
    fn review_request_body_deserializes_camel_case_only() {
        let req: ReviewRequestBody = serde_json::from_value(serde_json::json!({
            "projectId": "p1",
            "pr": 9,
            "extraArgs": "--check",
            "requestId": "0123456789abcdef0123456789abcdef"
        }))
        .expect("camelCase body deserializes");
        assert_eq!(req.project_id.as_deref(), Some("p1"));
        assert_eq!(req.pr, 9);
        assert_eq!(req.extra_args, "--check");
        assert_eq!(req.request_id.as_str(), "0123456789abcdef0123456789abcdef");

        let missing = serde_json::from_value::<ReviewRequestBody>(
            serde_json::json!({"projectId": "p1", "pr": 9, "extraArgs": "--check"}),
        )
        .expect_err("requestId is mandatory");
        assert!(missing.to_string().contains("requestId"));

        // Free skill fields are rejected (Hard ingress — skill resolved from rules server-side).
        let free_skill = serde_json::from_value::<ReviewRequestBody>(serde_json::json!({
            "projectId": "p1",
            "pr": 9,
            "skillName": "evil",
            "skillPath": "secrets.env",
            "commandTemplate": "rm -rf /",
            "requestId": "0123456789abcdef0123456789abcdef"
        }))
        .expect_err("free skill fields fail closed");
        assert!(free_skill.to_string().contains("unknown field"));

        let legacy = serde_json::from_value::<ReviewRequestBody>(serde_json::json!({
            "projectId": "p1",
            "pr": 9,
            "skill_key": "check",
            "requestId": "0123456789abcdef0123456789abcdef",
            "id": "legacy-thread"
        }))
        .expect_err("legacy fields fail closed");
        assert!(legacy.to_string().contains("unknown field"));

        for invalid in [
            "0123456789abcdef0123456789abcde",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcdeg",
        ] {
            let error = serde_json::from_value::<ReviewRequestBody>(serde_json::json!({
                "repo": "owner/name", "pr": 9, "requestId": invalid
            }))
            .expect_err("invalid requestId must fail closed");
            assert!(error.to_string().contains("requestId"), "{error}");
        }
    }

    #[test]
    fn review_receipt_accepted_wire_shape_is_camel_case() {
        let v = serde_json::to_value(&ReviewReceiptAccepted {
            receipt_id: ReviewReceiptId::new(42).expect("receipt"),
            status_url: "http://127.0.0.1:8788/reviews/42".to_string(),
        })
        .expect("serializes");
        assert_eq!(v["receiptId"], 42);
        assert!(v.get("statusUrl").is_some());
        assert!(v.get("id").is_none());
        assert!(v.get("status_url").is_none());
    }

    #[test]
    fn receipt_status_url_is_loopback_for_cli_and_relative_for_remote() {
        let receipt = ReviewReceiptId::new(42).expect("receipt");
        assert_eq!(
            receipt_status_url(8788, "/api", false, receipt),
            "http://127.0.0.1:8788/api/reviews/42"
        );
        assert_eq!(
            receipt_status_url(8788, "/api", true, receipt),
            "/api/reviews/42"
        );
    }

    #[test]
    fn status_response_wire_shape_and_optional_comment_url() {
        // Terminal with a URL: camelCase `commentUrl` present, `status` is the camelCase enum.
        let done = serde_json::to_value(&StatusResponse {
            receipt_id: ReviewReceiptId::new(42).expect("receipt"),
            status: ReviewReceiptStatus::Done,
            thread_id: Some("th-1".to_string()),
            comment_url: Some("https://x/c".to_string()),
            outcome: Some("completed".to_string()),
            error: None,
        })
        .expect("serializes");
        assert_eq!(done["receiptId"], 42);
        assert_eq!(done["status"], "done");
        assert_eq!(done["threadId"], "th-1");
        assert_eq!(done["commentUrl"], "https://x/c");
        assert!(done.get("comment_url").is_none());

        // Non-terminal with no URL: `commentUrl` is OMITTED (skip_serializing_if), not null.
        let running = serde_json::to_value(&StatusResponse {
            receipt_id: ReviewReceiptId::new(42).expect("receipt"),
            status: ReviewReceiptStatus::Received,
            thread_id: None,
            comment_url: None,
            outcome: None,
            error: None,
        })
        .expect("serializes");
        assert_eq!(running["status"], "received");
        assert!(running["threadId"].is_null());
        assert!(running["commentUrl"].is_null());
    }

    #[test]
    fn notify_request_response_wire_shape_and_unknown_fields() {
        let req: SendNotificationRequest = serde_json::from_value(serde_json::json!({
            "level": "error",
            "title": "Deploy failed",
            "body": "Build 42 failed",
            "url": "https://example.com/build/42",
            "projectId": "p1",
            "channelIds": ["desktop"]
        }))
        .expect("notify request deserializes");
        assert_eq!(req.level, Some(crate::model::NotificationLevel::Error));
        assert_eq!(req.project_id.as_deref(), Some("p1"));
        assert_eq!(req.channel_ids, vec!["desktop"]);

        let snake = serde_json::from_value::<SendNotificationRequest>(serde_json::json!({
            "title": "Deploy failed",
            "body": "Build 42 failed",
            "project_id": "p1"
        }))
        .expect_err("deny_unknown_fields rejects snake_case");
        assert!(snake.to_string().contains("unknown field"), "{snake}");

        let secret = serde_json::from_value::<SendNotificationRequest>(serde_json::json!({
            "title": "Deploy failed",
            "body": "Build 42 failed",
            "authorization": "Bearer secret"
        }))
        .expect_err("secret-looking extras are rejected");
        assert!(secret.to_string().contains("unknown field"), "{secret}");

        let response = crate::model::SendNotificationResponse {
            outbox_ids: vec![1, 2],
        };
        let v = serde_json::to_value(&response).expect("notify response serializes");
        assert_eq!(v["outboxIds"], serde_json::json!([1, 2]));
        assert!(v.get("outbox_ids").is_none());
    }

    // --- shared-struct round-trip goldens (AB#1044: the CLI client reuses these exact structs
    //     in the OPPOSITE direction; lock both sides so the one definition cannot drift) -------

    #[test]
    fn review_request_body_serializes_camel_case_and_omits_none() {
        // The CLI client BUILDS + serializes this struct as the POST body. Lock its outbound
        // shape: camelCase keys, the unset project ref omitted (not sent as `null`).
        let v = serde_json::to_value(&ReviewRequestBody {
            project_id: Some("p1".to_string()),
            repo: None,
            pr: 9,
            extra_args: String::new(),
            request_id: ExternalRequestId::parse("0123456789abcdef0123456789abcdef")
                .expect("request id"),
        })
        .expect("serializes");
        assert_eq!(v["projectId"], "p1");
        assert_eq!(v["pr"], 9);
        assert_eq!(v["extraArgs"], "");
        assert!(v.get("skillName").is_none());
        assert!(v.get("skillPath").is_none());
        assert!(v.get("commandTemplate").is_none());
        assert_eq!(v["requestId"], "0123456789abcdef0123456789abcdef");
        assert!(v.get("repo").is_none(), "unset repo is omitted");
        assert!(v.get("project_id").is_none(), "snake_case absent");
    }

    #[test]
    fn review_receipt_accepted_round_trips_from_camel_case() {
        // The CLI client DESERIALIZES the 202 body. Lock its inbound parse.
        let r: ReviewReceiptAccepted = serde_json::from_value(
            serde_json::json!({"receiptId": 42, "statusUrl": "http://127.0.0.1:8788/reviews/42"}),
        )
        .expect("deserializes camelCase");
        assert_eq!(r.receipt_id.get(), 42);
        assert_eq!(r.status_url, "http://127.0.0.1:8788/reviews/42");
    }

    #[test]
    fn status_response_round_trips_from_camel_case() {
        // Terminal-with-URL: the CLI client reads `commentUrl` (the `gh run watch` analogue).
        let done: StatusResponse = serde_json::from_value(
            serde_json::json!({"receiptId": 42, "status": "done", "threadId": "th-1", "commentUrl": "https://x/c"}),
        )
        .expect("deserializes done+url");
        assert_eq!(done.status, ReviewReceiptStatus::Done);
        assert_eq!(done.thread_id.as_deref(), Some("th-1"));
        assert_eq!(done.comment_url.as_deref(), Some("https://x/c"));

        // `commentUrl` absent (the omitted-on-non-completed wire) → None via `serde(default)`.
        let running: StatusResponse =
            serde_json::from_value(serde_json::json!({"receiptId": 42, "status": "queued"}))
                .expect("deserializes without commentUrl");
        assert_eq!(running.status, ReviewReceiptStatus::Queued);
        assert!(running.comment_url.is_none());
    }

    #[test]
    fn receipt_stream_frames_are_tagged_and_keep_receipt_identity() {
        let receipt_id = ReviewReceiptId::new(42).expect("receipt");
        let queued = ReceiptStreamEvent::Receipt {
            receipt: ReviewReceiptSnapshot {
                receipt_id,
                status: ReviewReceiptStatus::Queued,
                thread_id: None,
                comment_url: None,
                outcome: None,
                error: None,
            },
        };
        let queued = serde_json::to_value(queued).expect("serialize queued receipt");
        assert_eq!(queued["type"], "receipt");
        assert_eq!(queued["receipt"]["receiptId"], 42);
        assert_eq!(queued["receipt"]["status"], "queued");

        let review = ReceiptStreamEvent::Review {
            receipt_id,
            event: ReviewEvent::MessageDelta {
                project_id: "p1".to_string(),
                thread_id: "th-1".to_string(),
                item_id: "i1".to_string(),
                text: "delta".to_string(),
            },
        };
        let review = serde_json::to_value(review).expect("serialize linked review event");
        assert_eq!(review["type"], "review");
        assert_eq!(review["receiptId"], 42);
        assert_eq!(review["event"]["threadId"], "th-1");
    }

    // --- resolve_session (codex F1: durable Err must not fold into 404) ----------------------

    fn sample_info(thread: &str) -> SessionInfo {
        SessionInfo {
            project_id: "p1".to_string(),
            thread_id: thread.to_string(),
            turn_id: String::new(),
            pr_number: 7,
            skill_key: crate::model::SkillInvocation::skill_key("pr-review", ""),
            engine_kind: crate::model::EngineKind::Codex,
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

    // --- SSE stream helpers (AB#1072 / #1373): pure id-filter / terminal-detect / synthetic-
    //     terminal mapping. The handler wiring (subscribe→filter→close) is exercised manually. ----

    #[test]
    fn review_thread_id_some_for_session_scoped_none_for_dispatch() {
        let delta = ReviewEvent::MessageDelta {
            project_id: "p".to_string(),
            thread_id: "t9".to_string(),
            item_id: "i".to_string(),
            text: "x".to_string(),
        };
        assert_eq!(review_thread_id(&delta), Some("t9"));
        let term = ReviewEvent::TurnCompleted {
            project_id: "p".to_string(),
            thread_id: "t9".to_string(),
            status: "completed".to_string(),
            comment_url: None,
        };
        assert_eq!(review_thread_id(&term), Some("t9"));
        // The session-less DispatchError carries no thread id → never matches an id filter.
        let dispatch = ReviewEvent::DispatchError {
            project_id: "p".to_string(),
            message: "m".to_string(),
        };
        assert_eq!(review_thread_id(&dispatch), None);
    }

    #[test]
    fn is_terminal_review_true_only_for_matching_turn_completed() {
        let term = ReviewEvent::TurnCompleted {
            project_id: "p".to_string(),
            thread_id: "t9".to_string(),
            status: "completed".to_string(),
            comment_url: None,
        };
        assert!(is_terminal_review(&term, "t9"));
        assert!(!is_terminal_review(&term, "other")); // wrong id → keep streaming
        let delta = ReviewEvent::MessageDelta {
            project_id: "p".to_string(),
            thread_id: "t9".to_string(),
            item_id: "i".to_string(),
            text: "x".to_string(),
        };
        assert!(!is_terminal_review(&delta, "t9")); // a delta is not terminal
    }

    #[test]
    fn terminal_stream_event_maps_terminal_status_to_wire_string() {
        // Done + URL → a `completed` TurnCompleted carrying the comment URL (the reconnect payload).
        let done = terminal_stream_event(&SessionInfo {
            comment_url: Some("https://x/c".to_string()),
            ..sample_info("t9")
        });
        match done {
            StreamEvent::Review(ReviewEvent::TurnCompleted {
                thread_id,
                status,
                comment_url,
                ..
            }) => {
                assert_eq!(thread_id, "t9");
                assert_eq!(status, "completed");
                assert_eq!(comment_url.as_deref(), Some("https://x/c"));
            }
            _ => panic!("expected a Review TurnCompleted"),
        }
        // Failed + no URL → a `failed` TurnCompleted with no comment URL.
        let failed = terminal_stream_event(&SessionInfo {
            status: SessionStatus::Failed,
            ..sample_info("t9")
        });
        match failed {
            StreamEvent::Review(ReviewEvent::TurnCompleted {
                status,
                comment_url,
                ..
            }) => {
                assert_eq!(status, "failed");
                assert!(comment_url.is_none());
            }
            _ => panic!("expected a Review TurnCompleted"),
        }
    }

    // --- review_event_stream (the SSE stream state machine: id-filter, terminal-close, lag fail-
    //     safe). Covers the `handle_stream` orchestration the pure helpers couldn't reach. ---------

    fn delta(tid: &str, text: &str) -> StreamEvent {
        StreamEvent::Review(ReviewEvent::MessageDelta {
            project_id: "p".to_string(),
            thread_id: tid.to_string(),
            item_id: "i".to_string(),
            text: text.to_string(),
        })
    }

    fn turn_completed(tid: &str, status: &str) -> StreamEvent {
        StreamEvent::Review(ReviewEvent::TurnCompleted {
            project_id: "p".to_string(),
            thread_id: tid.to_string(),
            status: status.to_string(),
            comment_url: None,
        })
    }

    #[tokio::test]
    async fn review_event_stream_filters_by_id_and_closes_after_terminal() {
        use tokio::sync::broadcast;
        let (tx, rx) = broadcast::channel(16);
        // No lag on this happy path → on_lag must never fire.
        let s = review_event_stream(rx, "t1".to_string(), || {
            panic!("on_lag must not fire without a lag")
        });
        tokio::pin!(s);

        tx.send(delta("t1", "a")).unwrap();
        tx.send(delta("other", "b")).unwrap(); // different id → filtered out
        tx.send(StreamEvent::Action(crate::events::OutboxEvent::Error {
            operation: "claim".to_string(),
            message: "x".to_string(),
        }))
        .unwrap(); // action domain → filtered out of a per-review stream
        tx.send(turn_completed("t1", "completed")).unwrap();

        // First yielded: the t1 delta (the other-id delta + the action event are filtered out).
        match s.next().await {
            Some(StreamEvent::Review(ReviewEvent::MessageDelta {
                thread_id, text, ..
            })) => {
                assert_eq!(thread_id, "t1");
                assert_eq!(text, "a");
            }
            other => panic!("expected t1 delta, got {other:?}"),
        }
        // Then the t1 terminal.
        match s.next().await {
            Some(StreamEvent::Review(ReviewEvent::TurnCompleted { thread_id, .. })) => {
                assert_eq!(thread_id, "t1");
            }
            other => panic!("expected t1 terminal, got {other:?}"),
        }
        // Closes right after the terminal — no hang waiting for further events.
        assert!(s.next().await.is_none(), "stream closes after the terminal");
    }

    #[tokio::test]
    async fn review_event_stream_on_lag_closes_via_synthetic_terminal() {
        use tokio::sync::broadcast;
        // Tiny ring so the receiver lags; the terminal could be among the dropped batch.
        let (tx, rx) = broadcast::channel(2);
        let synth = turn_completed("t1", "failed");
        // Simulate "the session is now terminal": on_lag yields the synthetic close event.
        let s = review_event_stream(rx, "t1".to_string(), move || Some(synth.clone()));
        tokio::pin!(s);

        // 5 sends into a ring of 2, before any recv → the receiver is behind by 3 → first recv is
        // a `Lagged`, exercising the fail-safe (without it the stream would hang forever).
        for i in 0..5 {
            tx.send(delta("t1", &i.to_string())).unwrap();
        }

        match s.next().await {
            Some(StreamEvent::Review(ReviewEvent::TurnCompleted { status, .. })) => {
                assert_eq!(status, "failed");
            }
            other => panic!("expected the synthetic terminal on lag, got {other:?}"),
        }
        assert!(
            s.next().await.is_none(),
            "stream closes after the synthetic terminal (no infinite hang on a dropped terminal)"
        );
    }

    // --- HTTP request-level gate (AB#1072 / #1373 reviewer F2): bind the REAL router on a loopback
    //     port and drive it with reqwest, locking that the new `/reviews/:id/stream` route is
    //     registered AND enforces the same three-layer gate. The pure gate fns (host/origin/bearer)
    //     are unit-tested above; this is the only request-level test that proves they are WIRED into
    //     the SSE route (a regression that forgot the gate would surface here, not in a pure test).
    #[test]
    fn stream_route_is_registered_and_gated() {
        let app = tauri::test::mock_app();
        // The token gate reads the live config via `app.state::<Database>()`; manage an empty
        // in-memory DB so config load returns a default (empty token) rather than panicking.
        app.handle()
            .manage(crate::db::Database::open_in_memory().expect("open db"));

        tauri::async_runtime::block_on(async move {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind loopback");
            let port = listener.local_addr().expect("addr").port();
            let ctx = Arc::new(Ctx {
                app: app.handle().clone(),
                port,
                base_path: String::new(),
                remote_entrypoint_id: None,
            });
            let server = tauri::async_runtime::spawn(async move {
                let _ = axum::serve(listener, build_router(ctx).into_make_service()).await;
            });

            let url = format!("http://127.0.0.1:{port}/reviews/42/stream");
            let notify_url = format!("http://127.0.0.1:{port}/notifications");
            let client = reqwest::Client::new();

            // Route IS registered + the host gate runs on it: a non-loopback `Host` (DNS-rebinding
            // shape) is rejected 403 — a 404 here would mean the route was never wired.
            let forbidden = client
                .get(&url)
                .header("host", "evil.com")
                .send()
                .await
                .expect("send");
            assert_eq!(forbidden.status(), reqwest::StatusCode::FORBIDDEN);

            // The token gate runs on it too: a loopback request with no `Authorization` is rejected
            // 401 (the empty configured token fail-closes EVERY request — the charter default, now
            // proven to cover the SSE route).
            let unauthorized = client.get(&url).send().await.expect("send");
            assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

            let forbidden_notify = client
                .post(&notify_url)
                .header("host", "evil.com")
                .json(&serde_json::json!({"title":"t","body":"b"}))
                .send()
                .await
                .expect("send");
            assert_eq!(forbidden_notify.status(), reqwest::StatusCode::FORBIDDEN);

            let unauthorized_notify = client
                .post(&notify_url)
                .json(&serde_json::json!({"title":"t","body":"b"}))
                .send()
                .await
                .expect("send");
            assert_eq!(
                unauthorized_notify.status(),
                reqwest::StatusCode::UNAUTHORIZED
            );

            server.abort();
        });
    }

    #[test]
    fn remote_review_route_is_reachable_at_its_mounted_base_path() {
        let app = tauri::test::mock_app();
        let db = crate::db::Database::open_in_memory().expect("open db");
        app.handle().manage(db);
        let mut config = crate::config::service::load(app.handle()).expect("load default config");
        config.local_api_token = "remote-review-token-0123456789".to_string();
        crate::config::service::save(app.handle(), config).expect("persist token through service");

        tauri::async_runtime::block_on(async move {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind loopback");
            let port = listener.local_addr().expect("addr").port();
            let ctx = Arc::new(Ctx {
                app: app.handle().clone(),
                port,
                base_path: "/api".to_string(),
                remote_entrypoint_id: Some("remote-web".to_string()),
            });
            // Match the real Remote Access composition: LocalApi is nested at its configured
            // route, independent from the Terminal/UI route that serves the browser shell.
            let router = Router::new().nest("/api", build_router(ctx));
            let server = tauri::async_runtime::spawn(async move {
                let _ = axum::serve(listener, router.into_make_service()).await;
            });

            let response = reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/api/reviews"))
                .bearer_auth("remote-review-token-0123456789")
                .header(header::CONTENT_TYPE.as_str(), "application/json")
                // Authenticated + reachable, then deliberately fail at the typed body boundary.
                .body(r#"{"projectId":"p1"}"#)
                .send()
                .await
                .expect("send");
            assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
            let body = response.text().await.expect("body");
            assert!(body.contains("请求体解析失败"), "{body}");

            server.abort();
        });
    }

    #[test]
    fn receipt_query_failure_diagnostic_has_one_source() {
        let source = include_str!("local_api.rs");
        let diagnostic = ["查询 review receipt", " 失败"].concat();
        assert!(source.contains("const RECEIPT_QUERY_FAILED_MESSAGE"));
        assert_eq!(source.matches(&diagnostic).count(), 1);
    }
}
