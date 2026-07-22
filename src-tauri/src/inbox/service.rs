//! Inbox ingress + worker processing logic (AB#1065/#1379): the composition-facing body the root installs as
//! the webhook ingestor / refresher / rule processor.
//!
//! **Decoupled from `pr`.** This module names NO `pr`-internal type. It works only on the neutral
//! [`crate::model::EventEnvelope`] and OPAQUE composition-root-injected closures
//! ([`crate::inbox::GithubRefeed`] / [`crate::inbox::AzureRefresh`] /
//! [`crate::inbox::RuleProcessor`]). The composition root normalizes a
//! `pr::webhook::WebhookEvent` into a `model::EventEnvelope` (via `pr::webhook::event_from_webhook`) BEFORE
//! calling [`ingest_github`], and the closures it injects are the only path back into other slices
//! (re-feed via `pr::commands::ingest_webhook`, re-discovery via `az`, rule processing via outbox).
//!
//! **Funnel (upstream Hard, downstream durable).** The inbox's UPSTREAM gate is the
//! `inbox_event.UNIQUE(dedupe_key)` constraint (the **Hard** ingress-idempotency carrier — a
//! re-delivered webhook is unexpressible as a second row, see [`crate::inbox::store::insert_dedup`]).
//! The DOWNSTREAM review/check dedup gates are the outbox's permanent `producer_key` and the review
//! executor's durable claim. The inbox dedups deliveries / stored producer inputs; outbox + executor
//! dedup review execution.

use sha2::{Digest, Sha256};
use tauri::{Emitter, Manager};

use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::events::{InboxErrorOperation, InboxEvent, INBOX_UPDATED_EVENT};
use crate::inbox::store;
use crate::inbox::{AzureRefresh, GithubRefeed, RuleProcessor};
use crate::model::{
    Candidate, EventEnvelope, EventSubject, EventType, InboxDedupeKey, InboxEntry, SourceKind,
};

/// Lowercase hex SHA-256 of `bytes` — the Azure audit dedupe-key body hash (AB#1065). Reuses the
/// `sha2` crate already in the dependency tree (no new dependency). The GitHub dedupe key is built
/// in `pr::webhook::event_from_webhook` (pr owns `WebhookEvent`); the inbox only builds the Azure
/// audit key here from the raw body.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Build the Azure AUDIT [`Event`] for a refresh delivery (AB#1065). PURE. Azure Service Hooks
/// are a refresh SIGNAL (no PR fields the inbox classifies — see `pr::webhook`), so this is a
/// [`EventType::Generic`] audit record: `dedupe_key` = `azure:sha256:{body-hash}` (a re-delivered
/// identical body dedups), `number: None`, `title`/`labels`/`url` empty. It exists so the inbox
/// is a complete record of EVERY delivery, GitHub and Azure alike. Built from the raw body + route
/// identity ONLY — no `pr` type involved.
pub fn normalize_azure(raw: &str, project_id: &str, repo: &str, now: u64) -> EventEnvelope {
    EventEnvelope::observation(
        InboxDedupeKey::new(format!("azure:sha256:{}", sha256_hex(raw.as_bytes())))
            .expect("hash key"),
        SourceKind::Azure,
        project_id,
        repo,
        EventType::Generic,
        EventSubject {
            number: None,
            title: String::new(),
            body: String::new(),
            labels: Vec::new(),
            url: String::new(),
        },
        now,
    )
    .expect("azure event has project id")
}

/// Whether an [`store::insert_dedup`] result means "process this delivery" (AB#1065): `Some(id)`
/// (a NEW row) → process; `None` (a duplicate `dedupe_key`) → skip. The production path embodies
/// this decision inline (`ingest_github`'s `let Some(id) = inserted else { return Ok(()) }`); this
/// named predicate exists so the dedup decision is unit-tested without an `AppHandle`. Test-only.
#[cfg(test)]
fn should_process(insert_result: Option<i64>) -> bool {
    insert_result.is_some()
}

/// Emit `inbox:updated` for one entry (best-effort — a gone window is not an error). Looks the
/// row up by id so the emit always carries the CURRENT persisted state (post-transition).
fn emit_updated<R: tauri::Runtime>(app: &tauri::AppHandle<R>, project_id: &str, entry: InboxEntry) {
    let _ = app.emit(
        INBOX_UPDATED_EVENT,
        &InboxEvent::Updated {
            project_id: project_id.to_string(),
            entry: Box::new(entry),
        },
    );
}

