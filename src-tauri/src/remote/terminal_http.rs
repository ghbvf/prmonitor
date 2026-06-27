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
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tauri::Manager;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use url::Url;

use crate::config::model::{terminal_auth_token_is_strong, Listener};
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

#[derive(Clone)]
pub(crate) struct Ctx<R: tauri::Runtime> {
    pub(crate) app: tauri::AppHandle<R>,
    pub(crate) port: u16,
    pub(crate) listener_id: String,
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
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(ctx)
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
        .map(|state| state.remote.public_urls_for_listener(&ctx.listener_id))
        .unwrap_or_default()
}

fn check_request<R: tauri::Runtime>(
    ctx: &Ctx<R>,
    headers: &HeaderMap,
    permission: Permission,
) -> HttpResult<Listener> {
    let cfg = config_service::load(&ctx.app)
        .map_err(|_| Box::new(error_response(StatusCode::UNAUTHORIZED, "鉴权配置不可用")))?;
    let listener = cfg
        .listeners
        .into_iter()
        .find(|l| l.id == ctx.listener_id && l.enabled)
        .ok_or_else(|| Box::new(error_response(StatusCode::FORBIDDEN, "终端监听器未启用")))?;
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
    let cfg = match config_service::load(&ctx.app) {
        Ok(cfg) => cfg,
        Err(_) => return error_response(StatusCode::UNAUTHORIZED, "鉴权配置不可用"),
    };
    let Some(listener) = cfg
        .listeners
        .into_iter()
        .find(|l| l.id == ctx.listener_id && l.enabled)
    else {
        return error_response(StatusCode::FORBIDDEN, "终端监听器未启用");
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
        let Some(db) = app.try_state::<Database>() else {
            let db = Database::open_in_memory().expect("open db");
            crate::config::service::persist_db(
                &db,
                &AppConfig {
                    listeners: vec![listener],
                    ..AppConfig::default()
                },
            )
            .expect("persist config");
            app.manage(db);
            return;
        };
        crate::config::service::persist_db(
            &db,
            &AppConfig {
                listeners: vec![listener],
                ..AppConfig::default()
            },
        )
        .expect("persist config");
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
                listener_id: "term".to_string(),
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
                listener_id: "term".to_string(),
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
                listener_id: "term".to_string(),
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
                listener_id: "term".to_string(),
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
}
