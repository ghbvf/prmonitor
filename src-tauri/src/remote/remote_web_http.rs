//! Browser HTTP/SSE surface for the Remote Web Console (#1369).
//!
//! This is deliberately NOT the local REST API (`review::local_api`): that listener remains
//! loopback-only for CLI/curl. `remote-web` is an explicit listener kind with its own strong bearer
//! token, HTTPS public URL policy, static SPA shell, and a sealed command allowlist.

use std::str::FromStr;
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
use tokio_stream::wrappers::BroadcastStream;
use url::Url;

use crate::config::model::{
    remote_web_auth_token_is_strong, AppConfig, Listener, ListenerAuthMode, ListenerKind, Project,
    Tunnel,
};
use crate::config::service as config_service;
use crate::error::{AppError, AppResult};
use crate::events::{remote_web_sse_topic, PrEvent, RemoteWebSseTopic, ReviewEvent, StreamEvent};
use crate::state::AppState;

const MAX_BODY_BYTES: usize = 64 * 1024;
const CORS_ALLOW_HEADERS: &str = "authorization, content-type";
const CORS_ALLOW_METHODS: &str = "GET, POST, OPTIONS";
const INDEX_HTML: &str = "index.html";

type HttpResult<T> = Result<T, Box<Response>>;

#[derive(RustEmbed)]
#[folder = "../dist-web"]
struct WebAssets;