/// Load entry `id` and emit `inbox:updated` for it (best-effort). A missing row / store error does
/// NOT fail the caller — the emit is a UI nicety, not a correctness gate — but a store ERROR is
/// logged (not silently swallowed) so a persistent read failure is diagnosable.
pub(crate) fn emit_for_id<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    db: &Database,
    project_id: &str,
    id: i64,
) {
    match store::get_entry(db, id) {
        Ok(Some(entry)) => emit_updated(app, project_id, entry),
        Ok(None) => {} // row gone (e.g. pruned) — nothing to emit.
        Err(e) => eprintln!(
            "inbox: 读取条目以发送 inbox:updated 失败（id={id}）：{}",
            e.message
        ),
    }
}

pub(crate) fn announce_worker_error<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    operation: &str,
    message: &str,
) {
    let operation = match operation {
        "retention" => InboxErrorOperation::Retention,
        _ => InboxErrorOperation::Worker,
    };
    let _ = app.emit(
        INBOX_UPDATED_EVENT,
        InboxEvent::Error {
            operation,
            message: message.to_string(),
        },
    );
}

/// GitHub webhook ingress (AB#1065): the body of the widened webhook ingestor the root installs.
///
/// **Persist-before-ACK split (F1).** This AWAITS the durable [`store::insert_dedup`] and returns
/// `AppResult<()>` reflecting THAT — the handler gates the HTTP ACK on it (`Err` → 500 → platform
/// retry, so no delivery is lost between ACK and the SQLite commit). On a NEW delivery it emits
/// `Received` and wakes the single worker. A DUPLICATE (`Ok(None)`) returns `Ok` without another
/// wake; a persist error returns `Err` so the platform retries the whole delivery durably.
///
/// Takes the NEUTRAL [`Event`] (the root already normalized the `WebhookEvent` via
/// `pr::webhook::event_from_webhook`) plus the verbatim `raw` body and the parsed-`WebhookEvent`
/// JSON (`webhook_event_json`) — NO `pr` type. The JSON is persisted for replay and passed to the
/// `github_refeed` closure (which deserializes it back into a `WebhookEvent` in `lib.rs`).
///
/// The worker owns all refeed and rule-processing side effects; ingress never executes them.
pub async fn ingest_github(
    app: &tauri::AppHandle,
    db: &Database,
    event: EventEnvelope,
    raw: String,
    webhook_event_json: String,
    candidate: Option<Candidate>,
) -> AppResult<()> {
    let project_id = event.project_id().to_string();

    // DURABLE PERSIST (awaited) — this is what the ACK gates on. An Err propagates → handler 500 →
    // platform retry. (No additive "persist failed → still refeed" fallback anymore: a 5xx retry is
    // the correct durable semantics, and refeeding without a persisted row would re-dispatch
    // unbounded on a persistently-broken store.)
    let inserted = store::insert_dedup(
        db,
        &event,
        &raw,
        Some(&webhook_event_json),
        candidate.as_ref(),
    )?;

    let Some(id) = inserted else {
        // Duplicate delivery (same dedupe_key): already persisted + processed on the first
        // delivery. Durable persist is satisfied (the row exists), so ACK Ok with NO re-feed.
        return Ok(());
    };

    emit_for_id(app, db, &project_id, id);
    if store::retention_pressure(db)? {
        announce_worker_error(
            app,
            "retention",
            "inbox 活任务超过保留目标；已保留全部 live rows",
        );
    }
    app.state::<crate::state::AppState>().inbox.wake();
    Ok(())
}

