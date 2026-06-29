//! User-authored notification send funnel (#1460).
//!
//! This is a composition-horizontal module: it reads notification channel config, builds the
//! backend-internal normalized [`crate::model::Notification`], and persists
//! [`crate::model::NotificationDeliveryPayload`] rows through the generic outbox. Keeping this out
//! of the `review` vertical slice preserves the existing review→outbox boundary.

use std::collections::HashSet;

use rusqlite::Transaction;
use tauri::{Manager, Runtime};
use url::Url;

use crate::config::model::{AppConfig, NotificationSettings};
use crate::config::service as config_service;
use crate::error::{AppError, AppResult};
use crate::model::{
    ActionKind, Notification, NotificationDeliveryPayload, NotificationLevel,
    RedactedNotificationBody, SendNotificationRequest, SendNotificationResponse,
};
use crate::outbox;

const MAX_TITLE_CHARS: usize = 200;
const MAX_BODY_CHARS: usize = 4_000;
const MAX_URL_CHARS: usize = 2_048;
const MAX_PROJECT_ID_CHARS: usize = 128;
const MAX_CHANNEL_IDS: usize = 32;
const MAX_CHANNEL_ID_CHARS: usize = 128;

struct NormalizedRequest {
    level: NotificationLevel,
    title: String,
    body: String,
    url: String,
    project_id: String,
    channel_ids: Vec<String>,
}

struct PreparedRow {
    project_id: String,
    summary: String,
    payload: String,
    dedupe_key: Option<String>,
}

/// Tauri command entry for desktop/frontend callers.
#[tauri::command]
pub async fn send_notification<R: Runtime>(
    app: tauri::AppHandle<R>,
    request: SendNotificationRequest,
) -> AppResult<SendNotificationResponse> {
    enqueue_notification(&app, request)
}

/// Normalize a user-authored notification and enqueue one delivery row per selected channel.
pub fn enqueue_notification<R: Runtime>(
    app: &tauri::AppHandle<R>,
    request: SendNotificationRequest,
) -> AppResult<SendNotificationResponse> {
    enqueue_notification_with_dedupe_prefix(app, request, None)
}

pub(crate) fn enqueue_notification_with_dedupe_prefix<R: Runtime>(
    app: &tauri::AppHandle<R>,
    request: SendNotificationRequest,
    dedupe_prefix: Option<&str>,
) -> AppResult<SendNotificationResponse> {
    enqueue_notification_with_dedupe_prefix_after(app, request, dedupe_prefix, 0)
}

pub(crate) fn enqueue_notification_with_dedupe_prefix_after<R: Runtime>(
    app: &tauri::AppHandle<R>,
    request: SendNotificationRequest,
    dedupe_prefix: Option<&str>,
    delay_secs: u64,
) -> AppResult<SendNotificationResponse> {
    let request = normalize_request(request)?;
    let channels = selected_channels(app, &request.channel_ids)?;
    let prepared = prepare_delivery_rows(request, channels, dedupe_prefix)?;
    let db = app.state::<crate::db::Database>();
    let now = outbox::store::now_epoch();
    let rows: Vec<outbox::store::EnqueueInput<'_>> = prepared
        .iter()
        .map(|row| outbox::store::EnqueueInput {
            project_id: &row.project_id,
            kind: ActionKind::Notification,
            summary: &row.summary,
            payload: &row.payload,
            dedupe_key: row.dedupe_key.as_deref(),
            next_attempt_at: Some(now.saturating_add(delay_secs)),
        })
        .collect();
    let outbox_ids = db
        .inner()
        .with_tx(|tx| enqueue_prepared_in_tx(tx, &prepared, &rows, now))?;
    for id in &outbox_ids {
        outbox::service::announce_updated(app, db.inner(), *id);
    }
    app.state::<crate::state::AppState>().outbox.wake();

    Ok(SendNotificationResponse { outbox_ids })
}

pub(crate) fn enqueue_notification_in_tx(
    tx: &Transaction<'_>,
    config: &AppConfig,
    request: SendNotificationRequest,
    dedupe_prefix: Option<&str>,
    delay_secs: u64,
    now: u64,
) -> AppResult<Vec<i64>> {
    let request = normalize_request(request)?;
    let channels = selected_channels_from_settings(&config.notifications, &request.channel_ids)?;
    let prepared = prepare_delivery_rows(request, channels, dedupe_prefix)?;
    let rows: Vec<outbox::store::EnqueueInput<'_>> = prepared
        .iter()
        .map(|row| outbox::store::EnqueueInput {
            project_id: &row.project_id,
            kind: ActionKind::Notification,
            summary: &row.summary,
            payload: &row.payload,
            dedupe_key: row.dedupe_key.as_deref(),
            next_attempt_at: Some(now.saturating_add(delay_secs)),
        })
        .collect();
    enqueue_prepared_in_tx(tx, &prepared, &rows, now)
}

