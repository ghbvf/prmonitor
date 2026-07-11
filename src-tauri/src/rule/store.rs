use rusqlite::{OptionalExtension, Transaction};
use serde::Serialize;

use crate::db::{map_err, Database};
use crate::error::AppResult;

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleMatchEntry {
    pub id: i64,
    pub rule_id: String,
    pub rule_name: String,
    pub inbox_event_id: i64,
    pub project_id: String,
    pub action_count: u32,
    pub error: Option<String>,
    pub created_at: u64,
    pub action_outbox_ids: Vec<i64>,
}

pub struct NewRuleMatch<'a> {
    pub rule_id: &'a str,
    pub rule_name: &'a str,
    pub inbox_event_id: i64,
    pub project_id: &'a str,
    pub action_ids: &'a [i64],
    pub error: Option<&'a str>,
    pub now: u64,
}

pub fn now_epoch() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn insert_match(db: &Database, new: NewRuleMatch<'_>) -> AppResult<i64> {
    db.with_tx(|tx| insert_match_in_tx(tx, new))
}

pub(crate) fn insert_match_in_tx(tx: &Transaction<'_>, new: NewRuleMatch<'_>) -> AppResult<i64> {
    tx.execute(
        "INSERT INTO rule_match \
         (rule_id, rule_name, inbox_event_id, project_id, action_count, error, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            new.rule_id,
            new.rule_name,
            new.inbox_event_id,
            new.project_id,
            new.action_ids.len() as i64,
            new.error,
            new.now as i64
        ],
    )
    .map_err(map_err)?;
    let id = tx.last_insert_rowid();
    for action_id in new.action_ids {
        tx.execute(
            "INSERT INTO rule_match_action (rule_match_id, action_outbox_id) VALUES (?1, ?2)",
            rusqlite::params![id, action_id],
        )
        .map_err(map_err)?;
    }
    Ok(id)
}

pub fn list_by_inbox(db: &Database, inbox_event_id: i64) -> AppResult<Vec<RuleMatchEntry>> {
    let rows: Vec<RawMatch> = db.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, rule_id, rule_name, inbox_event_id, project_id, action_count, error, created_at \
             FROM rule_match WHERE inbox_event_id = ?1 ORDER BY id DESC",
        )?;
        let rows = stmt
            .query_map([inbox_event_id], read_match)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    })?;
    hydrate_actions(db, rows)
}

fn hydrate_actions(db: &Database, rows: Vec<RawMatch>) -> AppResult<Vec<RuleMatchEntry>> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let action_outbox_ids = db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT action_outbox_id FROM rule_match_action \
                 WHERE rule_match_id = ?1 ORDER BY action_outbox_id",
            )?;
            let ids = stmt
                .query_map([row.id], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(ids)
        })?;
        out.push(RuleMatchEntry {
            id: row.id,
            rule_id: row.rule_id,
            rule_name: row.rule_name,
            inbox_event_id: row.inbox_event_id,
            project_id: row.project_id,
            action_count: row.action_count.max(0) as u32,
            error: row.error,
            created_at: row.created_at.max(0) as u64,
            action_outbox_ids,
        });
    }
    Ok(out)
}

fn read_match(r: &rusqlite::Row) -> rusqlite::Result<RawMatch> {
    Ok(RawMatch {
        id: r.get(0)?,
        rule_id: r.get(1)?,
        rule_name: r.get(2)?,
        inbox_event_id: r.get(3)?,
        project_id: r.get(4)?,
        action_count: r.get(5)?,
        error: r.get(6)?,
        created_at: r.get(7)?,
    })
}

struct RawMatch {
    id: i64,
    rule_id: String,
    rule_name: String,
    inbox_event_id: i64,
    project_id: String,
    action_count: i64,
    error: Option<String>,
    created_at: i64,
}

pub fn get(db: &Database, id: i64) -> AppResult<Option<RuleMatchEntry>> {
    let row = db.with_conn(|conn| {
        conn.query_row(
            "SELECT id, rule_id, rule_name, inbox_event_id, project_id, action_count, error, created_at \
             FROM rule_match WHERE id = ?1",
            [id],
            read_match,
        )
        .optional()
    })?;
    match row {
        Some(row) => hydrate_actions(db, vec![row]).map(|mut rows| rows.pop()),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_inbox_and_outbox(db: &Database) -> (i64, i64) {
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO inbox_event \
                 (dedupe_key, source, event_type, project_id, repo, number, event_json, raw_payload, status, received_at_epoch) \
                 VALUES ('k', 'github', 'pullRequest', 'p1', 'owner/repo', 1, '{}', '{}', 'received', 10)",
                [],
            )?;
            let inbox_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO action_outbox \
                 (project_id, kind, summary, payload, status, next_attempt_at, created_at, updated_at, producer_key) \
                 VALUES ('p1', 'review', 'summary', '{}', 'pending', 10, 10, 10, 'rule-store-test')",
                [],
            )?;
            let outbox_id = conn.last_insert_rowid();
            Ok((inbox_id, outbox_id))
        })
        .expect("seed rows")
    }

    #[test]
    fn insert_and_list_match_links_actions() {
        let db = Database::open_in_memory().expect("open db");
        let (inbox_id, outbox_id) = seed_inbox_and_outbox(&db);

        let id = insert_match(
            &db,
            NewRuleMatch {
                rule_id: "r1",
                rule_name: "Ready",
                inbox_event_id: inbox_id,
                project_id: "p1",
                action_ids: &[outbox_id],
                error: None,
                now: 123,
            },
        )
        .expect("insert match");

        let entry = get(&db, id).expect("get").expect("present");
        assert_eq!(entry.rule_id, "r1");
        assert_eq!(entry.inbox_event_id, inbox_id);
        assert_eq!(entry.action_outbox_ids, vec![outbox_id]);
        assert_eq!(entry.action_count, 1);

        let listed = list_by_inbox(&db, inbox_id).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
    }

    #[test]
    fn foreign_keys_reject_orphan_trace_rows() {
        let db = Database::open_in_memory().expect("open db");
        assert!(insert_match(
            &db,
            NewRuleMatch {
                rule_id: "r1",
                rule_name: "Ready",
                inbox_event_id: 999,
                project_id: "p1",
                action_ids: &[],
                error: None,
                now: 1,
            },
        )
        .is_err());

        let (inbox_id, _outbox_id) = seed_inbox_and_outbox(&db);
        assert!(insert_match(
            &db,
            NewRuleMatch {
                rule_id: "r1",
                rule_name: "Ready",
                inbox_event_id: inbox_id,
                project_id: "p1",
                action_ids: &[999],
                error: None,
                now: 1,
            },
        )
        .is_err());
    }
}