/// Azure refresh ingress (AB#1065): the body of the widened webhook refresher the root installs.
///
/// **Persist-before-ACK split (F1).** AWAITS the durable audit-entry [`store::insert_dedup`] and
/// returns `AppResult<()>` reflecting THAT (the handler gates the ACK on it; `Err` → 500 → retry).
/// Then emits `Received` and wakes the single worker. Returns `Ok` once the audit row is durable.
///
/// **Duplicate handling.** Only a NEW audit row (`insert_dedup` → `Some(id)`) drives the status
/// lifecycle. A DUPLICATE (`None`) is durable-Ok (the row already exists) but must NOT touch any
/// existing row's status — re-flipping a prior `Failed` row to `Processed` would corrupt the audit
/// history. A duplicate does not re-run refresh: an explicit replay must first make the guarded
/// `Failed → Received` transition and hand the row back to the worker.
pub async fn ingest_azure_refresh(
    app: &tauri::AppHandle,
    db: &Database,
    raw: String,
    project_id: String,
    repo: String,
) -> AppResult<()> {
    let now = store::now_epoch();
    let event = normalize_azure(&raw, &project_id, &repo, now);

    // DURABLE PERSIST (awaited) — gates the ACK. Azure audit rows carry no parsed WebhookEvent
    // (replay re-invokes the refresher, not a re-feed), so `webhook_event_json` is None. `Some(id)`
    // = new audit row; `None` = duplicate (still durable-Ok). An Err propagates → handler 500.
    let new_id = store::insert_dedup(db, &event, &raw, None, None)?;

    if let Some(id) = new_id {
        emit_for_id(app, db, &project_id, id);
        app.state::<crate::state::AppState>().inbox.wake();
    }
    Ok(())
}

/// Process one `Received` inbox entry (AB#1065/#1379): GitHub entries re-feed the stored parsed
/// `WebhookEvent` JSON through the injected [`GithubRefeed`], Azure audit entries re-invoke the
/// [`AzureRefresh`], and candidate-backed entries re-run the rule engine through
/// [`RuleProcessor`]. The single inbox worker is the only caller; non-`Received` rows are ignored.
/// An unknown id is an error. Failures transition the row to `Failed`; successful rule processing
/// commits `Processed` atomically with its traces and produced actions.
///
/// **Replay approach: persisted `WebhookEvent` JSON (not re-parse).** `pr::webhook::parse_delivery`
/// is private AND route-snapshot-dependent (it needs the live `routes`, unavailable at replay
/// time), so the inbox persists the parsed `WebhookEvent` JSON at ingress
/// (`inbox_event.webhook_event_json`) and replays from THAT — handing the OPAQUE JSON to the
/// `github_refeed` closure, which deserializes it back into a `WebhookEvent` in `lib.rs` (the only
/// place that names that type). Candidate-backed rows instead use the stored backend-only
/// `candidate_json`, so a replay can re-run rule processing without a webhook JSON.
/// The inbox itself never sees a `pr` or `outbox` type.
pub(crate) async fn process_received(
    app: &tauri::AppHandle,
    db: &Database,
    github_refeed: &GithubRefeed,
    refresher: &AzureRefresh,
    rule_processor: &RuleProcessor,
    id: i64,
) -> AppResult<()> {
    let Some(plan) = load_received_plan(db, id)? else {
        return Ok(());
    };
    let project_id = plan.project_id.clone();

    // Execute the plan's AppHandle-bound action via the injected closures. F2: the closure RESULT
    // is authoritative — a refeed/refresh FAILURE marks the row `Failed` (not falsely `Processed`)
    // and is returned so `inbox_replay` surfaces it to the frontend.
    let result: AppResult<()> = async {
        match plan.action {
            ProcessingAction::ProcessReviewRequest => {
                let replayable = store::get_replayable(db, id)?
                    .ok_or_else(|| AppError::new(format!("inbox 条目不存在（id={id}）")))?;
                rule_processor(app.clone(), id, replayable.event, None).await
            }
            ProcessingAction::RefeedGithub(webhook_event_json) => {
                let gated_candidate = github_refeed(app.clone(), webhook_event_json).await?;
                let replayable = store::get_replayable(db, id)?
                    .ok_or_else(|| AppError::new(format!("inbox 条目不存在（id={id}）")))?;
                rule_processor(app.clone(), id, replayable.event, gated_candidate).await
            }
            ProcessingAction::RefreshAzure => {
                refresher(project_id.clone()).await?;
                let replayable = store::get_replayable(db, id)?
                    .ok_or_else(|| AppError::new(format!("inbox 条目不存在（id={id}）")))?;
                rule_processor(app.clone(), id, replayable.event, None).await
            }
            ProcessingAction::ProcessRulesWithCandidate(candidate) => {
                let replayable = store::get_replayable(db, id)?
                    .ok_or_else(|| AppError::new(format!("inbox 条目不存在（id={id}）")))?;
                rule_processor(app.clone(), id, replayable.event, Some(candidate)).await
            }
        }
    }
    .await;

    // F2: record the terminal status from the closure Result (Ok → Processed, Err → Failed), then
    // surface the Result so the worker can report the failed attempt.
    record_processing_result(db, id, &result)?;
    emit_for_id(app, db, &project_id, id);
    result
}

