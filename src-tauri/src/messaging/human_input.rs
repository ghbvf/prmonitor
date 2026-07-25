//! Durable two-channel human input broker (#1810).
//!
//! Codex elicitation and a messaging card/text reply (Feishu or DingTalk) race through
//! [`answer`]. SQLite's conditional update is the authority: exactly one source changes
//! `pending`, and every later answer reads the already committed winner. The in-memory
//! notifier is only a latency optimization.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Deref;
use std::str::FromStr;
use std::sync::Mutex;

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use crate::db::{map_err, Database};
use crate::error::{AppError, AppResult};

pub const MAX_WAIT_SECONDS: u64 = 3_600;
pub const TERMINAL_RETENTION_SECONDS: u64 = 7 * 24 * 60 * 60;
pub const MAX_PENDING_GLOBAL: usize = 100;
pub const MAX_PENDING_PER_INTEGRATION: usize = 50;
pub const MAX_PENDING_PER_CONVERSATION: usize = 10;
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024;

const MAX_REQUEST_ID_BYTES: usize = 96;
const MAX_INTEGRATION_ID_BYTES: usize = 128;
const MAX_CONVERSATION_ID_BYTES: usize = 256;
const MAX_PURPOSE_BYTES: usize = 128;
const MAX_TITLE_BYTES: usize = 256;
const MAX_QUESTION_ID_BYTES: usize = 64;
const MAX_QUESTION_BYTES: usize = 1_024;
const MAX_OPTIONS: usize = 20;
const MAX_OPTION_BYTES: usize = 256;
/// Free-form answers (Feishu custom / Codex / `/answer`) share one bound with options, plus headroom.
const MAX_ANSWER_BYTES: usize = 1_024;
const MAX_CONTEXT_BYTES: usize = 16 * 1024;
const MAX_TOTAL_PAYLOAD_BYTES: usize = 32 * 1024;

const SELECT_REQUEST: &str = "SELECT id,integration_id,conversation_id,purpose,title,message,questions_json,context_json,status,answer_json,answer_source,card_message_id,created_at_epoch,expires_at_epoch,answered_at_epoch FROM human_input_request";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HumanInputStatus {
    Pending,
    Answered,
    Cancelled,
    Expired,
}

impl HumanInputStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Answered => "answered",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }
}

