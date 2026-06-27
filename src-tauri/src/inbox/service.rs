//! Inbox ingress + replay logic (AB#1065/#1379): the composition-facing body the root installs as
//! the webhook ingestor / refresher / rule processor.
//!
//! **Decoupled from `pr`.** This module names NO `pr`-internal type. It works only on the neutral
//! [`crate::model::Event`] envelope and OPAQUE composition-root-injected closures
//! ([`crate::inbox::GithubRefeed`] / [`crate::inbox::AzureRefresh`] /
//! [`crate::inbox::RuleProcessor`]). The composition root normalizes a
//! `pr::webhook::WebhookEvent` into a `model::Event` (via `pr::webhook::event_from_webhook`) BEFORE
//! calling [`ingest_github`], and the closures it injects are the only path back into other slices
//! (re-feed via `pr::commands::ingest_webhook`, re-discovery via `az`, rule processing via outbox).
//!
//! **Funnel (upstream Hard, downstream durable).** The inbox's UPSTREAM gate is the
//! `inbox_event.UNIQUE(dedupe_key)` constraint (the **Hard** ingress-idempotency carrier — a
//! re-delivered webhook is unexpressible as a second row, see [`crate::inbox::store::insert_dedup`]).
//! The DOWNSTREAM review/check dedup gate is the outbox pending `dedupe_key` plus the review
//! executor's durable claim. The inbox dedups DELIVERIES / stored producer inputs; outbox + executor
//! dedup REVIEW DISPATCHES.

use sha2::{Digest, Sha256};
use tauri::{async_runtime::spawn, Emitter, Manager};

use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::events::{InboxEvent, INBOX_UPDATED_EVENT};
use crate::inbox::store;
use crate::inbox::{AzureRefresh, GithubRefeed, RuleProcessor};
use crate::model::{Candidate, Event, EventType, InboxEntry, SourceKind};

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
pub fn normalize_azure(raw: &str, project_id: &str, repo: &str, now: u64) -> Event {
    Event {
        dedupe_key: format!("azure:sha256:{}", sha256_hex(raw.as_bytes())),
        source: SourceKind::Azure,
        event_type: EventType::Generic,
        project_id: project_id.to_string(),
        repo: repo.to_string(),
        number: None,
        title: String::new(),
        body: String::new(),
        labels: Vec::new(),
        url: String::new(),
        received_at_epoch: now,
    }
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
            entry,
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

/// Record a process/refeed [`AppResult`] as the row's TERMINAL status (AB#1065 F2): `Ok` →
/// `Processed`, `Err` → `Failed(message)`. The single decision shared by `process_github` /
/// `process_azure` / `replay`, so a refeed failure can never be recorded as a false `Processed`.
/// db-only (AppHandle-free) so the Ok→Processed / Err→Failed mapping is unit-tested directly.
pub(crate) fn record_terminal(db: &Database, id: i64, result: &AppResult<()>) -> AppResult<()> {
    match result {
        Ok(()) => store::mark_processed(db, id),
        Err(e) => store::mark_failed(db, id, &e.message),
    }
}

pub struct GithubIngestHooks<'a> {
    pub github_refeed: &'a GithubRefeed,
    pub rule_processor: &'a RuleProcessor,
}

/// GitHub webhook ingress (AB#1065): the body of the widened webhook ingestor the root installs.
///
/// **Persist-before-ACK split (F1).** This AWAITS the durable [`store::insert_dedup`] and returns
/// `AppResult<()>` reflecting THAT — the handler gates the HTTP ACK on it (`Err` → 500 → platform
/// retry, so no delivery is lost between ACK and the SQLite commit). On a NEW delivery it then
/// SPAWNS the post-ACK PROCESS (emit Received → refeed → mark Processed/Failed → emit) and returns
/// `Ok` immediately. A DUPLICATE (`Ok(None)`) returns `Ok` with no process; a persist error returns
/// `Err` (no spawn, no refeed — the platform retries the whole delivery durably).
///
/// Takes the NEUTRAL [`Event`] (the root already normalized the `WebhookEvent` via
/// `pr::webhook::event_from_webhook`) plus the verbatim `raw` body and the parsed-`WebhookEvent`
/// JSON (`webhook_event_json`) — NO `pr` type. The JSON is persisted for replay and passed to the
/// `github_refeed` closure (which deserializes it back into a `WebhookEvent` in `lib.rs`).
///
/// The spawned process re-acquires the [`Database`] via `app.state::<Database>()` (the `'static`
/// task can't borrow the caller's `&Database`); `app` is the managed handle, so the state is always
/// present once `setup` ran.
pub async fn ingest_github(
    app: &tauri::AppHandle,
    db: &Database,
    hooks: GithubIngestHooks<'_>,
    event: Event,
    raw: String,
    webhook_event_json: String,
    candidate: Option<Candidate>,
) -> AppResult<()> {
    let project_id = event.project_id.clone();

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

    // NEW delivery: the durable write is done, so ACK can fire. Run the PROCESS post-ACK in a
    // detached task — emit Received, refeed through the injected dispatch path, then mark the row
    // Processed (refeed Ok) or Failed (refeed Err, F2 — no false Processed), and re-emit.
    let app = app.clone();
    let github_refeed = hooks.github_refeed.clone();
    let rule_processor = hooks.rule_processor.clone();
    drop(spawn(async move {
        process_github(
            app,
            github_refeed,
            rule_processor,
            project_id,
            webhook_event_json,
            id,
        )
        .await;
    }));
    Ok(())
}