fn record_processing_result(db: &Database, id: i64, result: &AppResult<()>) -> AppResult<()> {
    if let Err(error) = result {
        store::mark_failed(db, id, &error.message)?;
    }
    Ok(())
}

/// Hydrate and plan one worker-owned `Received` row. Corrupt rows are terminalized here before the
/// error is returned, so the worker cannot select the same unreadable row in a tight loop.
fn load_received_plan(db: &Database, id: i64) -> AppResult<Option<ProcessingPlan>> {
    let replayable = match store::get_replayable(db, id) {
        Ok(Some(replayable)) => replayable,
        Ok(None) => return Err(AppError::new(format!("inbox 条目不存在（id={id}）"))),
        Err(error) => {
            if store::get_raw(db, id)?.is_some() {
                store::mark_failed(db, id, &error.message)?;
            }
            return Err(error);
        }
    };
    if replayable.status != crate::model::InboxStatus::Received {
        return Ok(None);
    }
    match load_processing_plan(db, id) {
        Ok(plan) => Ok(Some(plan)),
        Err(error) => {
            store::mark_failed(db, id, &error.message)?;
            Err(error)
        }
    }
}

/// Replay is a state transition only. The single worker remains the sole executor.
pub fn requeue_failed<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    db: &Database,
    id: i64,
) -> AppResult<()> {
    if !store::requeue_failed(db, id)? {
        return Err(AppError::new(format!(
            "inbox 条目仅允许 Failed → Received 重放（id={id}）"
        )));
    }
    if let Some(entry) = store::get_entry(db, id)? {
        let project_id = entry.event.project_id().to_string();
        emit_updated(app, &project_id, entry);
    }
    app.state::<crate::state::AppState>().inbox.wake();
    Ok(())
}

/// What processing a stored entry will do (AB#1065) — the AppHandle-free decision so the
/// source-branch logic (incl. the error cases) is unit-tested without a Tauri runtime.
#[derive(Debug)]
enum ProcessingAction {
    /// A typed external request: bypass source replay and enter the rule/outbox funnel directly.
    ProcessReviewRequest,
    /// A candidate-backed entry: re-run the stored event and candidate through rule processing.
    ProcessRulesWithCandidate(Candidate),
    /// A GitHub entry: re-feed the stored parsed-`WebhookEvent` JSON through the `github_refeed`
    /// closure. Carries the OPAQUE JSON String (NOT a `pr::webhook::WebhookEvent`) so the inbox
    /// stays decoupled — the closure deserializes it in `lib.rs`.
    RefeedGithub(String),
    /// An Azure audit entry: re-invoke the refresher (re-run `az` discovery).
    RefreshAzure,
}

/// One processing plan: the routing `project_id` + action. The action is AppHandle-free so the
/// whole decision (load → source branch → error cases) is testable before the worker executes it.
#[derive(Debug)]
struct ProcessingPlan {
    project_id: String,
    action: ProcessingAction,
}

