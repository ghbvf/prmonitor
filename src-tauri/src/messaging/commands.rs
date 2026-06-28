//! Messaging Tauri commands and HTTP router (#1559).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde_json::json;
use tauri::Manager;

use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::messaging::{service, store};
use crate::model::{MessagingEventEntry, MessagingProviderKind};

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
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let provider = match provider_from_path(&provider) {
        Ok(provider) => provider,
        Err(e) => return (StatusCode::NOT_FOUND, e.message).into_response(),
    };
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
