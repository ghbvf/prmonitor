//! Browser HTTP/SSE surface for the terminal listener (#1445).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures::StreamExt;
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tauri::Manager;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use url::Url;

use crate::config::model::{
    terminal_auth_token_is_strong, Listener, ListenerAuthMode, ListenerKind, RemoteCapability,
};
use crate::config::service as config_service;
use crate::db::Database;
use crate::error::AppError;
use crate::events::{terminal_event_name, StreamEvent, TerminalEvent};
use crate::model::CreateSessionOpts;
use crate::state::AppState;

const MAX_BODY_BYTES: usize = 64 * 1024;
type HttpResult<T> = Result<T, Box<Response>>;
const CORS_ALLOW_HEADERS: &str = "authorization, content-type";
const CORS_ALLOW_METHODS: &str = "GET, POST, OPTIONS";

/// The built web SPA (`pnpm build:web` → repo-root `dist-web`), served over the terminal listener so
/// the Remote Web Terminal loads in a browser via the tunnel (#1504). Release embeds these bytes into
/// the binary (self-contained); debug reads `dist-web/` from disk at request time (no Rust recompile
/// needed — but re-run `pnpm build:web` after SPA source changes). `build.rs` writes a placeholder
/// `index.html` so the compile-time `#[folder]` resolves on fresh checkouts.
// `#[folder]` is resolved relative to `CARGO_MANIFEST_DIR` (src-tauri) by rust-embed, so `../dist-web`
// is the repo-root bundle. (No `$CARGO_MANIFEST_DIR` interpolation — that needs an extra feature.)
#[derive(RustEmbed)]
#[folder = "../dist-web"]
struct WebAssets;

const INDEX_HTML: &str = "index.html";

#[derive(Clone)]
pub(crate) struct Ctx<R: tauri::Runtime> {
    pub(crate) app: tauri::AppHandle<R>,
    pub(crate) port: u16,
    pub(crate) entrypoint_id: String,
    pub(crate) route_id: String,
    pub(crate) base_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionIdArgs {
    session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputArgs {
    session_id: String,
    data: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResizeArgs {
    session_id: String,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateArgs {
    #[serde(default)]
    opts: CreateSessionOpts,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Permission {
    Read,
    Write,
    Create,
    Admin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalCommand {
    ListSessions,
    CreateSession,
    Attach,
    Detach,
    CloseSession,
    SendInput,
    Resize,
    Status,
    StopDaemon,
}

const TERMINAL_HTTP_COMMANDS: &[(&str, TerminalCommand, Permission)] = &[
    (
        "list_terminal_sessions",
        TerminalCommand::ListSessions,
        Permission::Read,
    ),
    (
        "create_terminal_session",
        TerminalCommand::CreateSession,
        Permission::Create,
    ),
    ("attach_terminal", TerminalCommand::Attach, Permission::Read),
    ("detach_terminal", TerminalCommand::Detach, Permission::Read),
    // SEC-1: lifecycle destruction is gated at `Create` (≥ creation), not `Write` — a SIGKILL
    // bypasses the shell's cleanup traps, so the bar to terminate must be at least the bar to spawn.
    (
        "close_terminal_session",
        TerminalCommand::CloseSession,
        Permission::Create,
    ),
    (
        "send_terminal_input",
        TerminalCommand::SendInput,
        Permission::Write,
    ),
    (
        "resize_terminal",
        TerminalCommand::Resize,
        Permission::Write,
    ),
    (
        "get_terminal_status",
        TerminalCommand::Status,
        Permission::Read,
    ),
    (
        "stop_terminal_daemon",
        TerminalCommand::StopDaemon,
        Permission::Admin,
    ),
];

fn terminal_command(name: &str) -> Option<(TerminalCommand, Permission)> {
    TERMINAL_HTTP_COMMANDS
        .iter()
        .find(|(candidate, _, _)| *candidate == name)
        .map(|(_, command, permission)| (*command, *permission))
}

pub(crate) fn build_router<R: tauri::Runtime>(ctx: Arc<Ctx<R>>) -> Router {
    Router::new()
        .route(
            "/invoke/:command",
            post(handle_invoke::<R>).options(handle_options::<R>),
        )
        .route(
            "/events",
            get(handle_events::<R>).options(handle_options::<R>),
        )
        // Static SPA shell + assets (#1504). `.fallback` only catches paths NOT matched by the
        // explicit `/invoke/:command` + `/events` routes above, so the API surface is never shadowed.
        .fallback(handle_static::<R>)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(ctx)
}

/// Map a request path to the embedded asset key. `/` (or empty) → `index.html`; otherwise strip the
/// leading slash. Returns `None` for any path containing a `..` segment — a defense-in-depth traversal
/// guard: release is already safe (the embed is a compile-time key map, so `../` simply never matches),
/// but debug reads from disk via rust-embed, whose traversal guard exempts symlinks; this app-layer
/// filter removes that implicit dependency on a dependency's internals. The SPA fallback for an unknown
/// *client route* lives in `handle_static` (this stays a pure, table-testable normaliser).
fn resolve_asset(path: &str) -> Option<&str> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.split('/').any(|seg| seg == "..") {
        return None;
    }
    Some(if trimmed.is_empty() {
        INDEX_HTML
    } else {
        trimmed
    })
}

/// Host/Origin gate for the static SPA shell. Deliberately NO bearer/permission check: the shell
/// (`index.html` + `/assets/*`) carries no secrets and must load BEFORE the SPA can prompt for the
/// bearer token — that token then gates every `/invoke` + `/events` call (see `check_request`). This
/// is the closed upstream of the funnel (Host = DNS-rebind defense; Origin = same-origin gate); the
/// stateful API surface stays bearer-gated downstream.
fn check_static_request<R: tauri::Runtime>(
    ctx: &Ctx<R>,
    headers: &HeaderMap,
) -> HttpResult<(Listener, Vec<String>)> {
    let listener = terminal_listener_from_config(ctx)
        .ok_or_else(|| Box::new(error_response(StatusCode::FORBIDDEN, "终端路由未启用")))?;
    let public_urls = runtime_public_urls(ctx);
    if !host_allowed_with_public_urls(
        header_str(headers, "host").unwrap_or(""),
        ctx.port,
        &listener,
        &public_urls,
    ) {
        return Err(Box::new(error_response(
            StatusCode::FORBIDDEN,
            "Host 不被允许",
        )));
    }
    if !static_origin_allowed_with_public_urls(
        header_str(headers, "origin"),
        ctx.port,
        &listener,
        &public_urls,
    ) {
        return Err(Box::new(error_response(
            StatusCode::FORBIDDEN,
            "Origin 不被允许",
        )));
    }
    Ok((listener, public_urls))
}

async fn handle_static<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let (listener, public_urls) = match check_static_request(&ctx, &headers) {
        Ok(v) => v,
        Err(resp) => {
            // Observe Host/Origin rejections (e.g. DNS-rebind probes) on the unauthenticated static
            // path — the only signal we get, since there is no bearer subject to attribute.
            audit(
                &ctx.app,
                &ctx.entrypoint_id,
                "static_forbidden",
                false,
                &headers,
            );
            return *resp;
        }
    };
    let Some(requested) = resolve_asset(uri.path()) else {
        return error_response(StatusCode::NOT_FOUND, "资源不存在");
    };
    let (key, body) = match WebAssets::get(requested) {
        Some(content) => (requested, content.data),
        None => {
            // A missing asset-like path (has a file extension) is a real 404 — never serve index.html
            // as JS/CSS (a `text/html` body would break the SPA with a MIME/parse error). Only an
            // extensionless *client route* falls back to index.html for client-side routing.
            if std::path::Path::new(requested).extension().is_some() {
                return error_response(StatusCode::NOT_FOUND, "资源不存在");
            }
            match WebAssets::get(INDEX_HTML) {
                Some(content) => (INDEX_HTML, content.data),
                None => return error_response(StatusCode::NOT_FOUND, "资源不存在"),
            }
        }
    };
    let mime = mime_guess::from_path(key).first_or_octet_stream();
    let body = if key == INDEX_HTML {
        scope_index_html(body.into_owned(), &ctx.base_path)
    } else {
        body.into_owned()
    };
    let response = (
        [
            (header::CONTENT_TYPE, mime.as_ref()),
            // Defense-in-depth for a publicly-tunneled endpoint: no MIME sniffing, no framing.
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::X_FRAME_OPTIONS, "SAMEORIGIN"),
        ],
        body,
    )
        .into_response();
    // Mirror the API path: tag the allowed Origin so a configured cross-origin SPA host can fetch the
    // shell (same-origin navigation carries no Origin → with_cors is a no-op).
    with_cors(
        response,
        cors_origin(&headers, ctx.port, &listener, &public_urls),
    )
}

fn scope_index_html(body: Vec<u8>, base_path: &str) -> Vec<u8> {
    let base_path = normalise_base_path(base_path);
    let html = match String::from_utf8(body) {
        Ok(html) => html,
        Err(err) => return err.into_bytes(),
    };
    let html = if base_path.is_empty() {
        html
    } else {
        let attr_base_path = html_attr_escape(&base_path);
        html.replace("src=\"/", &format!("src=\"{attr_base_path}/"))
            .replace("href=\"/", &format!("href=\"{attr_base_path}/"))
    };
    inject_remote_base_path(html, &base_path).into_bytes()
}

fn normalise_base_path(path: &str) -> String {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("/{trimmed}")
    }
}

fn inject_remote_base_path(html: String, base_path: &str) -> String {
    let value = script_json_string(base_path);
    let script = format!("<script>window.__PRMONITOR_REMOTE_BASE_PATH__={value};</script>");
    if let Some(idx) = html.find("</head>") {
        let mut out = String::with_capacity(html.len() + script.len());
        out.push_str(&html[..idx]);
        out.push_str(&script);
        out.push_str(&html[idx..]);
        out
    } else {
        format!("{script}{html}")
    }
}

fn html_attr_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn script_json_string(value: &str) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "\"\"".to_string())
        .replace('/', "\\/")
}

fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response {
    match serde_json::to_string(body) {
        Ok(s) => (status, [(header::CONTENT_TYPE, "application/json")], s).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn empty_response() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    json_response(
        status,
        &ErrorBody {
            message: message.into(),
        },
    )
}

fn with_cors(mut response: Response, origin: Option<&str>) -> Response {
    let Some(origin) = origin.and_then(|o| HeaderValue::from_str(o).ok()) else {
        return response;
    };
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(CORS_ALLOW_HEADERS),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static(CORS_ALLOW_METHODS),
    );
    headers.insert(header::VARY, HeaderValue::from_static("origin"));
    response
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn terminal_listener_from_config<R: tauri::Runtime>(ctx: &Ctx<R>) -> Option<Listener> {
    let cfg = config_service::load(&ctx.app).ok()?;
    let entrypoint = cfg
        .remote_access
        .entrypoints
        .into_iter()
        .find(|entrypoint| entrypoint.id == ctx.entrypoint_id && entrypoint.enabled)?;
    let route = entrypoint.routes.into_iter().find(|route| {
        route.id == ctx.route_id
            && route.enabled
            && matches!(route.capability, RemoteCapability::Terminal)
    })?;
    Some(Listener {
        id: route.id,
        name: route.name,
        kind: ListenerKind::Terminal,
        bind_host: entrypoint.bind_host,
        port: entrypoint.port,
        enabled: true,
        auth: ListenerAuthMode::Bearer,
        auth_token: route.auth_token,
        terminal_read: route.terminal_read,
        terminal_write: route.terminal_write,
        terminal_create: route.terminal_create,
        terminal_admin: route.terminal_admin,
        allowed_origins: entrypoint.allowed_origins,
        public_url: String::new(),
    })
}

fn host_without_port(host: &str) -> &str {
    host.rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host)
        .trim_matches(['[', ']'])
}

fn listener_public_host(listener: &Listener) -> Option<String> {
    public_url_host(&listener.public_url)
}

fn public_url_host(public_url: &str) -> Option<String> {
    Url::parse(public_url.trim())
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
}