/// Load entry `id` and decide its [`ProcessingPlan`] (AB#1065), db-only. An UNKNOWN id is an error
/// (the `inbox_replay` "err if unknown" contract); a corrupt / non-replayable entry is also an
/// error (caller marks it Failed). Branches on the sealed [`SourceKind`] EXHAUSTIVELY (Hard
/// carrier): a new source variant is a compile error here, forcing an explicit replay decision.
fn load_processing_plan(db: &Database, id: i64) -> AppResult<ProcessingPlan> {
    let store::ProcessableInboxRow {
        event,
        source,
        status: _status,
        webhook_event_json,
        candidate,
    } = store::get_replayable(db, id)?
        .ok_or_else(|| AppError::new(format!("inbox 条目不存在（id={id}）")))?;
    let project_id = event.project_id().to_string();
    let action = if event.as_review_request().is_some() {
        ProcessingAction::ProcessReviewRequest
    } else {
        match (source, webhook_event_json, candidate) {
            // A real GitHub webhook row can also carry candidate_json as producer metadata, but replay
            // must re-feed the full WebhookEvent so list update + gates + ledger/cooldown semantics run.
            (SourceKind::Github, Some(json), _) => ProcessingAction::RefeedGithub(json),
            // Synthetic candidate-backed rows have no webhook payload; candidate_json is their replay fact.
            (_, _, Some(candidate)) => ProcessingAction::ProcessRulesWithCandidate(candidate),
            (SourceKind::Github, None, None) => {
                return Err(AppError::new(format!(
                    "inbox 条目 {id} 缺少可重放的 WebhookEvent（GitHub 投递未保存解析结果）"
                )));
            }
            (SourceKind::Azure, _, None) => ProcessingAction::RefreshAzure,
            // Bitbucket has no inbound webhook (poll/API discovery only — see `model::SourceKind`),
            // so an inbox entry can never carry it unless it is a candidate-backed synthetic row (handled
            // above); reject a replay rather than silently no-op.
            (SourceKind::Bitbucket, _, None) => {
                return Err(AppError::new(format!(
                    "inbox 条目 {id} 来源 Bitbucket 不支持重放（无入站 webhook）"
                )));
            }
        }
    };
    Ok(ProcessingPlan { project_id, action })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a neutral `model::Event` for a GitHub delivery (AB#1065) WITHOUT any `pr` type — the
    /// inbox tests construct the envelope directly (the `WebhookEvent`→`Event` normalization is
    /// tested in `pr::webhook`, where `WebhookEvent` lives).
    fn github_event(dedupe_key: &str, project_id: &str, number: u64) -> EventEnvelope {
        EventEnvelope::observation(
            InboxDedupeKey::new(dedupe_key).unwrap(),
            SourceKind::Github,
            project_id,
            "owner/repo",
            EventType::PullRequest,
            EventSubject {
                number: Some(number),
                title: "Add feature".to_string(),
                body: String::new(),
                labels: vec!["pr-review".to_string()],
                url: "https://example.com/pr/7".to_string(),
            },
            1_700_000_000,
        )
        .unwrap()
    }

    fn candidate(number: u64, skill_key: impl AsRef<str>) -> Candidate {
        let skill_key = skill_key.as_ref();
        Candidate {
            number,
            skill_key: crate::model::SkillInvocation::migrate_legacy_skill_key(skill_key),
            head_sha: format!("head-{number}-{skill_key}"),
            head_ref: "main".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
        }
    }

    fn review_request_event(source: SourceKind) -> EventEnvelope {
        EventEnvelope::review_request(
            InboxDedupeKey::new("external:0123456789abcdef0123456789abcdef").unwrap(),
            source,
            "p1",
            "owner/repo",
            7,
            crate::model::DEFAULT_SKILL_NAME,
            "",
            crate::model::DEFAULT_SKILL_PATH,
            crate::model::DEFAULT_COMMAND_TEMPLATE,
            crate::model::ExternalRequestId::parse("0123456789abcdef0123456789abcdef").unwrap(),
            crate::model::ExternalTriggerOrigin::Http,
            false,
            1,
        )
        .unwrap()
    }

    // `normalize_azure` (AB#1065): a Generic audit event keyed on the body hash, no number — built
    // from the raw body + route identity only (no `pr` type).
    #[test]
    fn normalize_azure_is_generic_audit_keyed_on_body_hash() {
        let event = normalize_azure("azure-body", "p2", "myrepo", 555);
        assert!(event.dedupe_key().starts_with("azure:sha256:"));
        assert_eq!(event.source(), SourceKind::Azure);
        assert_eq!(
            event.as_observation().unwrap().event_type,
            EventType::Generic
        );
        assert_eq!(event.project_id(), "p2");
        assert_eq!(event.repo(), "myrepo");
        assert_eq!(event.as_observation().unwrap().subject.number, None);
        assert_eq!(event.received_at_epoch(), 555);
        // Same body → same key (dedups); different body → different key.
        assert_eq!(
            event.dedupe_key(),
            normalize_azure("azure-body", "p2", "myrepo", 1).dedupe_key()
        );
        assert_ne!(
            event.dedupe_key(),
            normalize_azure("other-body", "p2", "myrepo", 1).dedupe_key()
        );
    }

    // Duplicate dedupe key does not double-trigger (AB#1065): driving the SAME delivery through the
    // store twice inserts ONE row, and `should_process` is true only on the FIRST insert — so the
    // re-feed (the IO `ingest_github` gates on `should_process`) runs once.
    #[test]
    fn duplicate_dedupe_key_does_not_double_trigger() {
        let db = Database::open_in_memory().expect("open db");
        let event = github_event("github:dup-guid", "p1", 7);

        let first = store::insert_dedup(&db, &event, "raw", None, None).expect("first");
        let second = store::insert_dedup(&db, &event, "raw", None, None).expect("second");

        assert!(should_process(first), "first delivery triggers processing");
        assert!(
            !should_process(second),
            "duplicate delivery must NOT re-trigger processing"
        );
    }

    // `should_process` decision table (AB#1065): Some(id) → process; None → skip.
    #[test]
    fn should_process_is_some_only() {
        assert!(should_process(Some(1)));
        assert!(!should_process(None));
    }

    // Worker planning uses the persisted normalized payload. Requeue itself is tested at the store
    // boundary because it must never perform this side effect inline.
    #[test]
    fn received_github_entry_plans_refeed() {
        let db = Database::open_in_memory().expect("open db");
        let event = github_event("github:g1", "p1", 7);
        let webhook_json = "{\"projectId\":\"p1\",\"number\":7}".to_string();
        let id = store::insert_dedup(&db, &event, "raw", Some(&webhook_json), None)
            .expect("insert")
            .expect("new id");
        let plan = load_processing_plan(&db, id).expect("plan");
        assert_eq!(plan.project_id, "p1");
        match plan.action {
            ProcessingAction::RefeedGithub(json) => assert_eq!(json, webhook_json),
            other => panic!("expected RefeedGithub, got {other:?}"),
        }
    }

    // Replay of an Azure audit entry plans a refresh (no stored WebhookEvent needed).
    #[test]
    fn replay_azure_plans_a_refresh() {
        let db = Database::open_in_memory().expect("open db");
        let event = normalize_azure("azure-body", "p2", "myrepo", 1);
        let id = store::insert_dedup(&db, &event, "azure-body", None, None)
            .expect("insert")
            .expect("new id");
        let plan = load_processing_plan(&db, id).expect("plan");
        assert_eq!(plan.project_id, "p2");
        assert!(matches!(plan.action, ProcessingAction::RefreshAzure));
    }

    #[test]
    fn replay_candidate_json_reruns_rule_processing() {
        let db = Database::open_in_memory().expect("open db");
        let event = github_event("auto-dispatch:p1:7@head-7-review:review", "p1", 7);
        let cand = candidate(7, "review");
        let id = store::insert_dedup(&db, &event, "payload", None, Some(&cand))
            .expect("insert")
            .expect("new id");

        let plan = load_processing_plan(&db, id).expect("plan");
        assert_eq!(plan.project_id, "p1");
        match plan.action {
            ProcessingAction::ProcessRulesWithCandidate(actual) => assert_eq!(actual, cand),
            other => panic!("expected ProcessRulesWithCandidate, got {other:?}"),
        }
    }

    #[test]
    fn review_request_plans_rule_processing_independent_of_source_metadata() {
        for source in [SourceKind::Github, SourceKind::Azure, SourceKind::Bitbucket] {
            let db = Database::open_in_memory().expect("open db");
            let event = review_request_event(source);
            let id = store::insert_dedup(&db, &event, "external", None, None)
                .expect("insert")
                .expect("new id");

            let plan = load_processing_plan(&db, id)
                .unwrap_or_else(|error| panic!("{source:?} request must plan: {}", error.message));
            assert_eq!(plan.project_id, "p1");
        }
    }

    #[test]
    fn replay_github_with_webhook_json_prefers_refeed_over_candidate_metadata() {
        let db = Database::open_in_memory().expect("open db");
        let event = github_event("github:g1", "p1", 7);
        let webhook_json = "{\"projectId\":\"p1\",\"number\":7}".to_string();
        let cand = candidate(7, "review");
        let id = store::insert_dedup(&db, &event, "raw", Some(&webhook_json), Some(&cand))
            .expect("insert")
            .expect("new id");

        let plan = load_processing_plan(&db, id).expect("plan");
        assert_eq!(plan.project_id, "p1");
        match plan.action {
            ProcessingAction::RefeedGithub(json) => assert_eq!(json, webhook_json),
            other => panic!("expected RefeedGithub, got {other:?}"),
        }
    }

    // Replay unknown id errors (AB#1065): an id with no row is an error, not a silent no-op —
    // the `inbox_replay` "err if unknown" contract. The db-only `load_processing_plan` carries it.
    #[test]
    fn replay_unknown_id_errors() {
        let db = Database::open_in_memory().expect("open db");
        let err = load_processing_plan(&db, 99_999).expect_err("unknown id must error");
        assert!(err.message.contains("不存在"), "{}", err.message);
    }

    // A GitHub entry stored WITHOUT a parsed-WebhookEvent JSON (e.g. a persist that lost it) is not
    // replayable — an explicit error, never a silent no-op.
    #[test]
    fn replay_github_without_webhook_event_errors() {
        let db = Database::open_in_memory().expect("open db");
        let event = github_event("github:g1", "p1", 7);
        let id = store::insert_dedup(&db, &event, "raw", None, None)
            .expect("insert")
            .expect("new id");
        let err = load_processing_plan(&db, id).expect_err("missing webhook event must error");
        assert!(err.message.contains("缺少可重放"), "{}", err.message);
    }

    // `load_processing_plan` rejects a Bitbucket entry (AB#1065): Bitbucket has no inbound webhook, so
    // an inbox entry can't legitimately carry it — replay must error, not no-op.
    #[test]
    fn load_processing_plan_bitbucket_errors() {
        let db = Database::open_in_memory().expect("open db");
        let bb_event = EventEnvelope::observation(
            InboxDedupeKey::new("bb:1").unwrap(),
            SourceKind::Bitbucket,
            "p1",
            "owner/repo",
            EventType::Generic,
            EventSubject {
                number: None,
                title: String::new(),
                body: String::new(),
                labels: Vec::new(),
                url: String::new(),
            },
            1,
        )
        .unwrap();
        let id = store::insert_dedup(&db, &bb_event, "raw", None, None)
            .expect("insert")
            .expect("new id");
        let err = load_processing_plan(&db, id).expect_err("bitbucket replay must error");
        assert!(
            err.message.contains("Bitbucket") && err.message.contains("不支持重放"),
            "{}",
            err.message
        );
    }

    // Duplicate Azure delivery does NOT change an existing row's status (AB#1065): a re-delivered
    // Azure body (`insert_dedup` → None) must leave the prior row alone. The IO `ingest_azure_refresh`
    // only marks-processed on a NEW id, so a duplicate leaves a `Failed` row Failed (the old bug
    // re-flipped it to Processed).
    #[test]
    fn duplicate_azure_delivery_does_not_change_existing_status() {
        let db = Database::open_in_memory().expect("open db");
        let event = normalize_azure("azure-body", "p2", "myrepo", 1);
        let id = store::insert_dedup(&db, &event, "azure-body", None, None)
            .expect("first insert")
            .expect("new id");
        store::mark_failed(&db, id, "earlier failure").expect("mark failed");

        let dup = store::insert_dedup(&db, &event, "azure-body", None, None).expect("dup insert");
        assert!(dup.is_none(), "duplicate Azure delivery is a no-op insert");

        let entry = store::get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(
            entry.status,
            crate::model::InboxStatus::Failed,
            "duplicate must NOT re-flip a Failed row to Processed"
        );
        assert_eq!(entry.error.as_deref(), Some("earlier failure"));
    }

    #[test]
    fn processing_failure_transitions_received_to_failed() {
        let db = Database::open_in_memory().expect("open db");
        let event = github_event("github:g1", "p1", 7);
        let id = store::insert_dedup(&db, &event, "raw", Some("not-a-webhook-event"), None)
            .expect("insert")
            .expect("new id");
        let result = Err(AppError::new("WebhookEvent 反序列化失败"));
        record_processing_result(&db, id, &result).expect("record failure");
        let entry = store::get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(
            entry.status,
            crate::model::InboxStatus::Failed,
            "worker errors terminalize instead of tight-looping Received"
        );
        assert!(entry.error.is_some());
    }

    #[test]
    fn corrupt_received_row_transitions_to_failed_instead_of_looping() {
        let db = Database::open_in_memory().expect("open db");
        let event = github_event("github:corrupt", "p1", 7);
        let id = store::insert_dedup(&db, &event, "raw", Some("{}"), None)
            .expect("insert")
            .expect("new id");
        db.with_conn(|conn| {
            conn.pragma_update(None, "ignore_check_constraints", true)?;
            conn.execute(
                "UPDATE inbox_event SET source = 'corrupt-source' WHERE id = ?1",
                [id],
            )?;
            conn.pragma_update(None, "ignore_check_constraints", false)
        })
        .expect("corrupt source");
        assert!(load_received_plan(&db, id).is_err());

        let entry = store::get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(entry.status, crate::model::InboxStatus::Failed);
        assert!(entry.error.as_deref().is_some_and(|e| e.contains("source")));
    }
}