#[derive(Clone)]
pub(crate) struct Ctx<R: tauri::Runtime> {
    pub(crate) app: tauri::AppHandle<R>,
    pub(crate) port: u16,
    pub(crate) listener_id: String,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteProjectSummary {
    id: String,
    name: String,
    enabled: bool,
    repo: String,
    source_kind: crate::model::SourceKind,
    engine_kind: crate::model::EngineKind,
    update_mode: crate::model::UpdateMode,
}

impl From<Project> for RemoteProjectSummary {
    fn from(project: Project) -> Self {
        Self {
            id: project.id,
            name: project.name,
            enabled: project.enabled,
            repo: project.repo,
            source_kind: project.source_kind,
            engine_kind: project.engine_kind,
            update_mode: project.update_mode,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteConsoleSnapshot {
    app_version: String,
    active_project_id: String,
    projects: Vec<RemoteProjectSummary>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectArgs {
    project_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrArgs {
    project_id: String,
    pr_number: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionHistoryArgs {
    project_id: String,
    pr_number: u64,
    thread_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartReviewArgs {
    project_id: String,
    pr_number: u64,
    kind: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StopReviewArgs {
    session_id: String,
}

/// The sealed remote command surface. Handlers dispatch on this enum, never on ad-hoc strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum RemoteWebCommand {
    RemoteConsoleSnapshot,
    GetPrs,
    ListReviewSessions,
    GetPrSessions,
    GetSessionHistory,
    GetCodexStatus,
    GetClaudeStatus,
    StartReview,
    StopReview,
}

impl FromStr for RemoteWebCommand {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "remote_console_snapshot" => Ok(Self::RemoteConsoleSnapshot),
            "get_prs" => Ok(Self::GetPrs),
            "list_review_sessions" => Ok(Self::ListReviewSessions),
            "get_pr_sessions" => Ok(Self::GetPrSessions),
            "get_session_history" => Ok(Self::GetSessionHistory),
            "get_codex_status" => Ok(Self::GetCodexStatus),
            "get_claude_status" => Ok(Self::GetClaudeStatus),
            "start_review" => Ok(Self::StartReview),
            "stop_review" => Ok(Self::StopReview),
            _ => Err(()),
        }
    }
}

/// Validated, private-field policy for one remote-web listener. API/static/SSE gates consume this
/// type instead of raw config so an unchecked listener cannot accidentally reach the public surface.
#[derive(Debug, Clone)]
struct RemoteWebPolicy {
    token: String,
    public_origins: Vec<String>,
    public_authorities: Vec<String>,
}

impl RemoteWebPolicy {
    fn from_config(
        cfg: AppConfig,
        listener_id: &str,
        runtime_public_urls: Vec<String>,
    ) -> AppResult<Self> {
        let public_urls = tunnel_public_urls_for_listener(&cfg.tunnels, listener_id)
            .into_iter()
            .chain(runtime_public_urls)
            .collect();
        let listener = cfg
            .listeners
            .into_iter()
            .find(|l| l.id == listener_id && l.enabled && l.kind == ListenerKind::RemoteWeb)
            .ok_or_else(|| AppError::new("远程面板监听器未启用"))?;
        Self::from_listener(listener, public_urls)
    }

    fn from_listener(listener: Listener, runtime_public_urls: Vec<String>) -> AppResult<Self> {
        if !listener.enabled || listener.kind != ListenerKind::RemoteWeb {
            return Err(AppError::new(
                "远程面板 policy 只能由已启用 remote-web 监听器构造",
            ));
        }
        if listener.auth != ListenerAuthMode::Bearer {
            return Err(AppError::new("远程面板必须使用 bearer 鉴权"));
        }
        if !remote_web_auth_token_is_strong(&listener.auth_token) {
            return Err(AppError::new("远程面板 token 强度不足"));
        }
        let mut public_urls = Vec::new();
        if !listener.public_url.trim().is_empty() {
            public_urls.push(listener.public_url.trim().to_string());
        }
        public_urls.extend(
            runtime_public_urls
                .into_iter()
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty()),
        );
        let mut public_origins = Vec::new();
        let mut public_authorities = Vec::new();
        for url in public_urls {
            let parsed = Url::parse(&url)
                .map_err(|_| AppError::new(format!("远程面板 publicUrl 非法: {url}")))?;
            if parsed.scheme() != "https"
                || !parsed.username().is_empty()
                || parsed.password().is_some()
            {
                return Err(AppError::new(format!(
                    "远程面板 publicUrl 必须是 https:// 且不能内嵌凭据: {url}"
                )));
            }
            let host = parsed
                .host_str()
                .ok_or_else(|| AppError::new(format!("远程面板 publicUrl 缺少 host: {url}")))?
                .to_ascii_lowercase();
            let authority = public_authority(&parsed)?;
            let origin = public_origin(&parsed)?;
            if !public_authorities.iter().any(|h| h == &authority) {
                public_authorities.push(authority);
            }
            if parsed.port().is_none() && parsed.scheme() == "https" {
                if !public_authorities.iter().any(|h| h == &host) {
                    public_authorities.push(host.clone());
                }
                let default_authority = format!("{host}:443");
                if !public_authorities.iter().any(|h| h == &default_authority) {
                    public_authorities.push(default_authority);
                }
            }
            if !public_origins.iter().any(|o| o == &origin) {
                public_origins.push(origin);
            }
        }
        if public_origins.is_empty() || public_authorities.is_empty() {
            return Err(AppError::new("远程面板缺少 HTTPS publicUrl"));
        }
        Ok(Self {
            token: listener.auth_token.trim().to_string(),
            public_origins,
            public_authorities,
        })
    }

    fn host_allowed(&self, host: &str, port: u16) -> bool {
        loopback_host_allowed(host, port)
            || self
                .public_authorities
                .iter()
                .any(|public_host| normalize_host_header(host).eq_ignore_ascii_case(public_host))
    }

    fn origin_allowed(&self, origin: Option<&str>) -> bool {
        match origin {
            None => true,
            Some(origin) => self
                .public_origins
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(origin.trim_end_matches('/'))),
        }
    }

    fn cors_origin<'a>(&self, headers: &'a HeaderMap) -> Option<&'a str> {
        let origin = header_str(headers, "origin")?;
        self.origin_allowed(Some(origin)).then_some(origin)
    }

    fn verify(&self, header: &str) -> bool {
        verify_bearer(&self.token, header)
    }
}

fn tunnel_public_urls_for_listener(tunnels: &[Tunnel], listener_id: &str) -> Vec<String> {
    tunnels
        .iter()
        .filter(|t| t.enabled && t.target_listener_id.trim() == listener_id)
        .map(|t| t.public_url.trim().to_string())
        .filter(|u| !u.is_empty())
        .collect()
}

fn public_authority(url: &Url) -> AppResult<String> {
    let host = url
        .host_str()
        .ok_or_else(|| AppError::new("远程面板 publicUrl 缺少 host"))?
        .to_ascii_lowercase();
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn public_origin(url: &Url) -> AppResult<String> {
    let host = url
        .host_str()
        .ok_or_else(|| AppError::new("远程面板 publicUrl 缺少 host"))?;
    let mut origin = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    Ok(origin)
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
        .fallback(handle_static::<R>)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(ctx)
}

fn runtime_public_urls<R: tauri::Runtime>(ctx: &Ctx<R>) -> Vec<String> {
    ctx.app
        .try_state::<AppState>()
        .map(|state| state.remote.public_urls_for_listener(&ctx.listener_id))
        .unwrap_or_default()
}

fn policy<R: tauri::Runtime>(ctx: &Ctx<R>) -> HttpResult<RemoteWebPolicy> {
    let cfg = config_service::load(&ctx.app).map_err(|_| {
        Box::new(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "服务不可用",
        ))
    })?;
    RemoteWebPolicy::from_config(cfg, &ctx.listener_id, runtime_public_urls(ctx))
        .map_err(|e| Box::new(error_response(StatusCode::FORBIDDEN, e.message)))
}

fn check_static_request<R: tauri::Runtime>(
    ctx: &Ctx<R>,
    headers: &HeaderMap,
) -> HttpResult<RemoteWebPolicy> {
    let policy = policy(ctx)?;
    if !policy.host_allowed(header_str(headers, "host").unwrap_or(""), ctx.port) {
        return Err(Box::new(error_response(
            StatusCode::FORBIDDEN,
            "Host 不被允许",
        )));
    }
    if !policy.origin_allowed(header_str(headers, "origin")) {
        return Err(Box::new(error_response(
            StatusCode::FORBIDDEN,
            "Origin 不被允许",
        )));
    }
    Ok(policy)
}

fn check_request<R: tauri::Runtime>(
    ctx: &Ctx<R>,
    headers: &HeaderMap,
) -> HttpResult<RemoteWebPolicy> {
    let policy = check_static_request(ctx, headers)?;
    if !policy.verify(header_str(headers, "authorization").unwrap_or("")) {
        return Err(Box::new(with_cors(
            error_response(StatusCode::UNAUTHORIZED, "未授权"),
            policy.cors_origin(headers),
        )));
    }
    Ok(policy)
}

async fn handle_static<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let policy = match check_static_request(&ctx, &headers) {
        Ok(policy) => policy,
        Err(resp) => return *resp,
    };
    let Some(requested) = resolve_asset(uri.path()) else {
        return error_response(StatusCode::NOT_FOUND, "资源不存在");
    };
    let (key, body) = match WebAssets::get(requested) {
        Some(content) => (requested, content.data),
        None => {
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
    let response = (
        [
            (header::CONTENT_TYPE, mime.as_ref()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::X_FRAME_OPTIONS, "SAMEORIGIN"),
        ],
        body.into_owned(),
    )
        .into_response();
    with_cors(response, policy.cors_origin(&headers))
}

async fn handle_invoke<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    headers: HeaderMap,
    Path(command): Path<String>,
    body: Bytes,
) -> Response {
    let command = match RemoteWebCommand::from_str(&command) {
        Ok(command) => command,
        Err(()) => return error_response(StatusCode::NOT_FOUND, "未知远程面板命令"),
    };
    let policy = match check_request(&ctx, &headers) {
        Ok(policy) => policy,
        Err(resp) => return *resp,
    };
    let response = match dispatch_command(&ctx, command, body).await {
        Ok(response) => response,
        Err(e) => error_response(StatusCode::BAD_REQUEST, e.message),
    };
    with_cors(response, policy.cors_origin(&headers))
}

async fn dispatch_command<R: tauri::Runtime>(
    ctx: &Ctx<R>,
    command: RemoteWebCommand,
    body: Bytes,
) -> AppResult<Response> {
    match command {
        RemoteWebCommand::RemoteConsoleSnapshot => {
            let cfg = config_service::load(&ctx.app)?;
            Ok(json_response(
                StatusCode::OK,
                &RemoteConsoleSnapshot {
                    app_version: env!("CARGO_PKG_VERSION").to_string(),
                    active_project_id: cfg.active_project_id,
                    projects: cfg.projects.into_iter().map(Into::into).collect(),
                },
            ))
        }
        RemoteWebCommand::GetPrs => {
            let args: ProjectArgs = parse_args(body)?;
            let prs = crate::pr::commands::get_prs(ctx.app.clone(), &args.project_id)?;
            Ok(json_response(StatusCode::OK, &prs))
        }
        RemoteWebCommand::ListReviewSessions => {
            let state = ctx.app.state::<AppState>();
            let sessions = crate::review::commands::list_review_sessions(state);
            Ok(json_response(StatusCode::OK, &sessions))
        }
        RemoteWebCommand::GetPrSessions => {
            let args: PrArgs = parse_args(body)?;
            let sessions = crate::review::commands::get_pr_sessions(
                ctx.app.clone(),
                args.project_id,
                args.pr_number,
            )?;
            Ok(json_response(StatusCode::OK, &sessions))
        }
        RemoteWebCommand::GetSessionHistory => {
            let args: SessionHistoryArgs = parse_args(body)?;
            let history = crate::review::commands::get_session_history(
                ctx.app.clone(),
                args.project_id,
                args.pr_number,
                args.thread_id,
            )?;
            Ok(json_response(StatusCode::OK, &history))
        }
        RemoteWebCommand::GetCodexStatus => {
            let state = ctx.app.state::<AppState>();
            let status = crate::review::commands::get_codex_status(ctx.app.clone(), state).await?;
            Ok(json_response(StatusCode::OK, &status))
        }
        RemoteWebCommand::GetClaudeStatus => {
            let status = crate::review::commands::get_claude_status().await?;
            Ok(json_response(StatusCode::OK, &status))
        }
        RemoteWebCommand::StartReview => {
            let args: StartReviewArgs = parse_args(body)?;
            let state = ctx.app.state::<AppState>();
            let id = crate::review::commands::start_review(
                ctx.app.clone(),
                state,
                args.project_id,
                args.pr_number,
                args.kind,
            )
            .await?;
            Ok(json_response(StatusCode::OK, &id))
        }
        RemoteWebCommand::StopReview => {
            let args: StopReviewArgs = parse_args(body)?;
            let state = ctx.app.state::<AppState>();
            crate::review::commands::stop_review(ctx.app.clone(), state, args.session_id).await?;
            Ok(empty_response())
        }
    }
}

async fn handle_events<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let Some(topic) = remote_web_sse_topic(query_topic(&uri).as_deref()) else {
        return error_response(StatusCode::NOT_FOUND, "未知事件 topic");
    };
    let policy = match check_request(&ctx, &headers) {
        Ok(policy) => policy,
        Err(resp) => return *resp,
    };
    let rx = ctx.app.state::<AppState>().stream.subscribe();
    let auth_ctx = Arc::clone(&ctx);
    let auth_headers = headers.clone();
    let stream = BroadcastStream::new(rx)
        .take_while(move |_| {
            let auth_ctx = Arc::clone(&auth_ctx);
            let auth_headers = auth_headers.clone();
            async move { check_request(auth_ctx.as_ref(), &auth_headers).is_ok() }
        })
        .filter_map(move |item| async move {
            match (topic, item.ok()?) {
                (RemoteWebSseTopic::Review, StreamEvent::Review(ev)) => Some(sse_review_data(&ev)),
                (RemoteWebSseTopic::Pr, StreamEvent::Pr(ev)) => Some(sse_pr_data(&ev)),
                _ => None,
            }
        });
    with_cors(
        Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response(),
        policy.cors_origin(&headers),
    )
}

async fn handle_options<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    headers: HeaderMap,
) -> Response {
    let policy = match check_static_request(&ctx, &headers) {
        Ok(policy) => policy,
        Err(resp) => return *resp,
    };
    with_cors(empty_response(), policy.cors_origin(&headers))
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

fn parse_args<T: for<'de> Deserialize<'de>>(body: Bytes) -> AppResult<T> {
    serde_json::from_slice(body.as_ref()).map_err(|e| AppError::new(format!("请求体解析失败: {e}")))
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

fn resolve_asset(path: &str) -> Option<&str> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.split('/').any(|seg| seg == "..") {
        return None;
    }
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "ui" {
        return Some(INDEX_HTML);
    }
    let Some(without_ui) = trimmed.strip_prefix("ui/") else {
        return trimmed.starts_with("assets/").then_some(trimmed);
    };
    Some(if without_ui.is_empty() {
        INDEX_HTML
    } else {
        without_ui
    })
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn normalize_host_header(host: &str) -> String {
    host.trim().trim_matches(['[', ']']).to_ascii_lowercase()
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

fn verify_bearer(token: &str, header: &str) -> bool {
    let Some(presented) = header
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, value)| value)
    else {
        return false;
    };
    presented.as_bytes().ct_eq(token.as_bytes()).into()
}

fn query_topic(uri: &Uri) -> Option<String> {
    uri.query()?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "topic").then(|| value.replace("%3A", ":").replace("%3a", ":"))
    })
}