fn loopback_host_allowed(host: &str, port: u16) -> bool {
    let allowed = [
        "127.0.0.1".to_string(),
        format!("127.0.0.1:{port}"),
        "localhost".to_string(),
        format!("localhost:{port}"),
        "[::1]".to_string(),
        format!("[::1]:{port}"),
    ];
    allowed.iter().any(|h| h.eq_ignore_ascii_case(host))
}

#[cfg(test)]
fn host_allowed(host: &str, port: u16, listener: &Listener) -> bool {
    host_allowed_with_public_urls(host, port, listener, &[])
}

fn host_allowed_with_public_urls(
    host: &str,
    port: u16,
    listener: &Listener,
    public_urls: &[String],
) -> bool {
    if loopback_host_allowed(host, port) {
        return true;
    }
    let host = host_without_port(host);
    listener_public_host(listener)
        .is_some_and(|public_host| host.eq_ignore_ascii_case(&public_host))
        || public_urls.iter().any(|url| {
            public_url_host(url).is_some_and(|public_host| host.eq_ignore_ascii_case(&public_host))
        })
}

#[cfg(test)]
fn origin_allowed(
    origin: Option<&str>,
    sec_fetch_site: Option<&str>,
    port: u16,
    listener: &Listener,
) -> bool {
    origin_allowed_with_public_urls(origin, sec_fetch_site, port, listener, &[])
}

fn origin_allowed_with_public_urls(
    origin: Option<&str>,
    sec_fetch_site: Option<&str>,
    port: u16,
    listener: &Listener,
    public_urls: &[String],
) -> bool {
    if matches!(sec_fetch_site, Some("cross-site")) {
        return origin
            .is_some_and(|o| origin_matches_with_public_urls(o, port, listener, public_urls));
    }
    match origin {
        None => true,
        Some(o) => origin_matches_with_public_urls(o, port, listener, public_urls),
    }
}

fn static_origin_allowed_with_public_urls(
    origin: Option<&str>,
    port: u16,
    listener: &Listener,
    public_urls: &[String],
) -> bool {
    origin.is_none_or(|o| origin_matches_with_public_urls(o, port, listener, public_urls))
}

fn origin_matches_with_public_urls(
    origin: &str,
    port: u16,
    listener: &Listener,
    public_urls: &[String],
) -> bool {
    let loopback = [
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
        format!("http://[::1]:{port}"),
        "http://127.0.0.1".to_string(),
        "http://localhost".to_string(),
        "http://[::1]".to_string(),
    ];
    loopback.iter().any(|o| o.eq_ignore_ascii_case(origin))
        || listener
            .allowed_origins
            .iter()
            .any(|o| o.eq_ignore_ascii_case(origin))
        || (!listener.public_url.trim().is_empty()
            && listener
                .public_url
                .trim()
                .trim_end_matches('/')
                .eq_ignore_ascii_case(origin.trim_end_matches('/')))
        || public_urls.iter().any(|url| {
            !url.trim().is_empty()
                && url
                    .trim()
                    .trim_end_matches('/')
                    .eq_ignore_ascii_case(origin.trim_end_matches('/'))
        })
}

fn cors_origin<'a>(
    headers: &'a HeaderMap,
    port: u16,
    listener: &Listener,
    public_urls: &[String],
) -> Option<&'a str> {
    let origin = header_str(headers, "origin")?;
    origin_matches_with_public_urls(origin, port, listener, public_urls).then_some(origin)
}

fn verify_bearer(token: &str, header: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let Some(presented) = header
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, value)| value)
    else {
        return false;
    };
    presented.as_bytes().ct_eq(token.as_bytes()).into()
}

fn permission_allowed(listener: &Listener, permission: Permission) -> bool {
    match permission {
        Permission::Read => listener.terminal_read,
        Permission::Write => listener.terminal_write,
        Permission::Create => listener.terminal_create,
        Permission::Admin => listener.terminal_admin,
    }
}

fn runtime_public_urls<R: tauri::Runtime>(ctx: &Ctx<R>) -> Vec<String> {
    ctx.app
        .try_state::<AppState>()
        .map(|state| state.remote.public_urls_for_entrypoint(&ctx.entrypoint_id))
        .unwrap_or_default()
}

fn check_request<R: tauri::Runtime>(
    ctx: &Ctx<R>,
    headers: &HeaderMap,
    permission: Permission,
) -> HttpResult<Listener> {
    let listener = terminal_listener_from_config(ctx)
        .ok_or_else(|| Box::new(error_response(StatusCode::FORBIDDEN, "终端路由未启用")))?;
    let public_urls = runtime_public_urls(ctx);
    if !host_allowed_with_public_urls(
        header_str(headers, "host").unwrap_or(""),
        ctx.port,
        &listener,
        &public_urls,
    ) {
        return Err(Box::new(error_response(
            StatusCode::FORBIDDEN,
            "Host 不被允许",
        )));
    }
    if !origin_allowed_with_public_urls(
        header_str(headers, "origin"),
        header_str(headers, "sec-fetch-site"),
        ctx.port,
        &listener,
        &public_urls,
    ) {
        return Err(Box::new(error_response(
            StatusCode::FORBIDDEN,
            "Origin 不被允许",
        )));
    }
    if !terminal_auth_token_is_strong(&listener.auth_token) {
        return Err(Box::new(with_cors(
            error_response(StatusCode::UNAUTHORIZED, "终端监听器 token 强度不足"),
            cors_origin(headers, ctx.port, &listener, &public_urls),
        )));
    }
    if !verify_bearer(
        listener.auth_token.trim(),
        header_str(headers, "authorization").unwrap_or(""),
    ) {
        return Err(Box::new(with_cors(
            error_response(StatusCode::UNAUTHORIZED, "未授权"),
            cors_origin(headers, ctx.port, &listener, &public_urls),
        )));
    }
    if !permission_allowed(&listener, permission) {
        return Err(Box::new(with_cors(
            error_response(StatusCode::FORBIDDEN, "权限不足"),
            cors_origin(headers, ctx.port, &listener, &public_urls),
        )));
    }
    Ok(listener)
}

