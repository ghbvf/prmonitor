//! Local loopback REST API for messaging send/log commands.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Serialize;
use subtle::ConstantTimeEq;
use tauri::Manager;

use crate::config::service as config_service;
use crate::db::Database;
use crate::model::SendMessagingRequest;

const MAX_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Serialize)]
struct ErrorBody {
    message: String,
}

pub(crate) struct Ctx<R: tauri::Runtime> {
    pub(crate) app: tauri::AppHandle<R>,
    pub(crate) port: u16,
    pub(crate) remote_entrypoint_id: Option<String>,
}

pub(crate) fn build_router<R: tauri::Runtime>(ctx: Arc<Ctx<R>>) -> Router {
    Router::new()
        .route("/messaging/send", post(handle_send::<R>))
        .route("/messaging/events", get(handle_events::<R>))
        .route("/messaging/sends", get(handle_sends::<R>))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(ctx)
}

fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response {
    match serde_json::to_string(body) {
        Ok(s) => (status, [(header::CONTENT_TYPE, "application/json")], s).into_response(),
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

fn host_allowed(host: &str, port: u16) -> bool {
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

fn origin_allowed(origin: Option<&str>, sec_fetch_site: Option<&str>, port: u16) -> bool {
    if let Some(sfs) = sec_fetch_site {
        return matches!(sfs, "same-origin" | "none");
    }
    match origin {
        None => true,
        Some(o) => {
            let with_port = format!(":{port}");
            ["http://127.0.0.1", "http://localhost", "http://[::1]"]
                .iter()
                .any(|h| o == *h || o.strip_prefix(h).is_some_and(|rest| rest == with_port))
        }
    }
}

fn verify_bearer(token: &str, header: &str) -> bool {
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

fn check_request<R: tauri::Runtime>(ctx: &Ctx<R>, headers: &HeaderMap) -> Option<Response> {
    if ctx.remote_entrypoint_id.is_none() {
        if !host_allowed(header_str(headers, "host").unwrap_or(""), ctx.port) {
            return Some(error_response(
                StatusCode::FORBIDDEN,
                "Host 不被允许（仅 loopback 可访问）",
            ));
        }
        if !origin_allowed(
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
    let token = match config_service::load(&ctx.app) {
        Ok(cfg) => cfg.local_api_token,
        Err(_) => return Some(error_response(StatusCode::UNAUTHORIZED, "鉴权不可用")),
    };
    if !verify_bearer(
        token.trim(),
        header_str(headers, "authorization").unwrap_or(""),
    ) {
        return Some(error_response(StatusCode::UNAUTHORIZED, "未授权"));
    }
    None
}

fn query_param(uri: &Uri, key: &str) -> Option<String> {
    url::form_urlencoded::parse(uri.query()?.as_bytes())
        .find_map(|(k, v)| (k == key && !v.trim().is_empty()).then(|| v.into_owned()))
}

async fn handle_send<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(resp) = check_request(&ctx, &headers) {
        return resp;
    }
    let req: SendMessagingRequest = match serde_json::from_slice(body.as_ref()) {
        Ok(r) => r,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, format!("请求体解析失败: {e}")),
    };
    let runtime = ctx.app.state::<super::service::MessagingRuntime<R>>();
    match super::service::enqueue_send(&ctx.app, runtime.actions.as_ref(), req) {
        Ok(response) => json_response(StatusCode::ACCEPTED, &response),
        Err(e) => error_response(StatusCode::BAD_REQUEST, e.message),
    }
}

async fn handle_events<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Some(resp) = check_request(&ctx, &headers) {
        return resp;
    }
    let db = ctx.app.state::<Database>();
    match super::store::list(db.inner(), query_param(&uri, "integrationId").as_deref()) {
        Ok(entries) => json_response(StatusCode::OK, &entries),
        Err(e) => error_response(StatusCode::BAD_REQUEST, e.message),
    }
}

async fn handle_sends<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if let Some(resp) = check_request(&ctx, &headers) {
        return resp;
    }
    let runtime = ctx.app.state::<super::service::MessagingRuntime<R>>();
    let integration_id = query_param(&uri, "integrationId");
    match runtime
        .actions
        .list_sends(&ctx.app, integration_id.as_deref())
    {
        Ok(entries) => json_response(StatusCode::OK, &entries),
        Err(e) => error_response(StatusCode::BAD_REQUEST, e.message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_param_decodes_integration_id() {
        let uri: Uri = "/messaging/events?integrationId=wx%2Fcorp"
            .parse()
            .expect("uri");
        assert_eq!(
            query_param(&uri, "integrationId").as_deref(),
            Some("wx/corp")
        );
    }

    #[test]
    fn events_route_is_registered_and_gated() {
        let app = tauri::test::mock_app();
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
                remote_entrypoint_id: None,
            });
            let server = tauri::async_runtime::spawn(async move {
                let _ = axum::serve(listener, build_router(ctx).into_make_service()).await;
            });

            let url = format!("http://127.0.0.1:{port}/messaging/events");
            let client = reqwest::Client::new();

            let forbidden = client
                .get(&url)
                .header("host", "evil.com")
                .send()
                .await
                .expect("send");
            assert_eq!(forbidden.status(), reqwest::StatusCode::FORBIDDEN);

            let unauthorized = client.get(&url).send().await.expect("send");
            assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);

            server.abort();
        });
    }
}