/// The post-ACK PROCESS for a newly-persisted GitHub delivery (AB#1065 F1/F2): announce Received,
/// re-feed via the injected closure, then record the terminal row status from the refeed Result
/// (Ok → Processed, Err → Failed). Runs in a spawned `'static` task, so it re-acquires the
/// [`Database`] from the managed `app` state rather than borrowing the caller's handle.
async fn process_github(
    app: tauri::AppHandle,
    github_refeed: GithubRefeed,
    rule_processor: RuleProcessor,
    project_id: String,
    webhook_event_json: String,
    id: i64,
) {
    let db = app.state::<Database>();
    let db = db.inner();
    emit_for_id(&app, db, &project_id, id);
    // The refeed Result drives the inbox ROW STATUS (NOT the HTTP ACK, which F1 tied to persist):
    // Ok → Processed, Err → Failed (F2 — no false Processed on a failed refeed).
    let result = match github_refeed(app.clone(), webhook_event_json).await {
        Ok(gated_candidate) => match store::get_replayable(db, id) {
            Ok(Some(replayable)) => {
                rule_processor(app.clone(), id, replayable.event, gated_candidate).await
            }
            Ok(None) => Ok(()),
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    };
    if let Err(e) = record_terminal(db, id, &result) {
        eprintln!("inbox: 记录 GitHub 投递终态失败（id={id}）：{}", e.message);
    }
    emit_for_id(&app, db, &project_id, id);
}

/// Azure refresh ingress (AB#1065): the body of the widened webhook refresher the root installs.
///
/// **Persist-before-ACK split (F1).** AWAITS the durable audit-entry [`store::insert_dedup`] and
/// returns `AppResult<()>` reflecting THAT (the handler gates the ACK on it; `Err` → 500 → retry).
/// Then SPAWNS the post-ACK PROCESS (refresh + status). Returns `Ok` once the audit row is durable.
///
/// **Duplicate handling.** Only a NEW audit row (`insert_dedup` → `Some(id)`) drives the status
/// lifecycle. A DUPLICATE (`None`) is durable-Ok (the row already exists) but must NOT touch any
/// existing row's status — re-flipping a prior `Failed` row to `Processed` would corrupt the audit
/// history. The refresh STILL runs on a duplicate (a genuine re-delivery re-reads the labels), so
/// it is invoked in the spawned process either way; only a NEW row records the Processed/Failed
/// transition from the refresh Result (F2).
pub async fn ingest_azure_refresh(
    app: &tauri::AppHandle,
    db: &Database,
    refresher: &AzureRefresh,
    rule_processor: &RuleProcessor,
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

    // Run the refresh + (for a new row) the status transition post-ACK in a detached task.
    let app = app.clone();
    let refresher = refresher.clone();
    let rule_processor = rule_processor.clone();
    drop(spawn(async move {
        process_azure(app, refresher, rule_processor, project_id, new_id).await;
    }));
    Ok(())
}

/// The post-ACK PROCESS for an Azure refresh delivery (AB#1065 F1/F2): announce a NEW row Received,
/// invoke the `az` re-discovery (for new AND duplicate deliveries), then record the terminal row
/// status from the refresh Result (Ok → Processed, Err → Failed) for a NEW row only. Runs in a
/// spawned `'static` task; re-acquires the [`Database`] from the managed `app` state.
async fn process_azure(
    app: tauri::AppHandle,
    refresher: AzureRefresh,
    rule_processor: RuleProcessor,
    project_id: String,
    new_id: Option<i64>,
) {
    let db = app.state::<Database>();
    let db = db.inner();

    // Announce a NEW row (Received) before the refresh.
    if let Some(id) = new_id {
        emit_for_id(&app, db, &project_id, id);
    }

    // Always invoke the refresh — for a new AND a duplicate delivery. `discover_once` coalesces
    // concurrent refreshes per project. Its Result drives a NEW row's terminal status (F2).
    let mut result = refresher(project_id.clone()).await;

    if let Some(id) = new_id {
        if result.is_ok() {
            match store::get_replayable(db, id) {
                Ok(Some(replayable)) => {
                    result = rule_processor(app.clone(), id, replayable.event, None).await;
                }
                Ok(None) => {}
                Err(e) => result = Err(e),
            }
        }
        // F2: the refresh Result drives a NEW row's terminal status (Ok → Processed, Err → Failed).
        if let Err(e) = record_terminal(db, id, &result) {
            eprintln!("inbox: 记录 Azure 投递终态失败（id={id}）：{}", e.message);
        }
        emit_for_id(&app, db, &project_id, id);
    }
}

/// Re-process a stored inbox entry by id (AB#1065/#1379): GitHub entries re-feed the stored parsed
/// `WebhookEvent` JSON through the injected [`GithubRefeed`], Azure audit entries re-invoke the
/// [`AzureRefresh`], and candidate-backed entries re-run the rule engine through
/// [`RuleProcessor`]. Works on ANY entry regardless of its current status.
/// An unknown id is an error (the command surfaces it). The entry is marked `Processed` / `Failed`
/// and re-announced.
///
/// **Replay approach: persisted `WebhookEvent` JSON (not re-parse).** `pr::webhook::parse_delivery`
/// is private AND route-snapshot-dependent (it needs the live `routes`, unavailable at replay
/// time), so the inbox persists the parsed `WebhookEvent` JSON at ingress
/// (`inbox_event.webhook_event_json`) and replays from THAT — handing the OPAQUE JSON to the
/// `github_refeed` closure, which deserializes it back into a `WebhookEvent` in `lib.rs` (the only
/// place that names that type). Candidate-backed rows instead use the stored backend-only
/// `candidate_json`, so a replay can re-run rule processing without a webhook JSON.
/// The inbox itself never sees a `pr` or `outbox` type.
pub async fn replay(
    app: &tauri::AppHandle,
    db: &Database,
    github_refeed: &GithubRefeed,
    refresher: &AzureRefresh,
    rule_processor: &RuleProcessor,
    id: i64,
) -> AppResult<()> {
    // Load + decide the plan (db-only, AppHandle-free — `load_replay_plan` errors on an unknown
    // id, the `replay_unknown_id_errors` contract). A plan-build error (corrupt / non-replayable
    // entry) marks the entry Failed and surfaces, never silently no-ops.
    let plan = match load_replay_plan(db, id) {
        Ok(p) => p,
        Err(e) => {
            // Only mark-failed when the row EXISTS (an unknown id has nothing to mark — it just
            // errors). `get_raw` presence is the cheap existence probe.
            if store::get_raw(db, id)?.is_some() {
                store::mark_failed(db, id, &e.message)?;
                // The project id is unknown on a corrupt-row error path; skip the targeted emit.
            }
            return Err(e);
        }
    };
    let project_id = plan.project_id.clone();

    // Execute the plan's AppHandle-bound action via the injected closures. F2: the closure RESULT
    // is authoritative — a refeed/refresh FAILURE marks the row `Failed` (not falsely `Processed`)
    // and is returned so `inbox_replay` surfaces it to the frontend.
    let result: AppResult<()> = match plan.action {
        ReplayAction::RefeedGithub(webhook_event_json) => {
            let gated_candidate = github_refeed(app.clone(), webhook_event_json).await?;
            let replayable = store::get_replayable(db, id)?
                .ok_or_else(|| AppError::new(format!("inbox 条目不存在（id={id}）")))?;
            rule_processor(app.clone(), id, replayable.event, gated_candidate).await
        }
        ReplayAction::RefreshAzure => {
            refresher(project_id.clone()).await?;
            let replayable = store::get_replayable(db, id)?
                .ok_or_else(|| AppError::new(format!("inbox 条目不存在（id={id}）")))?;
            rule_processor(app.clone(), id, replayable.event, None).await
        }
        ReplayAction::ProcessRulesWithCandidate(candidate) => {
            let replayable = store::get_replayable(db, id)?
                .ok_or_else(|| AppError::new(format!("inbox 条目不存在（id={id}）")))?;
            rule_processor(app.clone(), id, replayable.event, Some(candidate)).await
        }
    };

    // F2: record the terminal status from the closure Result (Ok → Processed, Err → Failed), then
    // surface the Result so `inbox_replay` reports a failed replay to the frontend.
    record_terminal(db, id, &result)?;
    emit_for_id(app, db, &project_id, id);
    result
}

/// What a replay of a stored entry will DO (AB#1065) — the AppHandle-free replay decision so the
/// source-branch logic (incl. the error cases) is unit-tested without a Tauri runtime.
#[derive(Debug)]
enum ReplayAction {
    /// A candidate-backed entry: re-run the stored event and candidate through rule processing.
    ProcessRulesWithCandidate(Candidate),
    /// A GitHub entry: re-feed the stored parsed-`WebhookEvent` JSON through the `github_refeed`
    /// closure. Carries the OPAQUE JSON String (NOT a `pr::webhook::WebhookEvent`) so the inbox
    /// stays decoupled — the closure deserializes it in `lib.rs`.
    RefeedGithub(String),
    /// An Azure audit entry: re-invoke the refresher (re-run `az` discovery).
    RefreshAzure,
}

/// One decided replay: the routing `project_id` + the action. The action is AppHandle-free so the
/// whole decision (load → source branch → error cases) is testable; `replay` only executes it.
#[derive(Debug)]
struct ReplayPlan {
    project_id: String,
    action: ReplayAction,
}

/// Load entry `id` and decide its [`ReplayPlan`] (AB#1065), db-only. An UNKNOWN id is an error
/// (the `inbox_replay` "err if unknown" contract); a corrupt / non-replayable entry is also an
/// error (caller marks it Failed). Branches on the sealed [`SourceKind`] EXHAUSTIVELY (Hard
/// carrier): a new source variant is a compile error here, forcing an explicit replay decision.
fn load_replay_plan(db: &Database, id: i64) -> AppResult<ReplayPlan> {
    let store::Replayable {
        event,
        source,
        status: _status,
        webhook_event_json,
        candidate,
    } = store::get_replayable(db, id)?
        .ok_or_else(|| AppError::new(format!("inbox 条目不存在（id={id}）")))?;
    let project_id = event.project_id;
    let action = match (source, webhook_event_json, candidate) {
        // A real GitHub webhook row can also carry candidate_json as producer metadata, but replay
        // must re-feed the full WebhookEvent so list update + gates + ledger/cooldown semantics run.
        (SourceKind::Github, Some(json), _) => ReplayAction::RefeedGithub(json),
        // Synthetic candidate-backed rows have no webhook payload; candidate_json is their replay fact.
        (_, _, Some(candidate)) => ReplayAction::ProcessRulesWithCandidate(candidate),
        (SourceKind::Github, None, None) => {
            return Err(AppError::new(format!(
                "inbox 条目 {id} 缺少可重放的 WebhookEvent（GitHub 投递未保存解析结果）"
            )));
        }
        (SourceKind::Azure, _, None) => ReplayAction::RefreshAzure,
        // Bitbucket has no inbound webhook (poll/API discovery only — see `model::SourceKind`),
        // so an inbox entry can never carry it unless it is a candidate-backed synthetic row (handled
        // above); reject a replay rather than silently no-op.
        (SourceKind::Bitbucket, _, None) => {
            return Err(AppError::new(format!(
                "inbox 条目 {id} 来源 Bitbucket 不支持重放（无入站 webhook）"
            )));
        }
    };
    Ok(ReplayPlan { project_id, action })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a neutral `model::Event` for a GitHub delivery (AB#1065) WITHOUT any `pr` type — the
    /// inbox tests construct the envelope directly (the `WebhookEvent`→`Event` normalization is
    /// tested in `pr::webhook`, where `WebhookEvent` lives).
    fn github_event(dedupe_key: &str, project_id: &str, number: u64) -> Event {
        Event {
            dedupe_key: dedupe_key.to_string(),
            source: SourceKind::Github,
            event_type: EventType::PullRequest,
            project_id: project_id.to_string(),
            repo: "owner/repo".to_string(),
            number: Some(number),
            title: "Add feature".to_string(),
            body: String::new(),
            labels: vec!["pr-review".to_string()],
            url: "https://example.com/pr/7".to_string(),
            received_at_epoch: 1_700_000_000,
        }
    }

    fn candidate(number: u64, kind: &str) -> Candidate {
        Candidate {
            number,
            kind: kind.to_string(),
            head_sha: format!("head-{number}-{kind}"),
            head_ref: "main".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
        }
    }

    // `normalize_azure` (AB#1065): a Generic audit event keyed on the body hash, no number — built
    // from the raw body + route identity only (no `pr` type).
    #[test]
    fn normalize_azure_is_generic_audit_keyed_on_body_hash() {
        let event = normalize_azure("azure-body", "p2", "myrepo", 555);
        assert!(event.dedupe_key.starts_with("azure:sha256:"));
        assert_eq!(event.source, SourceKind::Azure);
        assert_eq!(event.event_type, EventType::Generic);
        assert_eq!(event.project_id, "p2");
        assert_eq!(event.repo, "myrepo");
        assert_eq!(event.number, None);
        assert_eq!(event.received_at_epoch, 555);
        // Same body → same key (dedups); different body → different key.
        assert_eq!(
            event.dedupe_key,
            normalize_azure("azure-body", "p2", "myrepo", 1).dedupe_key
        );
        assert_ne!(
            event.dedupe_key,
            normalize_azure("other-body", "p2", "myrepo", 1).dedupe_key
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

    // Replay reruns processing (AB#1065): a GitHub entry stored WITH its parsed-`WebhookEvent` JSON
    // yields a `RefeedGithub` plan carrying THAT JSON (the opaque payload `replay` hands the
    // `github_refeed` closure), and replay's success path flips the entry to `Processed`. Drives
    // the db-only seam (`load_replay_plan` + `mark_processed`) since the re-feed needs an
    // `AppHandle`; the IO `replay` only sequences these.
    #[test]
    fn replay_reruns_processing() {
        let db = Database::open_in_memory().expect("open db");
        let event = github_event("github:g1", "p1", 7);
        let webhook_json = "{\"projectId\":\"p1\",\"number\":7}".to_string();
        let id = store::insert_dedup(&db, &event, "raw", Some(&webhook_json), None)
            .expect("insert")
            .expect("new id");
        // Pretend a prior processing failed (replay works on ANY status).
        store::mark_failed(&db, id, "earlier failure").expect("mark failed");

        // The plan is a GitHub re-feed carrying the stored JSON verbatim (no `pr` type parsed here).
        let plan = load_replay_plan(&db, id).expect("plan");
        assert_eq!(plan.project_id, "p1");
        match plan.action {
            ReplayAction::RefeedGithub(json) => assert_eq!(json, webhook_json),
            other => panic!("expected RefeedGithub, got {other:?}"),
        }

        // Replay's success path marks Processed (clearing the prior error).
        store::mark_processed(&db, id).expect("mark processed");
        let entry = store::get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(entry.status, crate::model::InboxStatus::Processed);
        assert_eq!(entry.error, None);
    }

    // Replay of an Azure audit entry plans a refresh (no stored WebhookEvent needed).
    #[test]
    fn replay_azure_plans_a_refresh() {
        let db = Database::open_in_memory().expect("open db");
        let event = normalize_azure("azure-body", "p2", "myrepo", 1);
        let id = store::insert_dedup(&db, &event, "azure-body", None, None)
            .expect("insert")
            .expect("new id");
        let plan = load_replay_plan(&db, id).expect("plan");
        assert_eq!(plan.project_id, "p2");
        assert!(matches!(plan.action, ReplayAction::RefreshAzure));
    }

    #[test]
    fn replay_candidate_json_reruns_rule_processing() {
        let db = Database::open_in_memory().expect("open db");
        let event = github_event("auto-dispatch:p1:7@head-7-review:review", "p1", 7);
        let cand = candidate(7, "review");
        let id = store::insert_dedup(&db, &event, "payload", None, Some(&cand))
            .expect("insert")
            .expect("new id");

        let plan = load_replay_plan(&db, id).expect("plan");
        assert_eq!(plan.project_id, "p1");
        match plan.action {
            ReplayAction::ProcessRulesWithCandidate(actual) => assert_eq!(actual, cand),
            other => panic!("expected ProcessRulesWithCandidate, got {other:?}"),
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

        let plan = load_replay_plan(&db, id).expect("plan");
        assert_eq!(plan.project_id, "p1");
        match plan.action {
            ReplayAction::RefeedGithub(json) => assert_eq!(json, webhook_json),
            other => panic!("expected RefeedGithub, got {other:?}"),
        }
    }

    // Replay unknown id errors (AB#1065): an id with no row is an error, not a silent no-op —
    // the `inbox_replay` "err if unknown" contract. The db-only `load_replay_plan` carries it.
    #[test]
    fn replay_unknown_id_errors() {
        let db = Database::open_in_memory().expect("open db");
        let err = load_replay_plan(&db, 99_999).expect_err("unknown id must error");
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
        let err = load_replay_plan(&db, id).expect_err("missing webhook event must error");
        assert!(err.message.contains("缺少可重放"), "{}", err.message);
    }

    // `load_replay_plan` rejects a Bitbucket entry (AB#1065): Bitbucket has no inbound webhook, so
    // an inbox entry can't legitimately carry it — replay must error, not no-op.
    #[test]
    fn load_replay_plan_bitbucket_errors() {
        let db = Database::open_in_memory().expect("open db");
        let bb_event = Event {
            dedupe_key: "bb:1".to_string(),
            source: SourceKind::Bitbucket,
            event_type: EventType::Generic,
            project_id: "p1".to_string(),
            repo: "owner/repo".to_string(),
            number: None,
            title: String::new(),
            body: String::new(),
            labels: Vec::new(),
            url: String::new(),
            received_at_epoch: 1,
        };
        let id = store::insert_dedup(&db, &bb_event, "raw", None, None)
            .expect("insert")
            .expect("new id");
        let err = load_replay_plan(&db, id).expect_err("bitbucket replay must error");
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

    // F2: a FAILED refeed/refresh Result marks the row Failed (NOT Processed), and an Ok Result
    // marks it Processed. `record_terminal` is the single decision shared by the spawned process +
    // replay, so testing it locks the "no false Processed" guarantee without an AppHandle.
    #[test]
    fn record_terminal_maps_ok_to_processed_and_err_to_failed() {
        let db = Database::open_in_memory().expect("open db");
        let event = github_event("github:g1", "p1", 7);
        let id = store::insert_dedup(&db, &event, "raw", Some("{}"), None)
            .expect("insert")
            .expect("new id");

        // A failed refeed → Failed with the message (never a false Processed).
        record_terminal(&db, id, &Err(AppError::new("refeed boom"))).expect("record failed");
        let failed = store::get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(failed.status, crate::model::InboxStatus::Failed);
        assert_eq!(failed.error.as_deref(), Some("refeed boom"));

        // A subsequent Ok refeed (e.g. a later replay) → Processed, clearing the error.
        record_terminal(&db, id, &Ok(())).expect("record ok");
        let processed = store::get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(processed.status, crate::model::InboxStatus::Processed);
        assert_eq!(processed.error, None);
    }

    // F2 end-to-end at the replay decision level: replay of a GitHub entry whose stored JSON is
    // un-deserializable (the lib.rs github_refeed closure returns Err) ends with the row Failed.
    // We can't drive the real `replay` (needs an AppHandle), but the path is `load_replay_plan`
    // (Ok, carries the bad JSON) → refeed Err → `record_terminal(Err)` → Failed; this asserts the
    // tail of that chain on a row whose plan is a GitHub refeed.
    #[test]
    fn replay_failed_refeed_marks_entry_failed() {
        let db = Database::open_in_memory().expect("open db");
        let event = github_event("github:g1", "p1", 7);
        // Store a structurally-present but (for the closure) un-replayable JSON; the plan still
        // carries it (the inbox doesn't parse it — lib.rs does), so the failure surfaces at refeed.
        let id = store::insert_dedup(&db, &event, "raw", Some("not-a-webhook-event"), None)
            .expect("insert")
            .expect("new id");

        let plan = load_replay_plan(&db, id).expect("plan");
        assert!(matches!(plan.action, ReplayAction::RefeedGithub(_)));

        // Simulate the refeed closure failing (deser error in lib.rs) → record_terminal(Err).
        let refeed_result: AppResult<()> = Err(AppError::new("WebhookEvent 反序列化失败"));
        record_terminal(&db, id, &refeed_result).expect("record");
        let entry = store::get_entry(&db, id).expect("get").expect("exists");
        assert_eq!(
            entry.status,
            crate::model::InboxStatus::Failed,
            "a failed replay refeed marks the entry Failed, not Processed"
        );
        assert!(entry.error.is_some());
    }
}