fn prepare_delivery_rows(
    request: NormalizedRequest,
    channels: Vec<config_service::NotificationChannel>,
    dedupe_prefix: Option<&str>,
) -> AppResult<Vec<PreparedRow>> {
    if channels.is_empty() {
        return Err(AppError::new("没有启用的通知渠道可发送".to_string()));
    }

    let note = Notification::new(
        request.level,
        request.title,
        request.url,
        RedactedNotificationBody::user_supplied(request.body),
        request.project_id,
    );

    let mut prepared = Vec::with_capacity(channels.len());
    for channel in &channels {
        let delivery = NotificationDeliveryPayload {
            notification: note.clone(),
            channel_id: channel.id.clone(),
            kind: channel.kind,
        };
        let summary = format!("{} via {}", note.title, channel.name);
        let payload = serde_json::to_string(&delivery)
            .map_err(|e| AppError::new(format!("outbox 通知序列化失败：{e}")))?;
        let dedupe_key = dedupe_prefix.map(|prefix| format!("{prefix}:{}", channel.id));
        prepared.push(PreparedRow {
            project_id: note.project_id.clone(),
            summary,
            payload,
            dedupe_key,
        });
    }
    Ok(prepared)
}

fn enqueue_prepared_in_tx(
    tx: &Transaction<'_>,
    prepared: &[PreparedRow],
    rows: &[outbox::store::EnqueueInput<'_>],
    now: u64,
) -> AppResult<Vec<i64>> {
    let mut ids = Vec::with_capacity(prepared.len());
    for (prepared_row, row) in prepared.iter().zip(rows.iter()) {
        if let Some(dedupe_key) = prepared_row.dedupe_key.as_deref() {
            if let Some(existing_id) = outbox::store::id_by_dedupe_key_any_status_in_tx(
                tx,
                &prepared_row.project_id,
                dedupe_key,
            )? {
                ids.push(existing_id);
                continue;
            }
        }
        ids.push(outbox::store::enqueue_in_tx(tx, row, now)?);
    }
    Ok(ids)
}

fn normalize_request(request: SendNotificationRequest) -> AppResult<NormalizedRequest> {
    let level = request.level.unwrap_or_default();
    let title = trim_required("title", request.title, MAX_TITLE_CHARS)?;
    let body = trim_optional("body", request.body, MAX_BODY_CHARS)?.unwrap_or_default();
    let url = trim_optional("url", request.url, MAX_URL_CHARS)?.unwrap_or_default();
    if body.is_empty() && url.is_empty() {
        return Err(AppError::new("通知 body 与 url 至少提供一个".to_string()));
    }
    validate_url(&url)?;
    let project_id =
        trim_optional("projectId", request.project_id, MAX_PROJECT_ID_CHARS)?.unwrap_or_default();
    let channel_ids = normalize_channel_ids(request.channel_ids)?;
    Ok(NormalizedRequest {
        level,
        title,
        body,
        url,
        project_id,
        channel_ids,
    })
}

fn trim_required(field: &str, value: String, max_chars: usize) -> AppResult<String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(AppError::new(format!("通知 {field} 不能为空")));
    }
    ensure_len(field, &value, max_chars)?;
    Ok(value)
}

fn trim_optional(
    field: &str,
    value: Option<String>,
    max_chars: usize,
) -> AppResult<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim().to_string();
    ensure_len(field, &value, max_chars)?;
    Ok((!value.is_empty()).then_some(value))
}

fn ensure_len(field: &str, value: &str, max_chars: usize) -> AppResult<()> {
    if value.chars().count() > max_chars {
        return Err(AppError::new(format!(
            "通知 {field} 过长（最多 {max_chars} 字符）"
        )));
    }
    Ok(())
}

fn validate_url(url: &str) -> AppResult<()> {
    if url.is_empty() {
        return Ok(());
    }
    let parsed =
        Url::parse(url).map_err(|_| AppError::new("通知 url 必须是合法 URL".to_string()))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(AppError::new(
            "通知 url 仅支持无 userinfo 的 http/https URL".to_string(),
        ));
    }
    Ok(())
}