fn parse_args<T: for<'de> Deserialize<'de>>(body: Bytes) -> HttpResult<T> {
    serde_json::from_slice(body.as_ref()).map_err(|e| {
        Box::new(error_response(
            StatusCode::BAD_REQUEST,
            format!("请求体解析失败: {e}"),
        ))
    })
}

async fn handle_invoke<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    headers: HeaderMap,
    Path(command): Path<String>,
    body: Bytes,
) -> Response {
    let Some((terminal_command, permission)) = terminal_command(&command) else {
        return error_response(StatusCode::NOT_FOUND, "未知终端命令");
    };
    let listener = match check_request(&ctx, &headers, permission) {
        Ok(l) => l,
        Err(resp) => return *resp,
    };
    let state = ctx.app.state::<AppState>();
    let result = match terminal_command {
        TerminalCommand::ListSessions => {
            crate::terminal::commands::list_terminal_sessions_inner(&ctx.app, &state)
                .await
                .map(|v| json_response(StatusCode::OK, &v))
        }
        TerminalCommand::CreateSession => match parse_args::<CreateArgs>(body) {
            Ok(args) => crate::terminal::commands::create_terminal_session_inner(
                &ctx.app, &state, args.opts,
            )
            .await
            .map(|v| json_response(StatusCode::OK, &v)),
            Err(resp) => return *resp,
        },
        TerminalCommand::Attach => match parse_args::<SessionIdArgs>(body) {
            Ok(args) => {
                crate::terminal::commands::attach_terminal_inner(&ctx.app, &state, &args.session_id)
                    .await
                    .map(|_| empty_response())
            }
            Err(resp) => return *resp,
        },
        TerminalCommand::Detach => match parse_args::<SessionIdArgs>(body) {
            Ok(args) => {
                crate::terminal::commands::detach_terminal_inner(&ctx.app, &state, &args.session_id)
                    .await
                    .map(|_| empty_response())
            }
            Err(resp) => return *resp,
        },
        TerminalCommand::CloseSession => match parse_args::<SessionIdArgs>(body) {
            Ok(args) => crate::terminal::commands::close_terminal_session_inner(
                &ctx.app,
                &state,
                &args.session_id,
            )
            .await
            .map(|_| empty_response()),
            Err(resp) => return *resp,
        },
        TerminalCommand::SendInput => match parse_args::<InputArgs>(body) {
            Ok(args) => crate::terminal::commands::send_terminal_input_inner(
                &ctx.app,
                &state,
                &args.session_id,
                &args.data,
            )
            .await
            .map(|_| empty_response()),
            Err(resp) => return *resp,
        },
        TerminalCommand::Resize => match parse_args::<ResizeArgs>(body) {
            Ok(args) => crate::terminal::commands::resize_terminal_inner(
                &ctx.app,
                &state,
                &args.session_id,
                args.cols,
                args.rows,
            )
            .await
            .map(|_| empty_response()),
            Err(resp) => return *resp,
        },
        TerminalCommand::Status => Ok(json_response(
            StatusCode::OK,
            &crate::terminal::commands::get_terminal_status_inner(&ctx.app, &state).await,
        )),
        TerminalCommand::StopDaemon => Ok(json_response(
            StatusCode::OK,
            &crate::terminal::commands::stop_terminal_daemon_inner(&state),
        )),
    };
    let ok = result.is_ok();
    audit(&ctx.app, &listener.id, &command, ok, &headers);
    let response =
        result.unwrap_or_else(|e: AppError| error_response(StatusCode::BAD_REQUEST, e.message));
    let public_urls = runtime_public_urls(&ctx);
    with_cors(
        response,
        cors_origin(&headers, ctx.port, &listener, &public_urls),
    )
}

// SEC-3 (doc only, deferred): this SSE stream is SESSION-AGNOSTIC. A bearer holder with
// `terminal_read` receives `TerminalEvent`s (including WebPty `Output`) for ALL subscribed
// sessions on this listener, not just ones it opened — there is no per-session authorization on
// the event bus. Server-side per-session filtering is a tracked follow-up; no behavior change here
// (the user deferred the fix). Mitigation today: the listener is loopback-only + bearer-gated.
async fn handle_events<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if query_topic(&uri).as_deref() != Some(terminal_event_name()) {
        return error_response(StatusCode::NOT_FOUND, "未知事件 topic");
    }
    let listener = match check_request(&ctx, &headers, Permission::Read) {
        Ok(l) => l,
        Err(resp) => return *resp,
    };
    let public_urls = runtime_public_urls(&ctx);
    let rx = ctx.app.state::<AppState>().stream.subscribe();
    let stream_ctx = ctx.clone();
    let stream_headers = headers.clone();
    let stream = BroadcastStream::new(rx)
        .flat_map(move |item| {
            let outs: Vec<TerminalSseItem> = match item {
                Ok(StreamEvent::Terminal(ev)) => {
                    // Re-validate read permission before EACH frame — a mid-stream revocation closes
                    // the stream (the `End` sentinel is consumed by `take_while` below).
                    if check_request(&stream_ctx, &stream_headers, Permission::Read).is_err() {
                        vec![TerminalSseItem::End]
                    } else {
                        vec![TerminalSseItem::Frame(sse_data(&ev))]
                    }
                }
                Ok(_) => Vec::new(),
                Err(BroadcastStreamRecvError::Lagged(n)) => {
                    // F4: a dropped frame is UNRECOVERABLE for a WebPty byte-delta stream (no
                    // self-healing snapshot). Fail fast — emit ONE error frame, THEN close, so the
                    // client's transport `onClosed` + error banner drive a re-attach (which
                    // re-subscribes and replays the backend scrollback ring). A full sequence/replay
                    // protocol is the deferred follow-up.
                    eprintln!("terminal SSE 滞后，丢帧 {n}，关闭以触发重连");
                    vec![
                        TerminalSseItem::Frame(sse_data(&lag_error_event())),
                        TerminalSseItem::End,
                    ]
                }
            };
            futures::stream::iter(outs)
        })
        // Stop AT the first `End` (the lag error frame precedes it, so it is delivered first).
        .take_while(|item| futures::future::ready(!matches!(item, TerminalSseItem::End)))
        .map(|item| match item {
            TerminalSseItem::Frame(frame) => frame,
            // `End` terminates the stream in `take_while` above and never reaches here.
            TerminalSseItem::End => unreachable!("End is consumed by take_while"),
        });
    with_cors(
        Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response(),
        cors_origin(&headers, ctx.port, &listener, &public_urls),
    )
}