impl Deref for HumanInputStatus {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for HumanInputStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for HumanInputStatus {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "answered" => Ok(Self::Answered),
            "cancelled" => Ok(Self::Cancelled),
            "expired" => Ok(Self::Expired),
            _ => Err(AppError::new(format!("human input status 非法: {value}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum HumanAnswerSource {
    #[serde(rename = "feishu")]
    Feishu,
    #[serde(rename = "dingTalk")]
    DingTalk,
    #[serde(rename = "codex")]
    Codex,
}

impl HumanAnswerSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Feishu => "feishu",
            Self::DingTalk => "dingTalk",
            Self::Codex => "codex",
        }
    }
}

impl Deref for HumanAnswerSource {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for HumanAnswerSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for HumanAnswerSource {
    type Err = AppError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "feishu" => Ok(Self::Feishu),
            "dingTalk" | "ding_talk" => Ok(Self::DingTalk),
            "codex" => Ok(Self::Codex),
            _ => Err(AppError::new(format!(
                "human input answer source 非法: {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HumanQuestion {
    /// Stable id within this request (≤64 bytes; `[A-Za-z0-9._:-]`).
    pub id: String,
    /// Question text shown on the card / popup (≤1024 UTF-8 bytes).
    pub question: String,
    /// Plain string labels only (`string[]`). Do NOT pass `{label, description}` objects — that fails deserialization with "map, expected a string".
    #[serde(default)]
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HumanAnswer {
    pub question_id: String,
    pub answer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HumanInputRequest {
    pub id: String,
    pub integration_id: String,
    pub conversation_id: String,
    pub purpose: String,
    pub title: String,
    pub message: String,
    pub questions: Vec<HumanQuestion>,
    pub context: Value,
    pub status: HumanInputStatus,
    pub answer: Option<Vec<HumanAnswer>>,
    pub answer_source: Option<HumanAnswerSource>,
    pub card_message_id: Option<String>,
    pub created_at_epoch: u64,
    pub expires_at_epoch: u64,
    pub answered_at_epoch: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerOutcome {
    Won,
    AlreadyAnswered { source: Option<HumanAnswerSource> },
}

#[derive(Default)]
pub struct HumanInputBroker {
    waiters: Mutex<HashMap<String, watch::Sender<Option<HumanInputRequest>>>>,
}

impl HumanInputBroker {
    pub fn subscribe(&self, id: &str) -> watch::Receiver<Option<HumanInputRequest>> {
        let mut waiters = self.waiters.lock().unwrap_or_else(|p| p.into_inner());
        waiters
            .entry(id.to_string())
            .or_insert_with(|| watch::channel(None).0)
            .subscribe()
    }

    pub fn publish(&self, request: HumanInputRequest) {
        let sender = self
            .waiters
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&request.id);
        if let Some(sender) = sender {
            let _ = sender.send(Some(request));
        }
    }

    pub fn remove(&self, id: &str) {
        self.waiters
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
    }
}

pub fn create(db: &Database, request: &HumanInputRequest) -> AppResult<()> {
    validate_request(request)?;
    let questions = serde_json::to_string(&request.questions)
        .map_err(|e| AppError::new(format!("questions 序列化失败: {e}")))?;
    let context = serde_json::to_string(&request.context)
        .map_err(|e| AppError::new(format!("context 序列化失败: {e}")))?;
    db.with_tx(|tx| {
        prune_terminal_tx(tx, request.created_at_epoch)?;
        let global_pending = tx
            .query_row(
                "SELECT COUNT(*) FROM human_input_request WHERE status='pending'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_err)?;
        if global_pending >= MAX_PENDING_GLOBAL as i64 {
            return Err(AppError::new("等待中的人工输入请求已达到全局上限"));
        }
        let integration_pending = tx
            .query_row(
                "SELECT COUNT(*) FROM human_input_request WHERE status='pending' AND integration_id=?1",
                [request.integration_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_err)?;
        if integration_pending >= MAX_PENDING_PER_INTEGRATION as i64 {
            return Err(AppError::new("当前集成等待中的人工输入请求已达到上限"));
        }
        let conversation_pending = tx
            .query_row(
                "SELECT COUNT(*) FROM human_input_request WHERE status='pending' AND integration_id=?1 AND conversation_id=?2",
                rusqlite::params![request.integration_id, request.conversation_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_err)?;
        if conversation_pending >= MAX_PENDING_PER_CONVERSATION as i64 {
            return Err(AppError::new("当前集成/会话等待中的人工输入请求已达到上限"));
        }
        tx.execute(
            "INSERT INTO human_input_request (id,integration_id,conversation_id,purpose,title,message,questions_json,context_json,status,created_at_epoch,expires_at_epoch) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'pending',?9,?10)",
            rusqlite::params![request.id, request.integration_id, request.conversation_id,
                request.purpose, request.title, request.message, questions, context,
                request.created_at_epoch as i64, request.expires_at_epoch as i64],
        )
        .map_err(map_err)?;
        Ok(())
    })
}

fn validate_request(request: &HumanInputRequest) -> AppResult<()> {
    if request.status != HumanInputStatus::Pending
        || request.answer.is_some()
        || request.answer_source.is_some()
        || request.answered_at_epoch.is_some()
    {
        return Err(AppError::new("新建人工输入请求必须处于纯 pending 状态"));
    }
    validate_identifier("request id", &request.id, MAX_REQUEST_ID_BYTES)?;
    validate_identifier(
        "integration id",
        &request.integration_id,
        MAX_INTEGRATION_ID_BYTES,
    )?;
    validate_identifier(
        "conversation id",
        &request.conversation_id,
        MAX_CONVERSATION_ID_BYTES,
    )?;
    validate_text("purpose", &request.purpose, MAX_PURPOSE_BYTES, false)?;
    validate_text("title", &request.title, MAX_TITLE_BYTES, false)?;
    validate_text("message", &request.message, MAX_MESSAGE_BYTES, false)?;
    let timeout = request
        .expires_at_epoch
        .checked_sub(request.created_at_epoch)
        .ok_or_else(|| AppError::new("expiresAt 必须晚于 createdAt"))?;
    if !(1..=MAX_WAIT_SECONDS).contains(&timeout) {
        return Err(AppError::new(format!(
            "timeoutSecs 必须在 1-{MAX_WAIT_SECONDS} 秒之间"
        )));
    }
    if request.questions.is_empty() || request.questions.len() > 3 {
        return Err(AppError::new("questions 必须包含 1-3 个问题"));
    }
    let mut question_ids = HashSet::new();
    let mut total_bytes = request.purpose.len()
        + request.title.len()
        + request.message.len()
        + request.integration_id.len()
        + request.conversation_id.len();
    for question in &request.questions {
        validate_identifier("question id", &question.id, MAX_QUESTION_ID_BYTES)?;
        if !question_ids.insert(question.id.as_str()) {
            return Err(AppError::new("human input question id 必须唯一"));
        }
        validate_text("question", &question.question, MAX_QUESTION_BYTES, false)?;
        if question.options.len() > MAX_OPTIONS {
            return Err(AppError::new(format!(
                "每个问题最多包含 {MAX_OPTIONS} 个选项"
            )));
        }
        let mut options = HashSet::new();
        for option in &question.options {
            validate_text("option", option, MAX_OPTION_BYTES, false)?;
            if !options.insert(option.as_str()) {
                return Err(AppError::new("同一问题的选项必须唯一"));
            }
            total_bytes = total_bytes.saturating_add(option.len());
        }
        total_bytes = total_bytes
            .saturating_add(question.id.len())
            .saturating_add(question.question.len());
    }
    let context_bytes = serde_json::to_vec(&request.context)
        .map_err(|error| AppError::new(format!("context 序列化失败: {error}")))?;
    if context_bytes.len() > MAX_CONTEXT_BYTES {
        return Err(AppError::new(format!(
            "context 超过 {MAX_CONTEXT_BYTES} 字节上限"
        )));
    }
    total_bytes = total_bytes.saturating_add(context_bytes.len());
    if total_bytes > MAX_TOTAL_PAYLOAD_BYTES {
        return Err(AppError::new(format!(
            "人工输入请求超过 {MAX_TOTAL_PAYLOAD_BYTES} 字节总上限"
        )));
    }
    Ok(())
}

fn validate_identifier(label: &str, value: &str, max_bytes: usize) -> AppResult<()> {
    if value.is_empty()
        || value.len() > max_bytes
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(AppError::new(format!("{label} 非法")));
    }
    Ok(())
}

fn validate_text(label: &str, value: &str, max_bytes: usize, allow_empty: bool) -> AppResult<()> {
    if (!allow_empty && value.trim().is_empty())
        || value.len() > max_bytes
        || value.chars().any(|character| character == '\0')
    {
        return Err(AppError::new(format!(
            "{label} 非法或超过 {max_bytes} 字节上限"
        )));
    }
    Ok(())
}

pub fn set_card_message_id(db: &Database, id: &str, message_id: &str) -> AppResult<()> {
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE human_input_request SET card_message_id=?2 WHERE id=?1 AND status='pending'",
            rusqlite::params![id, message_id],
        )?;
        Ok(())
    })
}

pub fn answer(
    db: &Database,
    broker: &HumanInputBroker,
    id: &str,
    answers: &[HumanAnswer],
    source: HumanAnswerSource,
    now: u64,
) -> AppResult<AnswerOutcome> {
    let request =
        get(db, id)?.ok_or_else(|| AppError::new(format!("human input request 不存在: {id}")))?;
    let answers = validate_answers(&request, answers)?;
    let answer_json = serde_json::to_string(&answers)
        .map_err(|e| AppError::new(format!("answer 序列化失败: {e}")))?;
    let changed = db.with_conn(|conn| {
        conn.execute(
            "UPDATE human_input_request SET status='answered',answer_json=?2,answer_source=?3,answered_at_epoch=?4 WHERE id=?1 AND status='pending' AND expires_at_epoch>=?4",
            rusqlite::params![id, answer_json, source.as_str(), now as i64],
        )
    })?;
    let request =
        get(db, id)?.ok_or_else(|| AppError::new(format!("human input request 不存在: {id}")))?;
    if changed == 1 {
        broker.publish(request);
        Ok(AnswerOutcome::Won)
    } else {
        Ok(AnswerOutcome::AlreadyAnswered {
            source: request.answer_source,
        })
    }
}

fn validate_answers(
    request: &HumanInputRequest,
    answers: &[HumanAnswer],
) -> AppResult<Vec<HumanAnswer>> {
    let mut questions = HashMap::new();
    for question in &request.questions {
        let id = question.id.trim();
        if id.is_empty() || questions.insert(id, question).is_some() {
            return Err(AppError::new("human input question id 必须非空且唯一"));
        }
    }
    if answers.len() != questions.len() {
        return Err(AppError::new("必须完整回答每一个问题"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut normalized = Vec::with_capacity(answers.len());
    for answer in answers {
        let question_id = answer.question_id.trim();
        let value = answer.answer.trim();
        if question_id.is_empty() || !seen.insert(question_id) {
            return Err(AppError::new("answer questionId 必须非空且唯一"));
        }
        questions
            .get(question_id)
            .ok_or_else(|| AppError::new(format!("未知 questionId: {question_id}")))?;
        if value.is_empty() {
            return Err(AppError::new(format!("问题 {question_id} 的答案不能为空")));
        }
        if value.len() > MAX_ANSWER_BYTES || value.chars().any(|character| character == '\0') {
            return Err(AppError::new(format!(
                "问题 {question_id} 的答案非法或超过 {MAX_ANSWER_BYTES} 字节上限"
            )));
        }
        normalized.push(HumanAnswer {
            question_id: question_id.to_string(),
            answer: value.to_string(),
        });
    }
    Ok(normalized)
}

pub fn cancel(db: &Database, broker: &HumanInputBroker, id: &str, now: u64) -> AppResult<bool> {
    let changed = db.with_conn(|conn| {
        conn.execute(
            "UPDATE human_input_request SET status='cancelled',answered_at_epoch=?2 WHERE id=?1 AND status='pending'",
            rusqlite::params![id, now as i64],
        )
    })?;
    if changed == 1 {
        if let Some(request) = get(db, id)? {
            broker.publish(request);
        }
    }
    Ok(changed == 1)
}

pub fn expire(db: &Database, broker: &HumanInputBroker, id: &str, now: u64) -> AppResult<bool> {
    let changed = db.with_conn(|conn| {
        conn.execute(
            "UPDATE human_input_request SET status='expired',answered_at_epoch=?2 WHERE id=?1 AND status='pending'",
            rusqlite::params![id, now as i64],
        )
    })?;
    if changed == 1 {
        if let Some(request) = get(db, id)? {
            broker.publish(request);
        }
    }
    Ok(changed == 1)
}

/// Marks every overdue pending request terminal and wakes any live MCP waiter. The conditional
/// update makes startup reconciliation safe to race with an answer arriving from Feishu.
pub fn expire_due(
    db: &Database,
    broker: &HumanInputBroker,
    now: u64,
) -> AppResult<Vec<HumanInputRequest>> {
    let ids = db.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id FROM human_input_request WHERE status='pending' AND expires_at_epoch < ?1 ORDER BY created_at_epoch",
        )?;
        let ids = stmt
            .query_map([now as i64], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    })?;
    let mut expired = Vec::new();
    for id in ids {
        let changed = db.with_conn(|conn| {
            conn.execute(
                "UPDATE human_input_request SET status='expired',answered_at_epoch=?2 WHERE id=?1 AND status='pending' AND expires_at_epoch < ?2",
                rusqlite::params![id, now as i64],
            )
        })?;
        if changed == 1 {
            if let Some(request) = get(db, &id)? {
                broker.publish(request.clone());
                expired.push(request);
            }
        }
    }
    Ok(expired)
}

/// A process restart destroys all MCP waiters. Terminalize every request owned by the previous
/// process before accepting new MCP sessions so a live Feishu card cannot claim it will resume a
/// future that no longer exists.
pub fn reconcile_startup_orphans(
    db: &Database,
    broker: &HumanInputBroker,
    now: u64,
) -> AppResult<Vec<HumanInputRequest>> {
    let rows = db.with_tx(|tx| {
        let rows = {
            let mut stmt = tx
                .prepare(&format!(
                    "{SELECT_REQUEST} WHERE status='pending' ORDER BY created_at_epoch"
                ))
                .map_err(map_err)?;
            let rows = stmt
                .query_map([], hydrate)
                .map_err(map_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(map_err)?;
            rows
        };
        tx.execute(
            "UPDATE human_input_request SET status='cancelled', answered_at_epoch=?1 WHERE status='pending'",
            [now as i64],
        )
        .map_err(map_err)?;
        Ok(rows)
    })?;
    let mut reconciled = Vec::with_capacity(rows.len());
    for row in rows {
        let mut request = row.into_request()?;
        request.status = HumanInputStatus::Cancelled;
        request.answered_at_epoch = Some(now);
        broker.publish(request.clone());
        reconciled.push(request);
    }
    Ok(reconciled)
}

pub fn prune_terminal(db: &Database, now: u64) -> AppResult<usize> {
    db.with_conn(|conn| prune_terminal_conn(conn, now))
}

fn prune_terminal_tx(tx: &rusqlite::Transaction<'_>, now: u64) -> AppResult<usize> {
    prune_terminal_conn(tx, now).map_err(map_err)
}

fn prune_terminal_conn(conn: &rusqlite::Connection, now: u64) -> rusqlite::Result<usize> {
    let cutoff = now.saturating_sub(TERMINAL_RETENTION_SECONDS);
    conn.execute(
        "DELETE FROM human_input_request WHERE status!='pending' AND answered_at_epoch IS NOT NULL AND answered_at_epoch < ?1",
        [cutoff as i64],
    )
}

pub fn get(db: &Database, id: &str) -> AppResult<Option<HumanInputRequest>> {
    db.with_conn(|conn| {
        conn.query_row(&format!("{SELECT_REQUEST} WHERE id=?1"), [id], hydrate)
            .optional()
    })
    .and_then(|row| row.map(RawRequest::into_request).transpose())
}

pub fn latest_pending_for_conversation(
    db: &Database,
    integration_id: &str,
    conversation_id: &str,
) -> AppResult<Option<HumanInputRequest>> {
    db.with_conn(|conn| {
        conn.query_row(
            &format!("{SELECT_REQUEST} WHERE integration_id=?1 AND conversation_id=?2 AND status='pending' ORDER BY created_at_epoch DESC LIMIT 1"),
            rusqlite::params![integration_id, conversation_id],
            hydrate,
        )
        .optional()
    })
    .and_then(|row| row.map(RawRequest::into_request).transpose())
}

fn hydrate(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRequest> {
    Ok(RawRequest {
        id: row.get(0)?,
        integration_id: row.get(1)?,
        conversation_id: row.get(2)?,
        purpose: row.get(3)?,
        title: row.get(4)?,
        message: row.get(5)?,
        questions_json: row.get(6)?,
        context_json: row.get(7)?,
        status: row.get(8)?,
        answer_json: row.get(9)?,
        answer_source: row.get(10)?,
        card_message_id: row.get(11)?,
        created_at_epoch: row.get(12)?,
        expires_at_epoch: row.get(13)?,
        answered_at_epoch: row.get(14)?,
    })
}

struct RawRequest {
    id: String,
    integration_id: String,
    conversation_id: String,
    purpose: String,
    title: String,
    message: String,
    questions_json: String,
    context_json: String,
    status: String,
    answer_json: Option<String>,
    answer_source: Option<String>,
    card_message_id: Option<String>,
    created_at_epoch: i64,
    expires_at_epoch: i64,
    answered_at_epoch: Option<i64>,
}

impl RawRequest {
    fn into_request(self) -> AppResult<HumanInputRequest> {
        Ok(HumanInputRequest {
            id: self.id,
            integration_id: self.integration_id,
            conversation_id: self.conversation_id,
            purpose: self.purpose,
            title: self.title,
            message: self.message,
            questions: serde_json::from_str(&self.questions_json)
                .map_err(|e| AppError::new(format!("questions JSON 损坏: {e}")))?,
            context: serde_json::from_str(&self.context_json)
                .map_err(|e| AppError::new(format!("context JSON 损坏: {e}")))?,
            status: HumanInputStatus::from_str(&self.status)?,
            answer: self
                .answer_json
                .map(|value| serde_json::from_str(&value))
                .transpose()
                .map_err(|e| AppError::new(format!("answer JSON 损坏: {e}")))?,
            answer_source: self
                .answer_source
                .map(|source| HumanAnswerSource::from_str(&source))
                .transpose()?,
            card_message_id: self.card_message_id,
            created_at_epoch: self.created_at_epoch.max(0) as u64,
            expires_at_epoch: self.expires_at_epoch.max(0) as u64,
            answered_at_epoch: self.answered_at_epoch.map(|value| value.max(0) as u64),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn pending(id: &str) -> HumanInputRequest {
        HumanInputRequest {
            id: id.into(),
            integration_id: "feishu-main".into(),
            conversation_id: "oc_1".into(),
            purpose: "question".into(),
            title: "Choose".into(),
            message: "Pick".into(),
            questions: vec![HumanQuestion {
                id: "q1".into(),
                question: "Which?".into(),
                options: vec!["A".into(), "B".into()],
            }],
            context: serde_json::json!({}),
            status: HumanInputStatus::Pending,
            answer: None,
            answer_source: None,
            card_message_id: None,
            created_at_epoch: 10,
            expires_at_epoch: 100,
            answered_at_epoch: None,
        }
    }

    #[test]
    fn first_answer_wins_atomically() {
        let db = Database::open_in_memory().unwrap();
        let broker = HumanInputBroker::default();
        create(&db, &pending("Q-test")).unwrap();
        let answers = vec![HumanAnswer {
            question_id: "q1".into(),
            answer: "A".into(),
        }];
        assert_eq!(
            answer(
                &db,
                &broker,
                "Q-test",
                &answers,
                HumanAnswerSource::Feishu,
                20,
            )
            .unwrap(),
            AnswerOutcome::Won
        );
        assert_eq!(
            answer(
                &db,
                &broker,
                "Q-test",
                &answers,
                HumanAnswerSource::Codex,
                21,
            )
            .unwrap(),
            AnswerOutcome::AlreadyAnswered {
                source: Some(HumanAnswerSource::Feishu)
            }
        );
        assert_eq!(
            get(&db, "Q-test")
                .unwrap()
                .unwrap()
                .answer_source
                .as_deref(),
            Some("feishu")
        );
    }

    #[test]
    fn dingtalk_first_answer_wins_and_serde_wire_is_ding_talk() {
        let db = Database::open_in_memory().unwrap();
        let broker = HumanInputBroker::default();
        create(&db, &pending("Q-dt")).unwrap();
        let answers = vec![HumanAnswer {
            question_id: "q1".into(),
            answer: "A".into(),
        }];
        assert_eq!(
            answer(
                &db,
                &broker,
                "Q-dt",
                &answers,
                HumanAnswerSource::DingTalk,
                20,
            )
            .unwrap(),
            AnswerOutcome::Won
        );
        let stored = get(&db, "Q-dt").unwrap().unwrap();
        assert_eq!(stored.answer_source, Some(HumanAnswerSource::DingTalk));
        assert_eq!(
            serde_json::to_value(HumanAnswerSource::DingTalk).unwrap(),
            serde_json::json!("dingTalk")
        );
        assert_eq!(HumanAnswerSource::DingTalk.as_str(), "dingTalk");
    }

    #[test]
    fn rejects_incomplete_unknown_and_empty_answers() {
        let db = Database::open_in_memory().unwrap();
        let broker = HumanInputBroker::default();
        let mut request = pending("Q-valid");
        request.questions.push(HumanQuestion {
            id: "q2".into(),
            question: "Confirm?".into(),
            options: vec!["yes".into(), "no".into()],
        });
        create(&db, &request).unwrap();

        for answers in [
            vec![],
            vec![HumanAnswer {
                question_id: "missing".into(),
                answer: "A".into(),
            }],
            vec![HumanAnswer {
                question_id: "q1".into(),
                answer: " ".into(),
            }],
        ] {
            assert!(answer(
                &db,
                &broker,
                "Q-valid",
                &answers,
                HumanAnswerSource::Feishu,
                20,
            )
            .is_err());
            assert_eq!(
                get(&db, "Q-valid").unwrap().unwrap().status,
                HumanInputStatus::Pending
            );
        }

        let oversized = "x".repeat(MAX_ANSWER_BYTES + 1);
        assert!(answer(
            &db,
            &broker,
            "Q-valid",
            &[
                HumanAnswer {
                    question_id: "q1".into(),
                    answer: oversized.clone(),
                },
                HumanAnswer {
                    question_id: "q2".into(),
                    answer: "yes".into(),
                },
            ],
            HumanAnswerSource::Feishu,
            20,
        )
        .is_err());
    }

    #[test]
    fn accepts_custom_answer_outside_suggested_options() {
        let db = Database::open_in_memory().unwrap();
        let broker = HumanInputBroker::default();
        create(&db, &pending("Q-custom")).unwrap();

        assert_eq!(
            answer(
                &db,
                &broker,
                "Q-custom",
                &[HumanAnswer {
                    question_id: "q1".into(),
                    answer: "user supplied answer".into(),
                }],
                HumanAnswerSource::Feishu,
                20,
            )
            .unwrap(),
            AnswerOutcome::Won
        );
        assert_eq!(
            get(&db, "Q-custom").unwrap().unwrap().answer.unwrap()[0].answer,
            "user supplied answer"
        );
    }

    #[test]
    fn expires_due_pending_requests_and_notifies_waiters() {
        let db = Database::open_in_memory().unwrap();
        let broker = HumanInputBroker::default();
        create(&db, &pending("Q-expire")).unwrap();
        let receiver = broker.subscribe("Q-expire");

        let expired = expire_due(&db, &broker, 101).unwrap();

        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].status, HumanInputStatus::Expired);
        assert_eq!(
            receiver
                .borrow()
                .as_ref()
                .map(|request| request.status.as_str()),
            Some("expired")
        );
        assert_eq!(
            get(&db, "Q-expire").unwrap().unwrap().status,
            HumanInputStatus::Expired
        );
    }

    #[test]
    fn admission_rejects_invalid_ids_oversized_payload_timeout_and_pending_quota() {
        let db = Database::open_in_memory().unwrap();

        let mut invalid_id = pending("Q-invalid");
        invalid_id.questions[0].id = "bad id".into();
        assert!(create(&db, &invalid_id).is_err());

        let mut oversized = pending("Q-oversized");
        oversized.message = "x".repeat(MAX_MESSAGE_BYTES + 1);
        assert!(create(&db, &oversized).is_err());

        let mut timeout = pending("Q-timeout");
        timeout.expires_at_epoch = timeout
            .created_at_epoch
            .saturating_add(MAX_WAIT_SECONDS + 1);
        assert!(create(&db, &timeout).is_err());

        for index in 0..MAX_PENDING_PER_CONVERSATION {
            create(&db, &pending(&format!("Q-quota-{index}"))).unwrap();
        }
        assert!(create(&db, &pending("Q-quota-overflow")).is_err());

        for index in MAX_PENDING_PER_CONVERSATION..MAX_PENDING_PER_INTEGRATION {
            let mut request = pending(&format!("Q-integration-{index}"));
            request.conversation_id = format!("oc_{index}");
            create(&db, &request).unwrap();
        }
        let mut integration_overflow = pending("Q-integration-overflow");
        integration_overflow.conversation_id = "oc_integration_overflow".into();
        assert!(create(&db, &integration_overflow).is_err());

        for index in MAX_PENDING_PER_INTEGRATION..MAX_PENDING_GLOBAL {
            let mut request = pending(&format!("Q-global-{index}"));
            request.integration_id = format!("feishu-{index}");
            request.conversation_id = format!("oc_global_{index}");
            create(&db, &request).unwrap();
        }
        let mut global_overflow = pending("Q-global-overflow");
        global_overflow.integration_id = "feishu-overflow".into();
        global_overflow.conversation_id = "oc_global_overflow".into();
        assert!(create(&db, &global_overflow).is_err());
    }

    #[test]
    fn startup_reconciliation_terminalizes_orphans_and_retention_prunes_old_rows() {
        let db = Database::open_in_memory().unwrap();
        let broker = HumanInputBroker::default();
        create(&db, &pending("Q-orphan")).unwrap();

        let reconciled = reconcile_startup_orphans(&db, &broker, 50).unwrap();
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].status, HumanInputStatus::Cancelled);
        assert_eq!(reconciled[0].answered_at_epoch, Some(50));

        let deleted = prune_terminal(&db, 50 + TERMINAL_RETENTION_SECONDS + 1).unwrap();
        assert_eq!(deleted, 1);
        assert!(get(&db, "Q-orphan").unwrap().is_none());
    }

    #[test]
    fn malformed_persisted_status_and_source_are_rejected_during_hydration() {
        let db = Database::open_in_memory().unwrap();
        create(&db, &pending("Q-typed")).unwrap();
        db.with_conn(|conn| {
            conn.execute("PRAGMA ignore_check_constraints=ON", [])?;
            conn.execute(
                "UPDATE human_input_request SET status='mystery', answer_source='stranger' WHERE id='Q-typed'",
                [],
            )?;
            Ok(())
        })
        .unwrap();

        assert!(get(&db, "Q-typed").is_err());
    }

    #[test]
    fn concurrent_answers_have_exactly_one_committed_winner() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let broker = Arc::new(HumanInputBroker::default());
        create(&db, &pending("Q-race")).unwrap();
        let barrier = Arc::new(Barrier::new(3));

        let mut joins = Vec::new();
        for (source, value) in [
            (HumanAnswerSource::Feishu, "A"),
            (HumanAnswerSource::Codex, "B"),
        ] {
            let db = Arc::clone(&db);
            let broker = Arc::clone(&broker);
            let barrier = Arc::clone(&barrier);
            joins.push(thread::spawn(move || {
                barrier.wait();
                answer(
                    &db,
                    &broker,
                    "Q-race",
                    &[HumanAnswer {
                        question_id: "q1".into(),
                        answer: value.into(),
                    }],
                    source,
                    20,
                )
                .unwrap()
            }));
        }
        barrier.wait();
        let outcomes = joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, AnswerOutcome::Won))
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, AnswerOutcome::AlreadyAnswered { .. }))
                .count(),
            1
        );
        let stored = get(&db, "Q-race").unwrap().unwrap();
        assert_eq!(stored.status, HumanInputStatus::Answered);
        assert!(matches!(
            stored.answer_source,
            Some(
                HumanAnswerSource::Feishu | HumanAnswerSource::DingTalk | HumanAnswerSource::Codex
            )
        ));
    }
}