fn normalize_channel_ids(channel_ids: Vec<String>) -> AppResult<Vec<String>> {
    if channel_ids.len() > MAX_CHANNEL_IDS {
        return Err(AppError::new(format!(
            "通知 channelIds 过多（最多 {MAX_CHANNEL_IDS} 个）"
        )));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(channel_ids.len());
    for raw in channel_ids {
        let id = trim_required("channelId", raw, MAX_CHANNEL_ID_CHARS)?;
        if !seen.insert(id.clone()) {
            return Err(AppError::new(format!("通知 channelId 重复: {id}")));
        }
        out.push(id);
    }
    Ok(out)
}

fn selected_channels<R: Runtime>(
    app: &tauri::AppHandle<R>,
    channel_ids: &[String],
) -> AppResult<Vec<config_service::NotificationChannel>> {
    let config = config_service::load(app)?;
    selected_channels_from_settings(&config.notifications, channel_ids)
}

fn selected_channels_from_settings(
    notifications: &NotificationSettings,
    channel_ids: &[String],
) -> AppResult<Vec<config_service::NotificationChannel>> {
    if channel_ids.is_empty() {
        return Ok(notifications
            .channels
            .iter()
            .filter(|channel| channel.enabled)
            .cloned()
            .collect());
    }
    let mut channels = Vec::with_capacity(channel_ids.len());
    for id in channel_ids {
        let channel = notifications
            .channels
            .iter()
            .find(|channel| channel.id == *id)
            .cloned()
            .ok_or_else(|| AppError::new(format!("notificationChannelId 不存在: {id}")))?;
        if !channel.enabled {
            return Err(AppError::new(format!("notificationChannelId 已禁用: {id}")));
        }
        channels.push(channel);
    }
    Ok(channels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::{AppConfig, NotificationChannel};
    use crate::db::Database;
    use crate::model::{NotificationKind, NotificationLevel};
    use crate::outbox::store;
    use crate::state::AppState;
    use tauri::Manager;

    fn req() -> SendNotificationRequest {
        SendNotificationRequest {
            level: None,
            title: " Deploy done ".to_string(),
            body: Some(" Build 42 finished ".to_string()),
            url: Some("https://example.com/build/42".to_string()),
            project_id: Some(" p1 ".to_string()),
            channel_ids: Vec::new(),
        }
    }

    fn channel(id: &str, kind: NotificationKind) -> NotificationChannel {
        NotificationChannel {
            id: id.to_string(),
            name: id.to_string(),
            kind,
            enabled: true,
            webhook_url: "https://hooks.example.com/services/T000/B000/secret".to_string(),
            webhook_secret: "top-secret".to_string(),
            telegram_bot_token: "bot-token".to_string(),
            telegram_chat_id: "chat".to_string(),
            smtp_password: "smtp-secret".to_string(),
            ..NotificationChannel::default()
        }
    }

    #[test]
    fn normalize_request_defaults_trims_and_validates() {
        let normalized = normalize_request(req()).expect("valid");
        assert_eq!(normalized.level, NotificationLevel::Info);
        assert_eq!(normalized.title, "Deploy done");
        assert_eq!(normalized.body, "Build 42 finished");
        assert_eq!(normalized.project_id, "p1");

        assert!(normalize_request(SendNotificationRequest {
            title: " ".to_string(),
            ..req()
        })
        .is_err());
        assert!(normalize_request(SendNotificationRequest {
            body: None,
            url: None,
            ..req()
        })
        .is_err());
        assert!(normalize_request(SendNotificationRequest {
            url: Some("file:///tmp/secret".to_string()),
            ..req()
        })
        .is_err());
        assert!(normalize_request(SendNotificationRequest {
            url: Some("https://user:pass@example.com/x".to_string()),
            ..req()
        })
        .is_err());
        assert!(normalize_request(SendNotificationRequest {
            channel_ids: vec!["desktop".to_string(), " desktop ".to_string()],
            ..req()
        })
        .is_err());
    }

    #[test]
    fn enqueue_notification_returns_ids_and_persists_secret_free_delivery_payloads() {
        let app = tauri::test::mock_app();
        app.manage(AppState::default());
        let db = Database::open_in_memory().expect("open db");
        crate::config::service::persist_db(
            &db,
            &AppConfig {
                notifications: crate::config::model::NotificationSettings {
                    channels: vec![
                        channel("desktop", NotificationKind::Desktop),
                        channel("slack-main", NotificationKind::Slack),
                    ],
                },
                ..AppConfig::default()
            },
        )
        .expect("persist config");
        app.manage(db);

        let response = enqueue_notification(
            app.handle(),
            SendNotificationRequest {
                level: Some(NotificationLevel::Warning),
                channel_ids: vec!["desktop".to_string(), "slack-main".to_string()],
                ..req()
            },
        )
        .expect("enqueue");

        assert_eq!(response.outbox_ids.len(), 2);
        for id in response.outbox_ids {
            let raw = store::get_raw(app.state::<Database>().inner(), id)
                .expect("get raw")
                .expect("raw exists");
            let payload: NotificationDeliveryPayload =
                serde_json::from_str(&raw).expect("delivery payload parses");
            assert_eq!(payload.notification.title, "Deploy done");
            assert_eq!(payload.notification.level, NotificationLevel::Warning);
            assert!(raw.contains("\"channelId\""));
            assert!(!raw.contains("webhookUrl"), "{raw}");
            assert!(!raw.contains("top-secret"), "{raw}");
            assert!(!raw.contains("bot-token"), "{raw}");
            assert!(!raw.contains("smtp-secret"), "{raw}");
            assert!(!raw.contains("authorization"), "{raw}");
        }
    }

    #[test]
    fn dedupe_prefix_reuses_terminal_outbox_rows() {
        let app = tauri::test::mock_app();
        app.manage(AppState::default());
        let db = Database::open_in_memory().expect("open db");
        crate::config::service::persist_db(
            &db,
            &AppConfig {
                notifications: crate::config::model::NotificationSettings {
                    channels: vec![channel("desktop", NotificationKind::Desktop)],
                },
                ..AppConfig::default()
            },
        )
        .expect("persist config");
        app.manage(db);

        let first =
            enqueue_notification_with_dedupe_prefix(app.handle(), req(), Some("workflow:1:notify"))
                .expect("first enqueue");
        assert_eq!(first.outbox_ids.len(), 1);
        app.state::<Database>()
            .inner()
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE action_outbox SET status = 'done' WHERE id = ?1",
                    rusqlite::params![first.outbox_ids[0]],
                )
                .map(|_| ())
            })
            .expect("terminalize row");

        let second =
            enqueue_notification_with_dedupe_prefix(app.handle(), req(), Some("workflow:1:notify"))
                .expect("second enqueue");

        assert_eq!(second.outbox_ids, first.outbox_ids);
        let count: i64 = app
            .state::<Database>()
            .inner()
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM action_outbox WHERE dedupe_key = 'workflow:1:notify:desktop'",
                    [],
                    |row| row.get(0),
                )
            })
            .expect("count rows");
        assert_eq!(count, 1, "terminal row is reused, not duplicated");
    }

    #[test]
    fn enqueue_notification_rolls_back_all_channels_when_one_insert_fails() {
        let app = tauri::test::mock_app();
        app.manage(AppState::default());
        let db = Database::open_in_memory().expect("open db");
        crate::config::service::persist_db(
            &db,
            &AppConfig {
                notifications: crate::config::model::NotificationSettings {
                    channels: vec![
                        channel("first", NotificationKind::Desktop),
                        channel("fail-second", NotificationKind::Slack),
                    ],
                },
                ..AppConfig::default()
            },
        )
        .expect("persist config");
        db.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER fail_second_notification_insert
                 BEFORE INSERT ON action_outbox
                 WHEN NEW.summary LIKE '%fail-second%'
                 BEGIN
                   SELECT RAISE(ABORT, 'forced second insert failure');
                 END;",
            )
        })
        .expect("install trigger");
        app.manage(db);

        let err = enqueue_notification(
            app.handle(),
            SendNotificationRequest {
                channel_ids: vec!["first".to_string(), "fail-second".to_string()],
                ..req()
            },
        )
        .expect_err("second channel insert fails");
        assert!(
            err.message.contains("forced second insert failure"),
            "{}",
            err.message
        );

        let count: i64 = app
            .state::<Database>()
            .inner()
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM action_outbox WHERE status = 'pending'",
                    [],
                    |row| row.get(0),
                )
            })
            .expect("count rows");
        assert_eq!(count, 0, "batch enqueue must not leave a partial row");
    }
}
