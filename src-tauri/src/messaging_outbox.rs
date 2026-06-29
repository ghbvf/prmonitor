//! Messaging send outbox composition helpers.
//!
//! The messaging slice prepares provider-agnostic send payloads; this horizontal module persists
//! them into the generic outbox where composition code needs transaction ownership.

use rusqlite::Transaction;

use crate::config;
use crate::error::{AppError, AppResult};
use crate::messaging;
use crate::model::{ActionKind, SendMessagingRequest};
use crate::outbox;

pub(crate) const MESSAGING_OUTBOX_SCOPE: &str = "__messaging__";

pub(crate) fn enqueue_send_once_after_in_tx(
    tx: &Transaction<'_>,
    config: &config::model::AppConfig,
    request: SendMessagingRequest,
    delay_secs: u64,
    now: u64,
) -> AppResult<i64> {
    let integration = config
        .messaging
        .integrations
        .iter()
        .find(|integration| integration.id == request.integration_id)
        .cloned()
        .ok_or_else(|| {
            AppError::new(format!(
                "messagingIntegrationId 不存在: {}",
                request.integration_id
            ))
        })?;
    let prepared = messaging::service::prepare_send(&integration, request)?;
    if let Some(existing_id) = outbox::store::id_by_dedupe_key_any_status_in_tx(
        tx,
        MESSAGING_OUTBOX_SCOPE,
        &prepared.dedupe_key,
    )? {
        return Ok(existing_id);
    }
    outbox::store::enqueue_in_tx(
        tx,
        &outbox::store::EnqueueInput {
            project_id: MESSAGING_OUTBOX_SCOPE,
            kind: ActionKind::MessagingSend,
            summary: &prepared.summary,
            payload: &prepared.payload_json,
            dedupe_key: Some(&prepared.dedupe_key),
            next_attempt_at: Some(now.saturating_add(delay_secs)),
        },
        now,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[test]
    fn enqueue_send_once_after_in_tx_reuses_terminal_row() {
        let db = Database::open_in_memory().expect("open");
        let config = config::model::AppConfig {
            messaging: config::model::MessagingSettings {
                integrations: vec![config::model::MessagingIntegration {
                    id: "fs".to_string(),
                    name: "Feishu".to_string(),
                    allowed_conversation_ids: vec!["chat".to_string()],
                    enabled: true,
                    ..config::model::MessagingIntegration::feishu_default()
                }],
            },
            ..config::model::AppConfig::default()
        };
        let request = SendMessagingRequest {
            integration_id: "fs".to_string(),
            conversation_id: "chat".to_string(),
            text: "hello".to_string(),
            request_id: "req-1".to_string(),
        };
        let first = db
            .with_tx(|tx| enqueue_send_once_after_in_tx(tx, &config, request.clone(), 30, 100))
            .expect("first enqueue");
        outbox::store::mark_done(&db, first, 1, 200).expect("mark done");

        let second = db
            .with_tx(|tx| enqueue_send_once_after_in_tx(tx, &config, request, 30, 300))
            .expect("second enqueue");

        assert_eq!(second, first);
        let count: i64 = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM action_outbox WHERE dedupe_key = 'messaging-send:fs:req-1'",
                    [],
                    |row| row.get(0),
                )
            })
            .expect("count");
        assert_eq!(count, 1);
    }
}