fn sse_review_data(event: &ReviewEvent) -> Result<SseEvent, std::convert::Infallible> {
    Ok(SseEvent::default().data(serde_json::to_string(event).unwrap_or_else(|_| {
        r#"{"kind":"error","projectId":"","threadId":"","message":"review stream serialize error"}"#
            .to_string()
    })))
}

fn sse_pr_data(event: &PrEvent) -> Result<SseEvent, std::convert::Infallible> {
    Ok(
        SseEvent::default().data(serde_json::to_string(event).unwrap_or_else(|_| {
            r#"{"kind":"error","projectId":"","message":"pr stream serialize error"}"#.to_string()
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::ListenerAuthMode;
    use std::collections::BTreeSet;

    fn listener() -> Listener {
        Listener {
            id: "web".to_string(),
            name: "Remote Web".to_string(),
            kind: ListenerKind::RemoteWeb,
            bind_host: "127.0.0.1".to_string(),
            port: 9200,
            enabled: true,
            auth: ListenerAuthMode::Bearer,
            auth_token: "remote-web-token-0123456789abcdef".to_string(),
            terminal_read: false,
            terminal_write: false,
            terminal_create: false,
            terminal_admin: false,
            allowed_origins: vec!["https://evil.example.com".to_string()],
            public_url: "https://console.example.com".to_string(),
        }
    }

    #[test]
    fn remote_web_policy_allows_loopback_and_public_host_only() {
        let p = RemoteWebPolicy::from_listener(listener(), vec![]).expect("policy");
        assert!(p.host_allowed("127.0.0.1:9200", 9200));
        assert!(p.host_allowed("console.example.com", 9200));
        assert!(p.host_allowed("console.example.com:443", 9200));
        assert!(!p.host_allowed("evil.example.com", 9200));
    }

    #[test]
    fn remote_web_policy_ignores_listener_allowed_origins() {
        let p = RemoteWebPolicy::from_listener(listener(), vec![]).expect("policy");
        assert!(p.origin_allowed(Some("https://console.example.com")));
        assert!(!p.origin_allowed(Some("https://evil.example.com")));
    }

    #[test]
    fn remote_web_policy_requires_bearer_strong_token_and_https_url() {
        let no_bearer = Listener {
            auth: ListenerAuthMode::None,
            ..listener()
        };
        assert!(RemoteWebPolicy::from_listener(no_bearer, vec![]).is_err());
        let short = Listener {
            auth_token: "short".to_string(),
            ..listener()
        };
        assert!(RemoteWebPolicy::from_listener(short, vec![]).is_err());
        let http = Listener {
            public_url: "http://console.example.com".to_string(),
            ..listener()
        };
        assert!(RemoteWebPolicy::from_listener(http, vec![]).is_err());
    }

    #[test]
    fn remote_web_policy_preserves_public_url_port_in_host_gate() {
        let p = RemoteWebPolicy::from_listener(
            Listener {
                public_url: "https://console.example.com:8443/ui".to_string(),
                ..listener()
            },
            vec![],
        )
        .expect("policy");
        assert!(p.host_allowed("console.example.com:8443", 9200));
        assert!(!p.host_allowed("console.example.com", 9200));
        assert!(!p.host_allowed("console.example.com:9443", 9200));
        assert!(p.origin_allowed(Some("https://console.example.com:8443")));
        assert!(!p.origin_allowed(Some("https://console.example.com")));
    }

    #[test]
    fn remote_web_policy_uses_enabled_tunnel_public_url_from_config() {
        let cfg = AppConfig {
            listeners: vec![Listener {
                public_url: String::new(),
                ..listener()
            }],
            tunnels: vec![Tunnel {
                id: "web-tunnel".to_string(),
                enabled: true,
                target_listener_id: "web".to_string(),
                public_url: "https://tunnel.example.com".to_string(),
                ..Tunnel::default()
            }],
            ..AppConfig::default()
        };
        let p = RemoteWebPolicy::from_config(cfg, "web", vec![]).expect("policy");
        assert!(p.host_allowed("tunnel.example.com", 9200));
        assert!(p.origin_allowed(Some("https://tunnel.example.com")));
        assert!(!p.host_allowed("console.example.com", 9200));
    }

    #[test]
    fn remote_web_commands_are_closed_allowlist() {
        let names = [
            "remote_console_snapshot",
            "get_prs",
            "list_review_sessions",
            "get_pr_sessions",
            "get_session_history",
            "get_codex_status",
            "get_claude_status",
            "start_review",
            "stop_review",
        ];
        let parsed: BTreeSet<_> = names
            .iter()
            .map(|name| RemoteWebCommand::from_str(name).expect("known"))
            .collect();
        assert_eq!(parsed.len(), names.len());
        assert!(RemoteWebCommand::from_str("get_config").is_err());
        assert!(RemoteWebCommand::from_str("start_webhook").is_err());
        assert!(RemoteWebCommand::from_str("stop_terminal_daemon").is_err());
    }

    #[test]
    fn bearer_gate_rejects_missing_short_and_wrong_tokens() {
        let p = RemoteWebPolicy::from_listener(listener(), vec![]).expect("policy");
        assert!(p.verify("Bearer remote-web-token-0123456789abcdef"));
        assert!(p.verify("bearer remote-web-token-0123456789abcdef"));
        assert!(!p.verify(""));
        assert!(!p.verify("Basic remote-web-token-0123456789abcdef"));
        assert!(!p.verify("Bearer short"));
        assert!(!p.verify("Bearer remote-web-token-0123456789abcdeg"));
    }

    #[test]
    fn remote_console_snapshot_serde_shape_is_camel_case() {
        let json = serde_json::to_value(RemoteConsoleSnapshot {
            app_version: "1.2.3".to_string(),
            active_project_id: "p1".to_string(),
            projects: vec![RemoteProjectSummary {
                id: "p1".to_string(),
                name: "Project".to_string(),
                enabled: true,
                repo: "owner/repo".to_string(),
                source_kind: crate::model::SourceKind::Github,
                engine_kind: crate::model::EngineKind::Codex,
                update_mode: crate::model::UpdateMode::WebhookOnly,
            }],
        })
        .expect("snapshot serializes");
        assert_eq!(
            json,
            serde_json::json!({
                "appVersion": "1.2.3",
                "activeProjectId": "p1",
                "projects": [{
                    "id": "p1",
                    "name": "Project",
                    "enabled": true,
                    "repo": "owner/repo",
                    "sourceKind": "github",
                    "engineKind": "codex",
                    "updateMode": "webhook-only"
                }]
            })
        );
    }

    #[test]
    fn resolve_asset_mounts_ui_without_shadowing_assets() {
        assert_eq!(resolve_asset("/"), None);
        assert_eq!(resolve_asset("/ui"), Some(INDEX_HTML));
        assert_eq!(resolve_asset("/ui/sessions"), Some("sessions"));
        assert_eq!(resolve_asset("/assets/app.js"), Some("assets/app.js"));
        assert_eq!(resolve_asset("/ui/assets/app.js"), Some("assets/app.js"));
        assert_eq!(resolve_asset("/settings"), None);
        assert_eq!(resolve_asset("/../secret"), None);
    }
}
