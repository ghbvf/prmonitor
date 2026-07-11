//! Messaging event persistence (#1559).

use rusqlite::OptionalExtension;

use crate::db::{map_err, Database};
use crate::error::{AppError, AppResult};
use crate::messaging::truncate_utf8_boundary;
use crate::model::{
    ActionStatus, MessagingEvent, MessagingEventEntry, MessagingEventStatus, MessagingReplyAudit,
};

const LIST_LIMIT: i64 = 500;
const MAX_MESSAGING_EVENTS: i64 = 5000;
const MAX_ERROR_LEN: usize = 1024;
const MAX_REPLY_SUMMARY_LEN: usize = 512;

pub enum DedupInsert {
    Inserted(i64),
    Existing(Box<MessagingEventEntry>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyRecord {
    pub outbox_id: i64,
    pub kind: String,
    pub summary: String,
}

pub(crate) fn now_epoch() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn status_as_wire(status: MessagingEventStatus) -> &'static str {
    status.as_wire()
}

pub(crate) fn status_from_wire(value: &str) -> MessagingEventStatus {
    MessagingEventStatus::from_wire_lenient(value)
}

fn action_status_from_wire(value: &str) -> Option<ActionStatus> {
    serde_json::from_value(serde_json::Value::String(value.to_string())).ok()
}

pub fn insert_dedup(db: &Database, event: &MessagingEvent) -> AppResult<DedupInsert> {
    let event_json = serde_json::to_string(event)
        .map_err(|e| AppError::new(format!("messaging event 序列化失败: {e}")))?;
    db.with_tx(|tx| {
        let provider = event.provider.as_wire();
        tx.execute(
            "INSERT INTO messaging_event \
             (provider, integration_id, event_id, conversation_id, event_json, raw_summary, \
              status, received_at_epoch) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(provider, integration_id, event_id) DO NOTHING",
            rusqlite::params![
                provider,
                event.integration_id,
                event.event_id,
                event.conversation_id,
                event_json,
                event.raw_payload,
                status_as_wire(MessagingEventStatus::Received),
                event.received_at_epoch as i64,
            ],
        )
        .map_err(map_err)?;
        if tx.changes() != 1 {
            let raw = tx
                .query_row(
                    "SELECT me.id, me.event_json, me.status, me.processed_at_epoch, me.error, \
                            me.reply_outbox_id, me.reply_kind, me.reply_summary, ao.status, ao.last_error \
                     FROM messaging_event me \
                     LEFT JOIN action_outbox ao ON ao.id = me.reply_outbox_id \
                     WHERE me.provider = ?1 AND me.integration_id = ?2 AND me.event_id = ?3",
                    rusqlite::params![provider, event.integration_id, event.event_id],
                    hydrate_row,
                )
                .map_err(map_err)?;
            return raw.into_entry().map(Box::new).map(DedupInsert::Existing);
        }
        let id = tx.last_insert_rowid();
        tx.execute(
            "DELETE FROM messaging_event WHERE id IN ( \
                 SELECT id FROM messaging_event ORDER BY id DESC LIMIT -1 OFFSET ?1)",
            rusqlite::params![MAX_MESSAGING_EVENTS],
        )
        .map_err(map_err)?;
        Ok(DedupInsert::Inserted(id))
    })
}

pub fn mark_processed(db: &Database, id: i64, now: u64) -> AppResult<()> {
    mark_terminal(db, id, MessagingEventStatus::Processed, None, None, now)
}

pub fn mark_processed_with_reply(
    db: &Database,
    id: i64,
    reply: Option<&ReplyRecord>,
    now: u64,
) -> AppResult<()> {
    mark_terminal(db, id, MessagingEventStatus::Processed, None, reply, now)
}

pub fn mark_failed(db: &Database, id: i64, error: &str, now: u64) -> AppResult<()> {
    mark_failed_with_reply(db, id, error, None, now)
}

pub fn mark_failed_with_reply(
    db: &Database,
    id: i64,
    error: &str,
    reply: Option<&ReplyRecord>,
    now: u64,
) -> AppResult<()> {
    mark_terminal(
        db,
        id,
        MessagingEventStatus::Failed,
        Some(error),
        reply,
        now,
    )
}

fn mark_terminal(
    db: &Database,
    id: i64,
    status: MessagingEventStatus,
    error: Option<&str>,
    reply: Option<&ReplyRecord>,
    now: u64,
) -> AppResult<()> {
    let message = error.map(|value| truncate_utf8_boundary(value, MAX_ERROR_LEN));
    let reply_summary =
        reply.map(|value| truncate_utf8_boundary(&value.summary, MAX_REPLY_SUMMARY_LEN));
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE messaging_event SET \
                status = ?2, processed_at_epoch = ?3, error = ?4, \
                reply_outbox_id = COALESCE(?5, reply_outbox_id), \
                reply_kind = COALESCE(?6, reply_kind), \
                reply_summary = COALESCE(?7, reply_summary) \
             WHERE id = ?1",
            rusqlite::params![
                id,
                status_as_wire(status),
                now as i64,
                message,
                reply.map(|value| value.outbox_id),
                reply.map(|value| value.kind.as_str()),
                reply_summary,
            ],
        )?;
        Ok(())
    })
}