async fn handle_options<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    headers: HeaderMap,
) -> Response {
    let Some(listener) = terminal_listener_from_config(&ctx) else {
        return error_response(StatusCode::FORBIDDEN, "终端路由未启用");
    };
    let public_urls = runtime_public_urls(&ctx);
    if !host_allowed_with_public_urls(
        header_str(&headers, "host").unwrap_or(""),
        ctx.port,
        &listener,
        &public_urls,
    ) || !origin_allowed_with_public_urls(
        header_str(&headers, "origin"),
        header_str(&headers, "sec-fetch-site"),
        ctx.port,
        &listener,
        &public_urls,
    ) {
        return error_response(StatusCode::FORBIDDEN, "Origin 不被允许");
    }
    with_cors(
        empty_response(),
        cors_origin(&headers, ctx.port, &listener, &public_urls),
    )
}

fn query_topic(uri: &Uri) -> Option<String> {
    uri.query()?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "topic").then(|| value.replace("%3A", ":").replace("%3a", ":"))
    })
}

fn sse_data(event: &TerminalEvent) -> Result<SseEvent, std::convert::Infallible> {
    let payload = serde_json::to_string(event).unwrap_or_else(|_| {
        r#"{"kind":"error","message":"terminal stream serialize error"}"#.to_string()
    });
    Ok(SseEvent::default().data(payload))
}

/// One mapped output for the terminal SSE stream: a frame to send, or a terminal `End` sentinel
/// that closes the stream (consumed by `take_while` in `handle_events`). The sentinel lets the lag
/// branch emit ONE error frame THEN close (F4), and the reauth branch close, in one pipeline.
enum TerminalSseItem {
    Frame(Result<SseEvent, std::convert::Infallible>),
    End,
}

/// The one-shot connection-level error frame sent right before the terminal SSE closes on bus lag
/// (F4): the client surfaces it as a banner + reconnects (which replays the backend scrollback).
fn lag_error_event() -> TerminalEvent {
    TerminalEvent::Error {
        session_id: None,
        message: "终端流滞后，已丢帧——请重新连接以重放回放".to_string(),
    }
}

