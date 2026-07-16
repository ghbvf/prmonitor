//! Messaging Tauri commands and HTTP router (#1559).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde_json::json;
use tauri::Manager;

use crate::config::service as config_service;
use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::messaging::{service, store};
use crate::model::{
    MessagingEventEntry, MessagingIntegrationOption, MessagingProviderKind, OutboxEntry,
    SendMessagingRequest, SendMessagingResponse,
};

const MAX_BODY_BYTES: usize = 1024 * 1024;

pub(crate) struct Ctx<R: tauri::Runtime> {
    pub(crate) app: tauri::AppHandle<R>,
}

pub(crate) fn build_router<R: tauri::Runtime>(ctx: Arc<Ctx<R>>) -> Router {
    Router::new()
        .route("/:provider/:integration_id", post(handle_provider::<R>))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(ctx)
}

async fn handle_provider<R: tauri::Runtime>(
    State(ctx): State<Arc<Ctx<R>>>,
    Path((provider, integration_id)): Path<(String, String)>,
    mut headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let provider = match provider_from_path(&provider) {
        Ok(provider) => provider,
        Err(e) => return (StatusCode::NOT_FOUND, e.message).into_response(),
    };
    // Capability is the single source — never Soft-match Feishu|DingTalk here.
    if service::supports_long_connection(provider) {
        return (
            StatusCode::GONE,
            service::long_connection_http_gone_message(provider),
        )
            .into_response();
    }
    merge_provider_query_headers(provider, &mut headers, uri.query());
    let runtime = ctx.app.state::<service::MessagingRuntime<R>>();
    match service::ingest(
        &ctx.app,
        runtime.actions.as_ref(),
        provider,
        integration_id,
        headers,
        body.to_vec(),
    )
    .await
    {
        Ok(service::IngestResponse::Challenge { challenge }) => (
            StatusCode::OK,
            [("content-type", "application/json")],
            json!({ "challenge": challenge }).to_string(),
        )
            .into_response(),
        Ok(service::IngestResponse::Ack) => StatusCode::OK.into_response(),
        Err(e) => {
            let status = if e.message.contains("签名") || e.message.contains("token") {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::BAD_REQUEST
            };
            (status, e.message).into_response()
        }
    }
}

fn provider_from_path(value: &str) -> AppResult<MessagingProviderKind> {
    MessagingProviderKind::from_wire(value)
        .ok_or_else(|| AppError::new(format!("未知 messaging provider: {value}")))
}

fn merge_provider_query_headers(
    provider: MessagingProviderKind,
    headers: &mut HeaderMap,
    query: Option<&str>,
) {
    let Some(query) = query else {
        return;
    };
    if service::supports_long_connection(provider) {
        return;
    }
    if provider != MessagingProviderKind::WeChatWork {
        return;
    }
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let header = match key.as_ref() {
            "msg_signature" => "x-wechatwork-msg-signature",
            "timestamp" => "x-wechatwork-timestamp",
            "nonce" => "x-wechatwork-nonce",
            "echostr" => "x-wechatwork-echostr",
            _ => continue,
        };
        if let Ok(value) = HeaderValue::from_str(value.as_ref()) {
            headers.insert(header, value);
        }
    }
}

#[tauri::command]
pub fn messaging_events_list<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    integration_id: Option<String>,
) -> AppResult<Vec<MessagingEventEntry>> {
    let db = app.state::<Database>();
    store::list(db.inner(), integration_id.as_deref())
}

#[tauri::command]
pub fn messaging_event_raw<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    id: i64,
) -> AppResult<String> {
    let db = app.state::<Database>();
    store::get_raw_summary(db.inner(), id)?
        .ok_or_else(|| AppError::new(format!("messagingEventId 不存在: {id}")))
}

#[tauri::command]
pub async fn messaging_event_replay<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    id: i64,
) -> AppResult<()> {
    let runtime = app.state::<service::MessagingRuntime<R>>();
    service::replay(&app, runtime.actions.as_ref(), id).await
}

#[tauri::command]
pub fn messaging_send<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    request: SendMessagingRequest,
) -> AppResult<SendMessagingResponse> {
    let runtime = app.state::<service::MessagingRuntime<R>>();
    service::enqueue_send(&app, runtime.actions.as_ref(), request)
}

#[tauri::command]
pub fn messaging_sends_list<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    integration_id: Option<String>,
) -> AppResult<Vec<OutboxEntry>> {
    let runtime = app.state::<service::MessagingRuntime<R>>();
    runtime.actions.list_sends(&app, integration_id.as_deref())
}

#[tauri::command]
pub fn messaging_integrations_list<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> AppResult<Vec<MessagingIntegrationOption>> {
    Ok(config_service::enabled_messaging_integrations(&app)?
        .into_iter()
        .map(|integration| MessagingIntegrationOption {
            id: integration.id,
            name: integration.name,
            kind: integration.kind,
            allowed_conversation_ids: integration.allowed_conversation_ids,
        })
        .collect())
}

#[tauri::command]
pub fn messaging_connection_statuses_list<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> Vec<crate::model::MessagingConnectionStatus> {
    let mut statuses = app
        .state::<crate::messaging::feishu_long_connection::FeishuConnectionManager>()
        .statuses();
    statuses.extend(
        app.state::<crate::messaging::dingtalk_stream::DingTalkConnectionManager>()
            .statuses(),
    );
    statuses.sort_by(|a, b| {
        a.provider
            .as_wire()
            .cmp(b.provider.as_wire())
            .then_with(|| a.integration_id.cmp(&b.integration_id))
    });
    statuses
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wechat_work_query_params_map_to_signature_headers() {
        let mut headers = HeaderMap::new();
        merge_provider_query_headers(
            MessagingProviderKind::WeChatWork,
            &mut headers,
            Some("msg_signature=sig&timestamp=1&nonce=n&echostr=hello&ignored=x"),
        );
        assert_eq!(
            headers
                .get("x-wechatwork-msg-signature")
                .and_then(|v| v.to_str().ok()),
            Some("sig")
        );
        assert_eq!(
            headers
                .get("x-wechatwork-timestamp")
                .and_then(|v| v.to_str().ok()),
            Some("1")
        );
        assert_eq!(
            headers
                .get("x-wechatwork-nonce")
                .and_then(|v| v.to_str().ok()),
            Some("n")
        );
        assert_eq!(
            headers
                .get("x-wechatwork-echostr")
                .and_then(|v| v.to_str().ok()),
            Some("hello")
        );
        assert!(!headers.contains_key("ignored"));
    }

    #[test]
    fn http_gone_follows_supports_long_connection_capability() {
        assert!(service::supports_long_connection(
            MessagingProviderKind::Feishu
        ));
        assert!(service::supports_long_connection(
            MessagingProviderKind::DingTalk
        ));
        assert!(!service::supports_long_connection(
            MessagingProviderKind::WeChatWork
        ));
        assert!(service::long_connection_http_gone_message(
            MessagingProviderKind::Feishu
        )
        .contains("飞书"));
        assert!(service::long_connection_http_gone_message(
            MessagingProviderKind::DingTalk
        )
        .contains("钉钉"));
    }

    #[test]
    fn non_wechat_work_query_params_do_not_mutate_headers() {
        let mut headers = HeaderMap::new();
        merge_provider_query_headers(
            MessagingProviderKind::DingTalk,
            &mut headers,
            Some("msg_signature=sig&timestamp=1"),
        );
        assert!(headers.is_empty());
    }
}