pub fn list(db: &Database, integration_id: Option<&str>) -> AppResult<Vec<MessagingEventEntry>> {
    db.with_conn(|conn| {
        let rows = match integration_id {
            Some(id) => {
                let mut stmt = conn.prepare(
                    "SELECT me.id, me.event_json, me.status, me.processed_at_epoch, me.error, \
                            me.reply_outbox_id, me.reply_kind, me.reply_summary, ao.status, ao.last_error \
                     FROM messaging_event me \
                     LEFT JOIN action_outbox ao ON ao.id = me.reply_outbox_id \
                     WHERE me.integration_id = ?1 ORDER BY me.id DESC LIMIT ?2",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![id, LIST_LIMIT], hydrate_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT me.id, me.event_json, me.status, me.processed_at_epoch, me.error, \
                            me.reply_outbox_id, me.reply_kind, me.reply_summary, ao.status, ao.last_error \
                     FROM messaging_event me \
                     LEFT JOIN action_outbox ao ON ao.id = me.reply_outbox_id \
                     ORDER BY me.id DESC LIMIT ?1",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![LIST_LIMIT], hydrate_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            }
        };
        Ok(rows)
    })
    .and_then(|rows| {
        rows.into_iter()
            .map(|row| row.into_entry())
            .collect::<AppResult<Vec<_>>>()
    })
}

pub fn get_entry(db: &Database, id: i64) -> AppResult<Option<MessagingEventEntry>> {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT me.id, me.event_json, me.status, me.processed_at_epoch, me.error, \
                    me.reply_outbox_id, me.reply_kind, me.reply_summary, ao.status, ao.last_error \
             FROM messaging_event me \
             LEFT JOIN action_outbox ao ON ao.id = me.reply_outbox_id \
             WHERE me.id = ?1",
            [id],
            hydrate_row,
        )
        .optional()
    })
    .and_then(|maybe| maybe.map(RawRow::into_entry).transpose())
}

pub fn get_raw_summary(db: &Database, id: i64) -> AppResult<Option<String>> {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT raw_summary FROM messaging_event WHERE id = ?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()
    })
}

fn hydrate_row(row: &rusqlite::Row) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        id: row.get(0)?,
        event_json: row.get(1)?,
        status: row.get(2)?,
        processed_at_epoch: row.get::<_, Option<i64>>(3)?,
        error: row.get(4)?,
        reply_outbox_id: row.get(5)?,
        reply_kind: row.get(6)?,
        reply_summary: row.get(7)?,
        reply_status: row.get(8)?,
        reply_error: row.get(9)?,
    })
}

struct RawRow {
    id: i64,
    event_json: String,
    status: String,
    processed_at_epoch: Option<i64>,
    error: Option<String>,
    reply_outbox_id: Option<i64>,
    reply_kind: Option<String>,
    reply_summary: Option<String>,
    reply_status: Option<String>,
    reply_error: Option<String>,
}