fn audit<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    listener_id: &str,
    action: &str,
    ok: bool,
    headers: &HeaderMap,
) {
    let Some(db) = app.try_state::<Database>() else {
        return;
    };
    let host = header_str(headers, "host").unwrap_or("").to_string();
    let origin = header_str(headers, "origin").unwrap_or("").to_string();
    let _ = db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO terminal_audit (ts_ms, listener_id, action, ok, host, origin)
             VALUES (CAST(strftime('%s','now') AS INTEGER) * 1000, ?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![listener_id, action, ok, host, origin],
        )?;
        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::{AppConfig, ListenerAuthMode, ListenerKind};
    use std::collections::BTreeSet;
    use std::time::Duration;

    fn listener() -> Listener {
        Listener {
            id: "term".to_string(),
            name: "Terminal".to_string(),
            kind: ListenerKind::Terminal,
            bind_host: "127.0.0.1".to_string(),
            port: 9100,
            enabled: true,
            auth: ListenerAuthMode::Bearer,
            public_url: "https://term.example.com".to_string(),
            allowed_origins: vec!["https://mobile.example.com".to_string()],
            auth_token: "terminal-token-0123456789".to_string(),
            terminal_read: true,
            terminal_write: true,
            terminal_create: false,
            terminal_admin: false,
        }
    }

    fn terminal_http_command_names() -> BTreeSet<String> {
        TERMINAL_HTTP_COMMANDS
            .iter()
            .map(|(name, _, _)| (*name).to_string())
            .collect()
    }

    fn terminal_tauri_command_names() -> BTreeSet<String> {
        let lib_rs = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
            .expect("read lib.rs");
        lib_rs
            .lines()
            .filter_map(|line| {
                let (_, command) = line.split_once("terminal::commands::")?;
                Some(command.trim().trim_end_matches(',').to_string())
            })
            .collect()
    }

    fn terminal_typescript_api_command_names() -> BTreeSet<String> {
        let api_ts = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../src/terminal/api.ts"
        ))
        .expect("read src/terminal/api.ts");
        let (_, after_start) = api_ts
            .split_once("export const TERMINAL_COMMANDS = [")
            .expect("TERMINAL_COMMANDS start");
        let (commands_block, _) = after_start
            .split_once("] as const;")
            .expect("TERMINAL_COMMANDS end");
        commands_block
            .lines()
            .filter_map(|line| {
                let command = line.trim().trim_end_matches(',').trim_matches('"');
                (!command.is_empty()).then(|| command.to_string())
            })
            .collect()
    }

    #[test]
    fn terminal_host_allows_loopback_and_public_host() {
        let l = listener();
        assert!(host_allowed("127.0.0.1:9100", 9100, &l));
        assert!(host_allowed("term.example.com", 9100, &l));
        assert!(host_allowed("term.example.com:443", 9100, &l));
        assert!(!host_allowed("evil.example.com", 9100, &l));
    }

    #[test]
    fn terminal_origin_allows_configured_origins() {
        let l = listener();
        assert!(origin_allowed(
            Some("https://mobile.example.com"),
            None,
            9100,
            &l
        ));
        assert!(origin_allowed(
            Some("https://term.example.com"),
            None,
            9100,
            &l
        ));
        assert!(!origin_allowed(
            Some("https://evil.example.com"),
            None,
            9100,
            &l
        ));
    }

    #[test]
    fn terminal_permission_flags_are_action_scoped() {
        let l = listener();
        assert!(permission_allowed(&l, Permission::Read));
        assert!(permission_allowed(&l, Permission::Write));
        assert!(!permission_allowed(&l, Permission::Create));
        assert!(!permission_allowed(&l, Permission::Admin));
    }

    #[test]
    fn terminal_http_command_table_is_single_source_for_permissions() {
        let names: std::collections::HashSet<&str> = TERMINAL_HTTP_COMMANDS
            .iter()
            .map(|(name, _, _)| *name)
            .collect();
        assert_eq!(names.len(), TERMINAL_HTTP_COMMANDS.len());
        assert_eq!(
            terminal_command("stop_terminal_daemon"),
            Some((TerminalCommand::StopDaemon, Permission::Admin))
        );
        // SEC-1: closing a session (lifecycle destruction) is gated at `Create`, not `Write`.
        assert_eq!(
            terminal_command("close_terminal_session"),
            Some((TerminalCommand::CloseSession, Permission::Create))
        );
        assert_eq!(
            terminal_command("send_terminal_input"),
            Some((TerminalCommand::SendInput, Permission::Write))
        );
        assert!(terminal_command("not_terminal").is_none());
    }

    #[test]
    fn terminal_http_commands_match_tauri_and_typescript_contracts() {
        let http = terminal_http_command_names();
        assert_eq!(http, terminal_tauri_command_names());
        assert_eq!(http, terminal_typescript_api_command_names());
    }

    #[test]
    fn terminal_gate_allows_runtime_tunnel_public_url() {
        let l = listener();
        let public_urls = vec!["https://abc.trycloudflare.com".to_string()];
        assert!(host_allowed_with_public_urls(
            "abc.trycloudflare.com",
            9100,
            &l,
            &public_urls
        ));
        assert!(origin_matches_with_public_urls(
            "https://abc.trycloudflare.com",
            9100,
            &l,
            &public_urls
        ));
        assert!(!host_allowed_with_public_urls(
            "evil.trycloudflare.com",
            9100,
            &l,
            &public_urls
        ));
    }

    fn persist_listener<R: tauri::Runtime>(app: &tauri::AppHandle<R>, listener: Listener) {
        let config = AppConfig {
            remote_access: crate::config::model::RemoteAccessConfig {
                entrypoints: vec![crate::config::model::RemoteEntrypoint {
                    id: listener.id.clone(),
                    name: listener.name.clone(),
                    bind_host: listener.bind_host.clone(),
                    port: listener.port,
                    enabled: listener.enabled,
                    allowed_origins: listener.allowed_origins.clone(),
                    routes: vec![crate::config::model::RemoteRoute {
                        id: "terminal".to_string(),
                        name: "Terminal".to_string(),
                        path: "/terminal".to_string(),
                        capability: crate::config::model::RemoteCapability::Terminal,
                        enabled: true,
                        auth_token: listener.auth_token.clone(),
                        terminal_read: listener.terminal_read,
                        terminal_write: listener.terminal_write,
                        terminal_create: listener.terminal_create,
                        terminal_admin: listener.terminal_admin,
                    }],
                    ..crate::config::model::RemoteEntrypoint::default()
                }],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        };
        let Some(db) = app.try_state::<Database>() else {
            let db = Database::open_in_memory().expect("open db");
            crate::config::service::persist_db(&db, &config).expect("persist config");
            app.manage(db);
            return;
        };
        crate::config::service::persist_db(&db, &config).expect("persist config");
    }

    #[test]
    fn terminal_routes_are_registered_gated_and_cors_enabled() {
        let app = tauri::test::mock_app();
        tauri::async_runtime::block_on(async move {
            let socket = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind loopback");
            let port = socket.local_addr().expect("addr").port();
            let mut l = listener();
            l.port = port;
            l.terminal_write = false;
            persist_listener(app.handle(), l);
            let ctx = Arc::new(Ctx {
                app: app.handle().clone(),
                port,
                entrypoint_id: "term".to_string(),
                route_id: "terminal".to_string(),
                base_path: String::new(),
            });
            let server = tauri::async_runtime::spawn(async move {
                let _ = axum::serve(socket, build_router(ctx).into_make_service()).await;
            });
            let client = reqwest::Client::new();
            let base = format!("http://127.0.0.1:{port}");

            let bad_host = client
                .get(format!("{base}/events?topic=terminal:event"))
                .header("host", "evil.example.com")
                .send()
                .await
                .expect("send");
            assert_eq!(bad_host.status(), reqwest::StatusCode::FORBIDDEN);

            let preflight = client
                .request(reqwest::Method::OPTIONS, format!("{base}/events"))
                .header("origin", "https://mobile.example.com")
                .header("access-control-request-method", "GET")
                .header("access-control-request-headers", "authorization")
                .send()
                .await
                .expect("send");
            assert_eq!(preflight.status(), reqwest::StatusCode::NO_CONTENT);
            assert_eq!(
                preflight
                    .headers()
                    .get("access-control-allow-origin")
                    .and_then(|v| v.to_str().ok()),
                Some("https://mobile.example.com")
            );
            assert_eq!(
                preflight
                    .headers()
                    .get("access-control-allow-headers")
                    .and_then(|v| v.to_str().ok()),
                Some(CORS_ALLOW_HEADERS)
            );

            let unauth = client
                .get(format!("{base}/events?topic=terminal:event"))
                .header("origin", "https://mobile.example.com")
                .send()
                .await
                .expect("send");
            assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);
            assert_eq!(
                unauth
                    .headers()
                    .get("access-control-allow-origin")
                    .and_then(|v| v.to_str().ok()),
                Some("https://mobile.example.com")
            );
            let body = unauth.text().await.expect("body");
            assert!(body.contains("未授权"), "{body}");

            let forbidden = client
                .post(format!("{base}/invoke/send_terminal_input"))
                .header("authorization", "Bearer terminal-token-0123456789")
                .json(&serde_json::json!({"sessionId":"s1","data":"x"}))
                .send()
                .await
                .expect("send");
            assert_eq!(forbidden.status(), reqwest::StatusCode::FORBIDDEN);

            let unknown = client
                .post(format!("{base}/invoke/not_terminal"))
                .send()
                .await
                .expect("send");
            assert_eq!(unknown.status(), reqwest::StatusCode::NOT_FOUND);
            server.abort();
        });
    }

    #[test]
    fn terminal_invoke_get_status_dispatches_successfully() {
        let app = tauri::test::mock_app();
        app.manage(AppState::default());
        tauri::async_runtime::block_on(async move {
            let socket = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind loopback");
            let port = socket.local_addr().expect("addr").port();
            let mut l = listener();
            l.port = port;
            persist_listener(app.handle(), l);
            let ctx = Arc::new(Ctx {
                app: app.handle().clone(),
                port,
                entrypoint_id: "term".to_string(),
                route_id: "terminal".to_string(),
                base_path: String::new(),
            });
            let server = tauri::async_runtime::spawn(async move {
                let _ = axum::serve(socket, build_router(ctx).into_make_service()).await;
            });

            let resp = reqwest::Client::new()
                .post(format!(
                    "http://127.0.0.1:{port}/invoke/get_terminal_status"
                ))
                .header("authorization", "Bearer terminal-token-0123456789")
                .json(&serde_json::json!({}))
                .send()
                .await
                .expect("send");
            assert_eq!(resp.status(), reqwest::StatusCode::OK);
            let body: serde_json::Value = resp.json().await.expect("json");
            assert!(body.get("available").is_some(), "{body}");
            assert!(body.get("desiredRunning").is_some(), "{body}");
            server.abort();
        });
    }

    #[test]
    fn terminal_runtime_rejects_short_token_loaded_leniently() {
        let app = tauri::test::mock_app();
        tauri::async_runtime::block_on(async move {
            let socket = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind loopback");
            let port = socket.local_addr().expect("addr").port();
            let mut l = listener();
            l.port = port;
            l.auth_token = "short".to_string();
            persist_listener(app.handle(), l);
            let ctx = Arc::new(Ctx {
                app: app.handle().clone(),
                port,
                entrypoint_id: "term".to_string(),
                route_id: "terminal".to_string(),
                base_path: String::new(),
            });
            let server = tauri::async_runtime::spawn(async move {
                let _ = axum::serve(socket, build_router(ctx).into_make_service()).await;
            });
            let resp = reqwest::Client::new()
                .get(format!(
                    "http://127.0.0.1:{port}/events?topic=terminal:event"
                ))
                .header("authorization", "Bearer short")
                .send()
                .await
                .expect("send");
            assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
            let body = resp.text().await.expect("body");
            assert!(body.contains("强度不足"), "{body}");
            server.abort();
        });
    }

    #[test]
    fn terminal_event_stream_revalidates_read_permission_before_each_frame() {
        let app = tauri::test::mock_app();
        app.manage(AppState::default());
        tauri::async_runtime::block_on(async move {
            let socket = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind loopback");
            let port = socket.local_addr().expect("addr").port();
            let mut l = listener();
            l.port = port;
            persist_listener(app.handle(), l.clone());
            let ctx = Arc::new(Ctx {
                app: app.handle().clone(),
                port,
                entrypoint_id: "term".to_string(),
                route_id: "terminal".to_string(),
                base_path: String::new(),
            });
            let server = tauri::async_runtime::spawn(async move {
                let _ = axum::serve(socket, build_router(ctx).into_make_service()).await;
            });
            let client = reqwest::Client::new();
            let mut response = client
                .get(format!(
                    "http://127.0.0.1:{port}/events?topic=terminal:event"
                ))
                .header("authorization", "Bearer terminal-token-0123456789")
                .send()
                .await
                .expect("send");

            crate::stream::emit(
                app.handle(),
                StreamEvent::Terminal(TerminalEvent::ScreenUpdate {
                    session_id: "s1".to_string(),
                    cols: 80,
                    rows: 24,
                    contents: "first".to_string(),
                    cursor_row: None,
                    cursor_col: None,
                }),
            );
            let first = tokio::time::timeout(Duration::from_secs(1), response.chunk())
                .await
                .expect("first frame timeout")
                .expect("first frame")
                .expect("first frame bytes");
            assert!(
                String::from_utf8_lossy(&first).contains("screenUpdate"),
                "first frame should be forwarded before revocation"
            );

            l.terminal_read = false;
            persist_listener(app.handle(), l);
            crate::stream::emit(
                app.handle(),
                StreamEvent::Terminal(TerminalEvent::ScreenUpdate {
                    session_id: "s1".to_string(),
                    cols: 80,
                    rows: 24,
                    contents: "second".to_string(),
                    cursor_row: None,
                    cursor_col: None,
                }),
            );
            let closed = tokio::time::timeout(Duration::from_secs(1), response.chunk())
                .await
                .expect("stream close timeout")
                .expect("stream close result");
            assert!(closed.is_none(), "revoked read permission should close SSE");
            server.abort();
        });
    }

    #[test]
    fn resolve_asset_normalises_paths() {
        // root → index.html
        assert_eq!(resolve_asset(""), Some(INDEX_HTML));
        assert_eq!(resolve_asset("/"), Some(INDEX_HTML));
        // real asset → leading slash stripped, passthrough
        assert_eq!(
            resolve_asset("/assets/app-abc123.js"),
            Some("assets/app-abc123.js")
        );
        assert_eq!(resolve_asset("/index.html"), Some("index.html"));
        // client-side deep link → passthrough here; handle_static falls back to index.html on miss
        assert_eq!(resolve_asset("/settings/tokens"), Some("settings/tokens"));
        // traversal guard: any `..` segment → None (handle_static maps to 404), never escapes dist-web
        assert_eq!(resolve_asset("/../../etc/passwd"), None);
        assert_eq!(resolve_asset("/assets/../../secret"), None);
    }

    #[test]
    fn scoped_index_html_rewrites_assets_and_injects_remote_base_path() {
        let html = br#"<!doctype html><html><head><script type="module" src="/assets/app.js"></script><link rel="stylesheet" href="/assets/app.css"></head><body></body></html>"#;
        let scoped = String::from_utf8(scope_index_html(html.to_vec(), "terminal/")).unwrap();
        assert!(scoped.contains("window.__PRMONITOR_REMOTE_BASE_PATH__=\"\\/terminal\""));
        assert!(scoped.contains("src=\"/terminal/assets/app.js\""));
        assert!(scoped.contains("href=\"/terminal/assets/app.css\""));
    }

    #[test]
    fn scoped_index_html_escapes_malicious_base_path_contexts() {
        let html = br#"<!doctype html><html><head><script type="module" src="/assets/app.js"></script><link rel="stylesheet" href="/assets/app.css"></head><body></body></html>"#;
        let scoped = String::from_utf8(scope_index_html(
            html.to_vec(),
            r#"/terminal"></script><script>alert(1)</script>"#,
        ))
        .unwrap();
        assert!(!scoped.contains(r#"</script><script>alert(1)</script>"#));
        assert!(scoped.contains("&quot;&gt;&lt;/script&gt;&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(scoped.contains(r#"window.__PRMONITOR_REMOTE_BASE_PATH__="\/terminal\""#));
        assert!(scoped.contains(r#"<\/script><script>alert(1)<\/script>"#));
    }

    #[test]
    fn terminal_nested_route_serves_scoped_shell_without_bearer() {
        let app = tauri::test::mock_app();
        tauri::async_runtime::block_on(async move {
            let socket = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind loopback");
            let port = socket.local_addr().expect("addr").port();
            let mut l = listener();
            l.port = port;
            persist_listener(app.handle(), l);
            let ctx = Arc::new(Ctx {
                app: app.handle().clone(),
                port,
                entrypoint_id: "term".to_string(),
                route_id: "terminal".to_string(),
                base_path: "/terminal".to_string(),
            });
            let router = Router::new().nest("/terminal", build_router(ctx));
            let server = tauri::async_runtime::spawn(async move {
                let _ = axum::serve(socket, router.into_make_service()).await;
            });
            let base = format!("http://127.0.0.1:{port}");

            let shell = reqwest::get(format!("{base}/terminal"))
                .await
                .expect("send");
            assert_eq!(shell.status(), reqwest::StatusCode::OK);
            let body = shell.text().await.expect("body");
            assert!(
                body.contains("window.__PRMONITOR_REMOTE_BASE_PATH__=\"\\/terminal\""),
                "{body}"
            );
            assert!(
                !body.contains("src=\"/assets/") && !body.contains("href=\"/assets/"),
                "{body}"
            );

            let unknown_api_at_root = reqwest::Client::new()
                .post(format!("{base}/invoke/not_terminal"))
                .send()
                .await
                .expect("send");
            assert_eq!(unknown_api_at_root.status(), reqwest::StatusCode::NOT_FOUND);

            let unknown_api_under_route = reqwest::Client::new()
                .post(format!("{base}/terminal/invoke/not_terminal"))
                .send()
                .await
                .expect("send");
            assert_eq!(
                unknown_api_under_route.status(),
                reqwest::StatusCode::NOT_FOUND
            );
            server.abort();
        });
    }

    #[test]
    fn terminal_serves_static_spa_shell_host_gated_not_bearer_gated() {
        let app = tauri::test::mock_app();
        tauri::async_runtime::block_on(async move {
            let socket = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind loopback");
            let port = socket.local_addr().expect("addr").port();
            let mut l = listener();
            l.port = port;
            persist_listener(app.handle(), l);
            let ctx = Arc::new(Ctx {
                app: app.handle().clone(),
                port,
                entrypoint_id: "term".to_string(),
                route_id: "terminal".to_string(),
                base_path: String::new(),
            });
            let server = tauri::async_runtime::spawn(async move {
                let _ = axum::serve(socket, build_router(ctx).into_make_service()).await;
            });
            let client = reqwest::Client::new();
            let base = format!("http://127.0.0.1:{port}");

            // Shell loads WITHOUT a bearer token (the SPA must render before it can prompt for one).
            let root = client.get(format!("{base}/")).send().await.expect("send");
            assert_eq!(root.status(), reqwest::StatusCode::OK);
            assert!(
                root.headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .contains("text/html"),
                "root should serve index.html as text/html"
            );

            // Unknown deep link → SPA fallback to index.html (200 text/html), still no bearer.
            let deep = client
                .get(format!("{base}/settings/tokens"))
                .send()
                .await
                .expect("send");
            assert_eq!(deep.status(), reqwest::StatusCode::OK);
            assert!(deep
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .contains("text/html"));

            // Static is still Host-gated (DNS-rebind defense).
            let bad_host = client
                .get(format!("{base}/"))
                .header("host", "evil.example.com")
                .send()
                .await
                .expect("send");
            assert_eq!(bad_host.status(), reqwest::StatusCode::FORBIDDEN);

            // ...and Origin-gated: a cross-site request from a non-allowlisted Origin is rejected.
            let bad_origin = client
                .get(format!("{base}/"))
                .header("origin", "https://evil.example.com")
                .header("sec-fetch-site", "cross-site")
                .send()
                .await
                .expect("send");
            assert_eq!(bad_origin.status(), reqwest::StatusCode::FORBIDDEN);

            // Top-level document navigation can be cross-site without an Origin header; Host gating is
            // still enough for the unauthenticated static shell so the SPA can render its token prompt.
            let cross_site_navigation = client
                .get(format!("{base}/"))
                .header("sec-fetch-site", "cross-site")
                .send()
                .await
                .expect("send");
            assert_eq!(cross_site_navigation.status(), reqwest::StatusCode::OK);

            // A missing asset-like path (has an extension) → real 404, NOT index.html — so the browser
            // never receives `text/html` where it expects JS/CSS (which would break the SPA).
            let missing_asset = client
                .get(format!("{base}/assets/does-not-exist-xyz.js"))
                .send()
                .await
                .expect("send");
            assert_eq!(missing_asset.status(), reqwest::StatusCode::NOT_FOUND);

            // Regression: the static fallback did NOT loosen the API gate — /invoke without a bearer
            // is still rejected, and an unknown command is still 404 (matched before any auth).
            let api_unauth = client
                .post(format!("{base}/invoke/list_terminal_sessions"))
                .json(&serde_json::json!({}))
                .send()
                .await
                .expect("send");
            assert_eq!(api_unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

            let unknown = client
                .post(format!("{base}/invoke/not_terminal"))
                .send()
                .await
                .expect("send");
            assert_eq!(unknown.status(), reqwest::StatusCode::NOT_FOUND);

            server.abort();
        });
    }
}