impl RawRow {
    fn into_entry(self) -> AppResult<MessagingEventEntry> {
        let event = serde_json::from_str::<MessagingEvent>(&self.event_json)
            .map_err(|e| AppError::new(format!("messaging event 反序列化失败: {e}")))?;
        Ok(MessagingEventEntry {
            id: self.id,
            event,
            status: status_from_wire(&self.status),
            processed_at_epoch: self.processed_at_epoch.map(|n| n as u64),
            error: self.error,
            reply: self
                .reply_outbox_id
                .zip(self.reply_kind)
                .zip(self.reply_summary)
                .map(|((outbox_id, kind), summary)| MessagingReplyAudit {
                    outbox_id,
                    kind,
                    summary,
                    status: self
                        .reply_status
                        .as_deref()
                        .and_then(action_status_from_wire),
                    error: self.reply_error,
                }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MessagingProviderKind;

    fn event(event_id: &str) -> MessagingEvent {
        MessagingEvent {
            provider: MessagingProviderKind::Feishu,
            integration_id: "fs".to_string(),
            event_id: event_id.to_string(),
            conversation_id: "chat".to_string(),
            thread_id: "msg".to_string(),
            sender_id: "u".to_string(),
            text: "/help".to_string(),
            mentioned_bot: true,
            raw_payload: "{}".to_string(),
            received_at_epoch: 1,
        }
    }

    #[test]
    fn insert_dedup_processes_same_provider_integration_event_once() {
        let db = Database::open_in_memory().expect("open");
        let first = insert_dedup(&db, &event("evt")).expect("insert");
        let second = insert_dedup(&db, &event("evt")).expect("dedupe");
        assert!(matches!(first, DedupInsert::Inserted(_)));
        assert!(matches!(second, DedupInsert::Existing(_)));
        assert_eq!(list(&db, None).expect("list").len(), 1);
    }

    #[test]
    fn duplicate_returns_existing_status_for_retry_decision() {
        let db = Database::open_in_memory().expect("open");
        let id = match insert_dedup(&db, &event("evt")).expect("insert") {
            DedupInsert::Inserted(id) => id,
            DedupInsert::Existing(_) => panic!("first insert must be new"),
        };
        mark_failed(&db, id, "boom", 2).expect("mark failed");
        let existing = match insert_dedup(&db, &event("evt")).expect("dedupe") {
            DedupInsert::Existing(entry) => entry,
            DedupInsert::Inserted(_) => panic!("duplicate must return existing row"),
        };
        assert_eq!(existing.id, id);
        assert_eq!(existing.status, MessagingEventStatus::Failed);
    }

    #[test]
    fn mark_failed_truncates_non_ascii_on_char_boundary() {
        let db = Database::open_in_memory().expect("open");
        let id = match insert_dedup(&db, &event("evt")).expect("insert") {
            DedupInsert::Inserted(id) => id,
            DedupInsert::Existing(_) => panic!("first insert must be new"),
        };
        mark_failed(&db, id, &"错误".repeat(800), 2).expect("mark failed");
        let entry = get_entry(&db, id).expect("get").expect("entry");
        assert_eq!(entry.status, MessagingEventStatus::Failed);
        assert!(entry.error.expect("error").is_char_boundary(0));
    }

    #[test]
    fn status_wire_matches_serde_values() {
        assert_eq!(status_as_wire(MessagingEventStatus::Received), "received");
        assert_eq!(status_as_wire(MessagingEventStatus::Processed), "processed");
        assert_eq!(status_as_wire(MessagingEventStatus::Failed), "failed");
        assert_eq!(status_from_wire("bad"), MessagingEventStatus::Failed);
    }

    #[test]
    fn event_entry_includes_reply_outbox_status() {
        let db = Database::open_in_memory().expect("open");
        let id = match insert_dedup(&db, &event("evt")).expect("insert") {
            DedupInsert::Inserted(id) => id,
            DedupInsert::Existing(_) => panic!("first insert must be new"),
        };
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO action_outbox \
                 (id, project_id, kind, summary, payload, status, next_attempt_at, created_at, updated_at, producer_key) \
                 VALUES (9, 'fs', 'messagingReply', 'reply help', '{}', 'done', 0, 0, 0, 'test:messaging-reply:9')",
                [],
            )?;
            Ok(())
        })
        .expect("seed outbox");
        mark_processed_with_reply(
            &db,
            id,
            Some(&ReplyRecord {
                outbox_id: 9,
                kind: "help".to_string(),
                summary: "reply help".to_string(),
            }),
            2,
        )
        .expect("mark");

        let entry = get_entry(&db, id).expect("get").expect("entry");
        let reply = entry.reply.expect("reply");
        assert_eq!(reply.outbox_id, 9);
        assert_eq!(reply.kind, "help");
        assert_eq!(reply.summary, "reply help");
        assert_eq!(reply.status, Some(ActionStatus::Done));
    }
}
