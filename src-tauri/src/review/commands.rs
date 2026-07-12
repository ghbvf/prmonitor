//! Review slice Tauri commands.

use tauri::Manager;

use crate::config::service as config_service;
use crate::db::Database;
use crate::error::{AppError, AppResult};
use crate::model::{CliTool, EngineKind, ReviewKind, ReviewLifecycleDispatch, SourceKind};
use crate::review::claim_store;
use crate::review::engine::{ReviewEngine, ReviewStartCapability, SessionId, StartReviewOutcome};
use crate::review::engines::claude::process::{claude_availability, ClaudeStatus};
use crate::review::engines::claude::ClaudeEngine;
use crate::review::engines::codex::{CodexEngine, CodexStatus};
use crate::review::engines::cursor::{CursorEngine, CursorStatus};
use crate::review::history_store::{self, HistoryItem};
use crate::review::session::{CommentUrlContext, SessionInfo, SessionStatus, StopTarget};
use crate::state::AppState;

/// Rejects a `pr_number` of 0 at the command boundary (AB#1043, codex F2). PR/MR numbers are
/// 1-based, so 0 is never a real PR — but the trigger funnel accepts a free-form `u64` from an
/// untrusted transport (the local REST API), and nothing downstream re-checks it, so a 0 would
/// flow into the engine and start a bogus `/pr-review 0`. Fail-closed BEFORE any project resolve
/// / engine dispatch, shared by both [`start_review`] and
/// manual starts so the single funnel can't be bypassed. Pure (no `AppHandle`) so it is
/// unit-testable. `pub(crate)` so the deeplink parser ([`crate::review::deeplink`], AB#1045)
/// rejects `pr=0` at parse time, the same boundary the CLI/local-API hit.
pub(crate) fn validate_pr_number(pr_number: u64) -> AppResult<()> {
    if pr_number == 0 {
        Err(AppError::new("pr 非法（PR 号必须大于 0）: 0"))
    } else {
        Ok(())
    }
}

/// Reports codex app-server availability for the StatusBar. Ensures the resident
/// connection (lazy start: first call spawns + handshakes, later calls reuse) and
/// reports `available` + version. The probe never errors (failures map to a
/// status struct); only the config read can fail.
///
/// The codex app-server is GLOBAL and single (one resident process for all
/// projects); its spawn-handshake cwd is the ACTIVE project's `repo_root`, read from
/// the config slice's public service (#35). Per-turn `cwd` scopes each review's
/// working dir, so this is only the handshake cwd. `AppConfig` stays config-private.
enum CodexStatusInput<T> {
    Resident(CodexStatus),
    Cold(T),
}

fn codex_status_input<T>(
    manager: &crate::review::engines::codex::CodexManager,
    cold: impl FnOnce() -> AppResult<T>,
) -> AppResult<CodexStatusInput<T>> {
    match manager.resident_status() {
        Some(status) => Ok(CodexStatusInput::Resident(status)),
        None => cold().map(CodexStatusInput::Cold),
    }
}

#[tauri::command]
pub async fn get_codex_status<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> AppResult<CodexStatus> {
    // A stopped or live resident is authoritative and does not need the currently configured
    // executable. Resolve config only for the cold-start path; otherwise a stale CLI path would
    // hide the user's stopped state or a healthy process that is already serving requests.
    match codex_status_input(&state.codex, || {
        Ok((
            config_service::resolve_cli(&app, CliTool::Codex, false)?,
            config_service::active_repo_root(&app)?,
        ))
    })? {
        CodexStatusInput::Resident(status) => Ok(status),
        CodexStatusInput::Cold((codex, repo_root)) => {
            Ok(state.codex.status(&codex, &repo_root).await)
        }
    }
}

/// Reports `claude` CLI availability for the StatusBar. One-shot `claude --version`
/// probe — claude has NO resident server (unlike codex), so there is no start/stop
/// and this takes no `app`/`state`/repo_root. The probe never errors.
#[tauri::command]
pub async fn get_claude_status<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> AppResult<ClaudeStatus> {
    let claude = config_service::resolve_cli(&app, CliTool::Claude, false)?;
    Ok(claude_availability(&claude).await)
}

enum CursorStatusInput<T> {
    Resident(CursorStatus),
    Cold(T),
}

fn cursor_status_input<T>(
    manager: &crate::review::engines::cursor::CursorManager,
    cold: impl FnOnce() -> AppResult<T>,
) -> AppResult<CursorStatusInput<T>> {
    match manager.resident_status() {
        Some(status) => Ok(CursorStatusInput::Resident(status)),
        None => cold().map(CursorStatusInput::Cold),
    }
}

/// Reports Cursor ACP (`agent acp`) availability for the StatusBar. Ensures the resident
/// connection (lazy start) like codex; failures map to a status struct.
#[tauri::command]
pub async fn get_cursor_status<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> AppResult<CursorStatus> {
    match cursor_status_input(&state.cursor, || {
        Ok((
            config_service::resolve_cli(&app, CliTool::Agent, false)?,
            config_service::active_repo_root(&app)?,
        ))
    })? {
        CursorStatusInput::Resident(status) => Ok(status),
        CursorStatusInput::Cold((agent, repo_root)) => {
            Ok(state.cursor.status(&agent, &repo_root).await)
        }
    }
}

/// 显式启动常驻 codex app-server（清除「已停止」标记并拉起握手）。返回最新状态。
/// 全局单例 codex 的握手 cwd 取「活动项目」的 `repo_root`（#35）；每轮 review 的实际工作目录由 per-turn `cwd` 覆盖。
#[tauri::command]
pub async fn start_codex<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> AppResult<CodexStatus> {
    let repo_root = config_service::active_repo_root(&app)?;
    let codex = config_service::resolve_cli(&app, CliTool::Codex, false)?;
    let status = state.codex.start(&codex, &repo_root).await;
    state.review_resume.fire()?;
    Ok(status)
}

/// 显式停止常驻 codex app-server（设「已停止」标记 + 杀进程；被动状态探测此后不再自动拉起，显式 review 仍会强制启动）。
/// 走统一错误漏斗 `AppResult`（与其余命令一致；`stop` 不会失败，故恒 `Ok`。前端 `invoke<CodexStatus>` 不变——`AppResult` 成功序列化为 `T`）。
#[tauri::command]
pub fn stop_codex(state: tauri::State<'_, AppState>) -> AppResult<CodexStatus> {
    Ok(state.codex.stop())
}

/// 显式启动常驻 Cursor ACP（清除「已停止」标记并拉起握手）。
#[tauri::command]
pub async fn start_cursor<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> AppResult<CursorStatus> {
    let repo_root = config_service::active_repo_root(&app)?;
    let agent = config_service::resolve_cli(&app, CliTool::Agent, false)?;
    let status = state.cursor.start(&agent, &repo_root).await;
    state.review_resume.fire()?;
    Ok(status)
}

/// 显式停止常驻 Cursor ACP（设「已停止」标记 + 杀进程）。
#[tauri::command]
pub fn stop_cursor(state: tauri::State<'_, AppState>) -> AppResult<CursorStatus> {
    Ok(state.cursor.stop())
}

/// Whether a review start is an EXPLICIT user action or an AUTOMATIC one (AB#1069). The distinction
/// is the codex stop-flag contract (PR #47 F1): an explicit trigger (UI button / CLI / deeplink)
/// `resume()`s a user-stopped codex; an automatic trigger (the outbox action executor, driven by the
/// rule engine) must NOT revive it — it respects the user's `stop_codex` exactly like
/// the outbox executor. Claude has no resident server / stop flag, so this only gates codex.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartTrigger {
    /// Manual UI/CLI/deeplink start — overrides a prior `stop_codex` (resumes codex).
    Explicit,
    /// Outbox / rule-engine start — respects `stop_codex` (never resumes). [`start_for_outbox`]
    /// short-circuits an already-stopped codex, so an `Auto` start here is reached only when live.
    Auto,
}

struct EngineStartRequest {
    pr_number: u64,
    kind: ReviewKind,
    trigger: StartTrigger,
    outbox_claim_id: Option<i64>,
}

/// The single source for engine selection on the START path (AB#1042/AB#1069): the shared
/// engine-selection-and-start body behind [`dispatch_engine`] (manual UI/CLI start, `Explicit`) AND
/// the outbox action executor's [`start_for_outbox`] (`Auto`). Builds the concrete engine for
/// `project.engine_kind` and starts the review, returning the RAW [`StartReviewOutcome`] — each
/// caller maps the outcome differently (a dedup is a benign error on the manual path, but a DONE
/// outbox row without ledger landing on the at-least-once path — see [`outbox_start_outcome`]).
/// `trigger` decides the codex
/// stop-flag handling: `Explicit` resumes a user-stopped codex; `Auto` does not (PR #47 F1).
///
/// Follow-up chat has its own engine selection in [`send_review_message`]. This is the only
/// START-path `match EngineKind` (manual UI/CLI AND the outbox executor route through it) — don't add
/// another start-path match. That exhaustiveness is the **Hard** carrier: a new variant without an
/// arm here is a compile error.
///
impl ReviewStartCapability {
    async fn start_via_engine<R: tauri::Runtime>(
        &self,
        app: &tauri::AppHandle<R>,
        state: &AppState,
        project: &config_service::Project,
        request: EngineStartRequest,
    ) -> AppResult<StartReviewOutcome> {
        let EngineStartRequest {
            pr_number,
            kind,
            trigger,
            // AB#1204: the owning outbox row id on the OUTBOX path (`Some`), `None` on the manual
            // path. It reaches the engine's claim breadcrumb before the turn runs.
            outbox_claim_id,
        } = request;
        // Snapshot the comment-URL source context from the project NOW (AB#1042), so the terminal
        // `finalize_turn` resolves the pr-review comment URL against the project the review ran
        // against — never a config edited mid-review. Both engines carry this owned context into
        // their `Starting` session; built once here since the fields are identical for either.
        let url_ctx = comment_url_ctx_from(app, project);
        let outcome = match project.engine_kind {
            EngineKind::Codex => {
                let skill_abs = skill_abs_path(&project.repo_root, &project.skill_rel_path);
                // Codex stop-flag contract (PR #47 F1): an EXPLICIT trigger (UI / CLI / deeplink)
                // overrides a prior `stop_codex` — `resume()` clears the user-stop flag BEFORE
                // `engine.start()` reaches the `connection()` funnel (which refuses when stopped). An
                // AUTO trigger (outbox / rule-engine) does NOT resume: it respects the user's stop just
                // like automatic outbox starts (the caller `start_for_outbox` already short-circuits a
                // stopped codex, so an `Auto` start reaches here only when codex is live).
                if trigger == StartTrigger::Explicit {
                    state.codex.resume();
                    state.review_resume.fire()?;
                }
                let codex_cli = config_service::resolve_cli(app, CliTool::Codex, false)?;
                let engine = CodexEngine {
                    app,
                    codex: &state.codex,
                    registry: &state.sessions,
                    codex_cli: &codex_cli,
                    project_id: &project.id,
                    repo: &project.repo,
                    repo_root: &project.repo_root,
                    skill_abs_path: &skill_abs,
                    codex_model: &project.codex_model,
                    codex_reasoning_effort: project.codex_reasoning_effort,
                    url_ctx,
                    // `start` takes `pr_number` as a method arg; the field is the follow-up path's.
                    pr_number: 0,
                    session_info: None,
                    // AB#1204: `Some(outbox_id)` on the outbox path → claim breadcrumb at thread/start.
                    outbox_claim_id,
                };
                engine.start(self, pr_number, kind).await
            }
            EngineKind::Claude => {
                let claude_cli = config_service::resolve_cli(app, CliTool::Claude, false)?;
                let engine = ClaudeEngine {
                    app,
                    claude: &state.claude,
                    registry: &state.sessions,
                    claude_cli: &claude_cli,
                    project_id: &project.id,
                    repo: &project.repo,
                    repo_root: &project.repo_root,
                    claude_model: &project.claude_model,
                    claude_effort: project.claude_effort,
                    url_ctx,
                    // `start` takes `pr_number` as a method arg; the field is the follow-up path's.
                    pr_number: 0,
                    session_info: None,
                    // AB#1204: `Some(outbox_id)` on the outbox path → claim breadcrumb at thread/start.
                    outbox_claim_id,
                };
                engine.start(self, pr_number, kind).await
            }
            EngineKind::Cursor => {
                // Explicit trigger overrides a prior cursor stop (parity with Codex).
                if trigger == StartTrigger::Explicit {
                    state.cursor.resume();
                    state.review_resume.fire()?;
                }
                let agent_cli = config_service::resolve_cli(app, CliTool::Agent, false)?;
                let engine = CursorEngine {
                    app,
                    cursor: &state.cursor,
                    registry: &state.sessions,
                    agent_cli: &agent_cli,
                    project_id: &project.id,
                    repo: &project.repo,
                    repo_root: &project.repo_root,
                    url_ctx,
                    pr_number: 0,
                    session_info: None,
                    outbox_claim_id,
                };
                engine.start(self, pr_number, kind).await
            }
        }?;
        if let StartReviewOutcome::Started(thread_id) = &outcome {
            if let Err(e) = state.review_lifecycle.fire(ReviewLifecycleDispatch {
                project_id: project.id.clone(),
                pr_number,
                kind,
                thread_id: thread_id.clone(),
                event: crate::model::ReviewLifecycleEvent::Started,
                comment_url: None,
            }) {
                eprintln!(
                    "review lifecycle started 通知入队失败（{thread_id}）：{}",
                    e.message
                );
            }
        }
        Ok(outcome)
    }
}

/// The MANUAL / explicit start path (AB#1042): the shared dispatch body behind BOTH
/// [`start_review`] (project resolved by id)
/// `reference`). Thin mapper over [`start_via_engine`]: `Started` → the session id; `Deduped` (the
/// registry already has an in-flight review for this `(project_id, pr, kind)`) → a benign "already in
/// flight" error — a re-start does NOT double-start; stop the running one first to re-review. This
/// `Deduped → Err` is correct for a USER action (a re-click deserves the message); the outbox path
/// maps the same `Deduped` to a done-but-not-ledgered outcome instead (see
/// [`outbox_start_outcome`]).
///
async fn dispatch_engine<R: tauri::Runtime>(
    capability: &ReviewStartCapability,
    app: &tauri::AppHandle<R>,
    state: &AppState,
    project: &config_service::Project,
    pr_number: u64,
    kind: ReviewKind,
) -> AppResult<SessionId> {
    // Manual path: no outbox row, so `None` — the engine writes no AB#1204 claim breadcrumb.
    match capability
        .start_via_engine(
            app,
            state,
            project,
            EngineStartRequest {
                pr_number,
                kind,
                trigger: StartTrigger::Explicit,
                outbox_claim_id: None,
            },
        )
        .await?
    {
        StartReviewOutcome::Started(session_id) => Ok(session_id),
        StartReviewOutcome::Deduped => Err(AppError::new(format!(
            "PR {pr_number} 的 {kind} review 已在进行中"
        ))),
    }
}

/// The outbox action executor's entry into the review funnel (AB#1069): the same funnel
/// [`start_review`] goes through (`validate_pr_number` → `project_validated` →
/// [`start_via_engine`]), so a replayed `review`/`check` action CANNOT bypass any guard. Folds the
/// outcome mapping in ([`outbox_start_outcome`]) so the worker can mark `Ok` rows done while the
/// composition root can still distinguish a real start / suppressed replay from an active-session
/// dedupe for ledger landing.
///
/// **AUTO trigger (PR #47 F1)**: an automatic outbox review runs via [`StartTrigger::Auto`]
/// (NO `resume()`), respecting a user's `stop_codex`. A stopped Codex returns the sealed
/// `ActionExecutionResult::Blocked`; the worker persists `blocked` without consuming an attempt.
/// The shared resume seam later moves blocked review actions back to `pending` and wakes the worker.
/// Explicit review requests resume Codex before entering this same engine/session funnel.
///
/// `kind` comes from the sealed action payload as [`ReviewKind`]. `pr_number` comes from the
/// persisted/replayed payload, so it is re-validated. `pub(crate)`
/// so the composition root (`lib.rs`, outside the `review` module) can call it — it names only
/// `config`/`state`/review-internal types, never `crate::outbox`, so the slice boundary holds.
///
/// **Cross-restart dedup (AB#1204)**: `try_reserve_pair` is in-memory only, so after a restart a
/// replayed action whose review already ran would reserve freely → a DUPLICATE review. A write-ahead
/// [`claim_store`] claim keyed by `outbox_id` (the row IS the unit of at-least-once replay) closes
/// that window: on a replay whose claim already carries a `thread_id`, the prior review's durable
/// outcome is resolved and the duplicate suppressed (returns `Ok` → row `done`). `outbox_id` is the
/// row's id (`OutboxAction::id`), passed by the composition root.
///
/// **F1 (AB#1204) — breadcrumb written at thread/start, not after this returns**: the claim's
/// `thread_id` breadcrumb is recorded INSIDE the engine's `start` (via the `outbox_claim_id` we pass
/// to [`start_via_engine`]) — right after `thread/start` yields a stable thread id. The session row
/// and claim breadcrumb commit in one transaction BEFORE registry promotion / turn execution. A
/// linkage failure rolls the transaction back, leaves the reservation guard armed, and returns a
/// retryable error rather than exposing a false-live `Starting` session. The `Deduped` outcome (F5)
/// deliberately writes NO breadcrumb — its claim row keeps a NULL `thread_id`, which is EXPECTED: an
/// existing in-flight registry session for this `(project, pr, kind)` needs no new thread, and the
/// action's intent ("this PR is being reviewed") already holds. The next replay re-takes the same
/// `Deduped → Ok` path; once the row terminalizes the composition root's `release_claim` drops the
/// NULL-thread claim normally (idempotent) — see `release_claim_cleans_up_null_thread_claim` in
/// `claim_store.rs`. The chain is idempotent.
pub(crate) async fn start_for_outbox<R: tauri::Runtime>(
    capability: &ReviewStartCapability,
    app: &tauri::AppHandle<R>,
    state: &AppState,
    project_id: &str,
    pr_number: u64,
    kind: ReviewKind,
    outbox_id: i64,
) -> AppResult<OutboxReviewStartOutcome> {
    // Fail-closed on a replayed payload: a `pr = 0` must not reach the engine.
    validate_pr_number(pr_number)?;
    let project = config_service::project_validated(app, project_id)?;

    // AB#1204 cross-restart dedup. Write-ahead a claim keyed by the OUTBOX ROW id BEFORE starting
    // (propagating error: a write failure retries the row, nothing started yet). `begin_claim`
    // returns `Some(thread_id)` ONLY when THIS row already reached `thread/start` in a prior run (a
    // crash-replay) — resolve that review's durable outcome and SUPPRESS the duplicate if it ran far
    // enough to have posted its `pm:` comment. A fresh claim, or a claim with no thread (crashed
    // before `thread/start`, so no comment), falls through to a genuine start (at-least-once).
    let db = app.state::<Database>();
    if let Some(prior_thread_id) =
        claim_store::begin_claim(db.inner(), outbox_id, project_id, pr_number, kind)?
    {
        if replayed_review_should_suppress(db.inner(), &prior_thread_id)? {
            return Ok(OutboxReviewStartOutcome::SuppressedReplay {
                thread_id: prior_thread_id,
            });
        }
    }

    // Auto trigger: no `resume()` (respects `stop_codex`). A stopped codex makes `engine.start`
    // return a retryable `Err` (connection refused) — NOT a false `Ok`/`done` (F2). See the doc above.
    //
    // F1 (AB#1204): pass `Some(outbox_id)` so the engine writes the claim's `thread_id` breadcrumb
    // at `thread/start` — INSIDE `start`, transactionally with the durable session and BEFORE
    // registry promotion / turn execution. A linkage failure rolls back and retries cleanly; the
    // `Deduped` path writes none (F5). See the contract above.
    let outcome = capability
        .start_via_engine(
            app,
            state,
            &project,
            EngineStartRequest {
                pr_number,
                kind,
                trigger: StartTrigger::Auto,
                outbox_claim_id: Some(outbox_id),
            },
        )
        .await?;
    Ok(match outcome {
        StartReviewOutcome::Started(thread_id) => OutboxReviewStartOutcome::Started { thread_id },
        StartReviewOutcome::Deduped => {
            match state.sessions.stop_target(project_id, pr_number, kind) {
                crate::review::session::StopTarget::Live(thread_id) => {
                    OutboxReviewStartOutcome::ActiveDeduped { thread_id }
                }
                _ => return Err(AppError::new("active review dedupe missing session")),
            }
        }
    })
}

/// Decide whether a crash-replayed outbox review (whose claim already carries a `thread_id`) should
/// be SUPPRESSED rather than re-run (AB#1204). Resolves the prior review's durable `review_session`
/// row and biases toward suppress — re-run ONLY when we can PROVE no `pm:` comment was posted: the
/// session `Failed` with an EMPTY `turn_id` (the turn never started; `turn_id` is set at
/// `set_running`, strictly before the codex skill posts). A `Done`, a `Failed`-with-a-turn, an
/// unexpected still-live status, or a pruned/missing row all suppress — a duplicate `pm:` comment is
/// the worse failure than an occasional missed retry (mirrors the outbox `Deduped → Ok` philosophy).
///
/// Why the still-live statuses (`Starting`/`Running`/`Interrupting`) ALSO suppress: on the normal
/// path `lib.rs`'s startup `fail_orphaned_sessions` runs BEFORE the outbox worker, so any session
/// left non-terminal by a dead process is already reconciled to `Failed` before this is reached
/// (see the ORDERING note in `lib.rs`). A still-live status arriving here is therefore unexpected —
/// and `Failed`-with-empty-turn is the ONLY proof-of-no-comment case, so every non-matching status
/// (including these) takes the conservative suppress branch. The tests below pin that.
///
/// **Known residual window (best-effort turn_id persist):** `turn_id` lands in the durable row via
/// `session.rs::persist_session`, which is BEST-EFFORT (it logs + swallows a DB error; see its
/// doc-comment). So a turn can actually start — and the codex skill can post its `pm:` comment —
/// while the `persist_session` that would record its `turn_id` FAILS, leaving the DB row at
/// `Failed` (after the orphan flip) with an EMPTY `turn_id`. This function then reads `Failed +
/// empty turn` and rules the replay safe to re-run ⇒ a DUPLICATE `pm:` comment. This is an
/// inherited residual of the pre-existing best-effort persist, not a new regression — the claim
/// still closes the FAR larger window (a fully-recorded prior review). Closing this last sliver
/// needs the funnel's downstream Hard-ened (e.g. a non-swallowing turn_id write-ahead, or keying
/// the proof on the comment-URL rather than turn_id presence); tracked as a follow-up issue.
fn replayed_review_should_suppress(db: &Database, thread_id: &str) -> AppResult<bool> {
    Ok(match history_store::get_session(db, thread_id)? {
        Some(s) => !(s.status == SessionStatus::Failed && s.turn_id.is_empty()),
        None => true,
    })
}

/// The executor-visible outcome of a review/check outbox action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OutboxReviewStartOutcome {
    /// This row started a new review session for its candidate.
    Started { thread_id: String },
    /// This row is a crash replay whose prior started review is already durably visible; no duplicate
    /// start was needed, but the candidate was reviewed and may be ledgered.
    SuppressedReplay { thread_id: String },
    /// A live in-memory session already covers `(project, pr, kind)`. This makes the row `done`, but
    /// it must NOT land this candidate's `(pr, head, kind)` ledger key because the active session may
    /// be for a different head.
    ActiveDeduped { thread_id: String },
}

impl OutboxReviewStartOutcome {
    pub(crate) fn thread_id(&self) -> &str {
        match self {
            Self::Started { thread_id }
            | Self::SuppressedReplay { thread_id }
            | Self::ActiveDeduped { thread_id } => thread_id,
        }
    }
}

/// Map a [`StartReviewOutcome`] to the OUTBOX action outcome (AB#1069/#1379). `Deduped` is still a
/// successful outbox execution (the row can be `done`), but it is no longer indistinguishable from a
/// real start: the composition root uses [`should_record_dispatch_ledger`] to avoid recording a head
/// key that may not have been reviewed.
#[cfg(test)]
pub(crate) fn outbox_start_outcome(outcome: StartReviewOutcome) -> OutboxReviewStartOutcome {
    match outcome {
        StartReviewOutcome::Started(thread_id) => OutboxReviewStartOutcome::Started { thread_id },
        StartReviewOutcome::Deduped => OutboxReviewStartOutcome::ActiveDeduped {
            thread_id: String::new(),
        },
    }
}

/// Whether this successful outbox executor outcome should land the PR dispatch ledger.
pub(crate) fn should_record_dispatch_ledger(outcome: OutboxReviewStartOutcome) -> bool {
    matches!(
        outcome,
        OutboxReviewStartOutcome::Started { .. }
            | OutboxReviewStartOutcome::SuppressedReplay { .. }
    )
}

/// Build the IMMUTABLE comment-URL source context (AB#1042) from a project — the SINGLE
/// source shared by [`dispatch_engine`] (start path) and [`send_review_message`] (follow-up
/// path), so both snapshot the same `(source_kind, repo, azure_org, azure_project)` fields
/// the terminal `finalize_turn` resolves the pr-review comment URL against. Pinning one
/// builder keeps the two paths from drifting on which project fields the URL is resolved from.
fn optional_comment_url_cli(
    source_kind: SourceKind,
    resolve_gh: impl FnOnce() -> AppResult<config_service::ResolvedCli>,
) -> Option<config_service::ResolvedCli> {
    (source_kind == SourceKind::Github)
        .then(resolve_gh)
        .and_then(Result::ok)
}

fn comment_url_ctx_from<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project: &config_service::Project,
) -> CommentUrlContext {
    CommentUrlContext {
        gh: optional_comment_url_cli(project.source_kind, || {
            config_service::resolve_cli(app, CliTool::Gh, false)
        }),
        source_kind: project.source_kind,
        repo: project.repo.clone(),
        azure_org: project.azure_org.clone(),
        azure_project: project.azure_project.clone(),
    }
}

/// Resolve a session's `pr_number` for the follow-up (`send_review_message`) path: try the
/// in-memory registry first (a session finished THIS app run is still live), else fall back
/// to the durable `review_session` row (after a restart the registry is empty but a `Done`
/// row persists). Errors if neither has it — a follow-up to a session we never knew about.
/// A PR number is unique only within a project, so resolving it FROM the session row (not
/// from a caller-supplied number) keeps the follow-up turn keyed to the exact session.
///
/// SCOPED by `project_id` (mirrors `get_session_history`'s F6 `AND s.project_id = ?`): BOTH
/// the in-memory and the durable path verify the resolved session belongs to the caller's
/// project. A `thread_id` is globally unique, so without this check a caller could supply
/// another project's `thread_id` and resume that session under the wrong project (cross-tenant
/// confusion). A project mismatch reports the SAME "未找到 review 会话（无法续聊）" error as a
/// genuinely-absent session — an attacker learns nothing about another project's sessions.
fn resolve_session_info<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    project_id: &str,
    thread_id: &str,
) -> AppResult<SessionInfo> {
    if let Some(info) = state.sessions.get(thread_id) {
        if info.project_id != project_id {
            return Err(AppError::new(format!(
                "未找到 review 会话（无法续聊）: {thread_id}"
            )));
        }
        return Ok(info);
    }
    let db = app.state::<Database>();
    match history_store::get_session(db.inner(), thread_id)? {
        Some(info) if info.project_id == project_id => Ok(info),
        _ => Err(AppError::new(format!(
            "未找到 review 会话（无法续聊）: {thread_id}"
        ))),
    }
}

/// Continue an existing review session with a follow-up user `message` (chat continuation):
/// after a session reaches a terminal status the user types a follow-up, the backend
/// continues the SAME conversation with the AI and streams the reply back through the
/// existing `review:event` pipeline (`MessageDelta` / `ReasoningDelta` / `TurnCompleted`).
/// The user's typed message is persisted to the session history under the CALLER-supplied
/// `user_item_id` (so the frontend's optimistic bubble id == the persisted id, and
/// reopen-dedup works).
///
/// Engine selection mirrors [`dispatch_engine`]: an exhaustive `match project.engine_kind`
/// over the sealed [`EngineKind`] (model.rs) — a new variant without an arm is a compile
/// error (the **Hard** carrier), so a follow-up can never silently miss a new engine. The
/// command routes through the `ReviewEngine::send_message` trait method (NOT a downcast).
#[tauri::command]
pub async fn send_review_message<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    project_id: String,
    thread_id: String,
    message: String,
    user_item_id: String,
) -> AppResult<()> {
    // Fail-fast at the boundary: an empty / whitespace-only follow-up has nothing to send — reject
    // it before any project resolve / engine dispatch.
    if message.trim().is_empty() {
        return Err(AppError::new("消息为空".to_string()));
    }
    // An empty / whitespace-only `user_item_id` would collide on the history table's
    // `UNIQUE(thread_id, item_id)` constraint and COALESCE every user message of the session
    // into one row — reject it here so each follow-up persists as its own bubble.
    if user_item_id.trim().is_empty() {
        return Err(AppError::new("user_item_id 为空".to_string()));
    }
    // Cap the follow-up length: a huge message would be passed to `claude` as a process ARG,
    // failing the spawn at `execve` ARG_MAX (typically ~256KB on macOS, ~2MB on Linux) with an
    // opaque error. Reject oversized input here with a friendly message. 128 KiB (bytes, not
    // chars — `len()` is the encoded byte count that hits ARG_MAX) leaves ample headroom.
    if message.len() > 131072 {
        return Err(AppError::new("消息过长（上限 128KB）".to_string()));
    }
    // Resolve + validate the owning project (#35) — same per-project validation as
    // `start_review` (the review slice stays on `config::service`, never `config::model`).
    let project = config_service::project_validated(&app, &project_id)?;
    // Resolve the session identity (in-memory live row, else the durable row), SCOPED to
    // this project so a caller can't resume another project's session by raw `thread_id`.
    // The session's `engine_kind` is authoritative: changing project config after the review
    // must not route this existing conversation to a different engine.
    let session_info = resolve_session_info(&app, &state, &project_id, &thread_id)?;
    let pr_number = session_info.pr_number;
    let url_ctx = comment_url_ctx_from(&app, &project);
    match session_info.engine_kind {
        EngineKind::Codex => {
            // MANUAL / explicit force-start (parity with `dispatch_engine`'s Codex arm): a
            // follow-up is an explicit user action, so clear any prior `stop_codex` before
            // the `connection()` funnel (which refuses when stopped).
            state.codex.resume();
            state.review_resume.fire()?;
            let codex_cli = config_service::resolve_cli(&app, CliTool::Codex, false)?;
            let engine = CodexEngine {
                app: &app,
                codex: &state.codex,
                registry: &state.sessions,
                codex_cli: &codex_cli,
                project_id: &project.id,
                repo: &project.repo,
                repo_root: &project.repo_root,
                skill_abs_path: "",
                codex_model: &project.codex_model,
                codex_reasoning_effort: project.codex_reasoning_effort,
                url_ctx,
                pr_number,
                session_info: Some(session_info.clone()),
                // Follow-up path: no outbox row → no AB#1204 claim breadcrumb.
                outbox_claim_id: None,
            };
            engine
                .send_message(&thread_id, &message, &user_item_id)
                .await
        }
        EngineKind::Claude => {
            let claude_cli = config_service::resolve_cli(&app, CliTool::Claude, false)?;
            let engine = ClaudeEngine {
                app: &app,
                claude: &state.claude,
                registry: &state.sessions,
                claude_cli: &claude_cli,
                project_id: &project.id,
                repo: &project.repo,
                repo_root: &project.repo_root,
                claude_model: &project.claude_model,
                claude_effort: project.claude_effort,
                url_ctx,
                pr_number,
                session_info: Some(session_info.clone()),
                // Follow-up path: no outbox row → no AB#1204 claim breadcrumb.
                outbox_claim_id: None,
            };
            engine
                .send_message(&thread_id, &message, &user_item_id)
                .await
        }
        EngineKind::Cursor => {
            state.cursor.resume();
            state.review_resume.fire()?;
            let agent_cli = config_service::resolve_cli(&app, CliTool::Agent, false)?;
            let engine = CursorEngine {
                app: &app,
                cursor: &state.cursor,
                registry: &state.sessions,
                agent_cli: &agent_cli,
                project_id: &project.id,
                repo: &project.repo,
                repo_root: &project.repo_root,
                url_ctx,
                pr_number,
                session_info: Some(session_info.clone()),
                outbox_claim_id: None,
            };
            engine
                .send_message(&thread_id, &message, &user_item_id)
                .await
        }
    }
}

/// Start a review for `(project_id, pr_number)` (`kind` = `"review"` or `"check"`),
/// returning the session id (codex `threadId`). Output streams out-of-band via the
/// `review:event` Tauri event ([`crate::events::ReviewEvent`]), each event stamped
/// with `project_id` (#35) so the frontend routes it to the owning project.
pub(crate) async fn start_review_authorized<R: tauri::Runtime>(
    capability: &ReviewStartCapability,
    app: tauri::AppHandle<R>,
    state: &AppState,
    project_id: String,
    pr_number: u64,
    kind: ReviewKind,
) -> AppResult<SessionId> {
    validate_pr_number(pr_number)?;
    // Resolve the project being reviewed (#35) and re-check ITS filesystem-dependent
    // paths so an absent / escaping `skillRelPath` (e.g. a hand-edited config) fails
    // before we attach the skill path to the turn, rather than handing codex a bad path.
    // `project_validated` is the per-project analogue of the old `load_validated`; the
    // review slice still depends only on `config::service`, never `config::model`.
    let project = config_service::project_validated(&app, &project_id)?;
    dispatch_engine(capability, &app, state, &project, pr_number, kind).await
}

/// Interrupt a running review session by its `threadId` (#718): the shared stop body behind the
/// manual [`stop_review`] command AND the AB#1069 outbox `stop-review` action ([`stop_for_outbox`]).
/// Prefer the session's persisted `engine_kind` when known; otherwise probe Claude's kill map,
/// then fall through to Codex (legacy ownership probe). Cursor cancels via `session/cancel` on
/// the resident ACP connection.
pub(crate) async fn stop_session_by_id<R: tauri::Runtime>(
    _app: &tauri::AppHandle<R>,
    state: &AppState,
    session_id: &str,
) -> AppResult<()> {
    if let Some(info) = state.sessions.get(session_id) {
        match info.engine_kind {
            EngineKind::Claude => {
                let _ = state.claude.stop(session_id);
                return Ok(());
            }
            EngineKind::Cursor => {
                if let Some(client) = state.cursor.existing_client() {
                    crate::review::engines::cursor::process::session_cancel(&client, session_id)
                        .await?;
                }
                return Ok(());
            }
            EngineKind::Codex => {
                return crate::review::session::stop_review(
                    &state.codex,
                    &state.sessions,
                    session_id,
                )
                .await;
            }
        }
    }
    // Registry miss: try Cursor cancel before Claude/Codex ownership probes.
    if let Some(client) = state.cursor.existing_client() {
        if crate::review::engines::cursor::process::session_cancel(&client, session_id)
            .await
            .is_ok()
        {
            return Ok(());
        }
    }
    if state.claude.stop(session_id) {
        return Ok(());
    }
    crate::review::session::stop_review(&state.codex, &state.sessions, session_id).await
}

/// Interrupt a running review session (by its `threadId`). The terminal `turnCompleted` (status
/// `interrupted`) follows on the `review:event` stream. Thin wrapper over [`stop_session_by_id`].
#[tauri::command]
pub async fn stop_review<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> AppResult<()> {
    stop_session_by_id(&app, &state, &session_id).await
}

/// Absorb a stop-review TOCTOU race (AB#1069): given the `stop_session_by_id` result and whether the
/// session is STILL active immediately after, decide the outbox action result. A stop that FAILED on
/// a session that has since vanished is the benign race (the session ended between our lookup and the
/// interrupt) — the stop's intent ("no running turn") already holds, so → `Ok(())` (idempotent, no
/// dead-letter). A stop that failed while the session is STILL active is a genuine interrupt failure →
/// propagate for retry. Pure (no `AppHandle`) so the contract is unit-tested without a Tauri runtime.
fn absorb_stop_toctou(result: AppResult<()>, still_active: bool) -> AppResult<()> {
    match result {
        Ok(()) => Ok(()),
        // The session vanished mid-stop → the race resolved in our favor; the intent holds.
        Err(_) if !still_active => Ok(()),
        // Still active but the interrupt errored → a real failure, retry it.
        Err(e) => Err(e),
    }
}

/// The outbox action executor's stop-review entry (AB#1069): interrupt the in-flight session for
/// `(project_id, pr_number, kind)`. Resolves the triple to a [`StopTarget`] (AB#1069 F4) and acts on
/// each of the three states:
/// - [`Absent`](StopTarget::Absent) → `Ok(())`. IDEMPOTENT for at-least-once execution — a crash-replay
///   or a stop fired after the review already self-completed finds nothing to stop, and that intent
///   ("no running turn") already holds; an `Err` here would dead-letter a no-op.
/// - [`Reserved`](StopTarget::Reserved) → a RETRYABLE `Err`. A start is mid-flight (reserved, not yet
///   promoted to a `thread_id`): returning `Ok` would DROP the stop and let the start promote to
///   `Running` unimpeded (the F4 bug). Retrying re-resolves on a later sweep — by then the start has
///   promoted to `Live` (interrupt it) or failed (→ `Absent` → `Ok`). The reservation→promotion window
///   is the `thread/start` latency, so this resolves within a sweep or two.
/// - [`Live`](StopTarget::Live) → interrupt by `thread_id`. If the stop FAILS, re-resolve: a session
///   that vanished mid-stop (no longer `Live`) is the benign TOCTOU race → absorb to `Ok`; a still-`Live`
///   session means a genuine interrupt failure → propagate for retry (see [`absorb_stop_toctou`]).
///
/// `pub(crate)` for the composition root; the persisted payload has already decoded `kind` into
/// [`ReviewKind`], so invalid modes fail before this entry point.
pub(crate) async fn stop_for_outbox<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    project_id: &str,
    pr_number: u64,
    kind: ReviewKind,
) -> AppResult<()> {
    validate_pr_number(pr_number)?;
    let thread_id = match state.sessions.stop_target(project_id, pr_number, kind) {
        StopTarget::Absent => return Ok(()), // nothing in flight → intent holds (idempotent)
        // A start is mid-flight (reserved, no thread_id yet): a RETRYABLE error so the stop is not
        // dropped — the next sweep finds it promoted (Live) or failed (Absent).
        StopTarget::Reserved => {
            return Err(AppError::new(format!(
            "stop-review：PR {pr_number} 的 {kind} 评审正在启动（reserved，未 promote），稍后重试"
        )))
        }
        StopTarget::Live(thread_id) => thread_id,
    };
    let result = stop_session_by_id(app, state, &thread_id).await;
    // Re-resolve (typed, not string-matched): if the session is no longer Live, a stop error was the
    // benign TOCTOU race; only a still-Live session's error is a real failure worth retrying.
    let still_live = matches!(
        state.sessions.stop_target(project_id, pr_number, kind),
        StopTarget::Live(_)
    );
    absorb_stop_toctou(result, still_live)
}

/// Snapshot of all review sessions (running + finished) for the UI — the IN-MEMORY
/// registry (live status). After a restart this is empty; [`get_pr_sessions`] reads the
/// durable table instead.
#[tauri::command]
pub fn list_review_sessions(state: tauri::State<'_, AppState>) -> Vec<SessionInfo> {
    state.sessions.list()
}

/// A session's persisted history items in stream order (#70) — the message/reasoning
/// blocks produced before the user opened the session. Drives the review panel's "open a
/// history session and see prior content" (#67) by hydrating the per-session buffer.
#[tauri::command]
pub fn get_session_history<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: String,
    pr_number: u64,
    thread_id: String,
) -> AppResult<Vec<HistoryItem>> {
    let db = app.state::<Database>();
    // Scoped read (pr-review F6): history is returned only if `thread_id` belongs to this
    // `(project_id, pr_number)`, so a caller can't pull another PR's history by raw id.
    history_store::get_history(db.inner(), &project_id, pr_number, &thread_id)
}

/// A PR's persisted sessions, newest first, from the durable `review_session` table (#70)
/// — so each PR can restore its session list after a restart (the #67 nav associates
/// sessions per PR). Distinct from [`list_review_sessions`] (in-memory live snapshot).
#[tauri::command]
pub fn get_pr_sessions<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: String,
    pr_number: u64,
) -> AppResult<Vec<SessionInfo>> {
    let db = app.state::<Database>();
    history_store::get_pr_sessions(db.inner(), &project_id, pr_number)
}

/// Resolve the absolute path to the pr-review skill file codex attaches to the
/// turn. `repo_root` is an absolute dir and `skill_rel_path` a relative path under
/// it (both config-validated), so the join is absolute and infallible.
///
/// Single source for codex start callsites. The review slice owns the codex skill-path concept, so
/// it lives here (`pub(crate)`).
pub(crate) fn skill_abs_path(repo_root: &str, skill_rel_path: &str) -> String {
    std::path::Path::new(repo_root)
        .join(skill_rel_path)
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_codex_status_skips_failing_cold_start_resolution() {
        let manager = crate::review::engines::codex::CodexManager::default();
        manager.stop();
        let mut called = false;
        let input = codex_status_input(&manager, || -> AppResult<()> {
            called = true;
            Err(AppError::new("stale configured path"))
        })
        .expect("resident status must bypass cold resolution");
        assert!(!called);
        let CodexStatusInput::Resident(status) = input else {
            panic!("stopped manager must return resident snapshot");
        };
        assert!(!status.available);
        assert!(!status.desired_running);
    }

    #[test]
    fn github_comment_url_context_tolerates_missing_gh() {
        let gh =
            optional_comment_url_cli(SourceKind::Github, || Err(AppError::new("gh unavailable")));
        assert!(gh.is_none(), "comment URL enrichment must stay best-effort");
    }

    #[test]
    fn non_github_comment_url_context_does_not_resolve_gh() {
        let mut called = false;
        let gh = optional_comment_url_cli(SourceKind::Azure, || {
            called = true;
            Err(AppError::new("must not run"))
        });
        assert!(gh.is_none());
        assert!(!called, "non-GitHub sources must not resolve gh");
    }

    #[test]
    fn validate_pr_number_rejects_zero_only() {
        // AB#1043 codex F2: 0 is never a real PR — reject it before any side effect so an
        // untrusted transport can't push `/pr-review 0` into the engine. Every positive number
        // is accepted (PR numbers are 1-based; there is no upper bound to enforce here).
        assert!(validate_pr_number(0).is_err(), "pr=0 must be rejected");
        for ok in [1, 7, 160, u64::MAX] {
            assert!(validate_pr_number(ok).is_ok(), "pr={ok} must be accepted");
        }
    }

    // AB#1069/#1379 acceptance lock: `Deduped` is still a successful outbox execution (done, no
    // retry/dead-letter), but it is NOT ledgerable because the active in-memory session is keyed
    // only by `(project, pr, kind)` and may be reviewing a different head.
    #[test]
    fn outbox_start_outcome_distinguishes_ledgerable_from_active_deduped() {
        let started = outbox_start_outcome(StartReviewOutcome::Started("t1".to_string()));
        let deduped = outbox_start_outcome(StartReviewOutcome::Deduped);
        assert_eq!(
            started,
            OutboxReviewStartOutcome::Started {
                thread_id: "t1".to_string()
            }
        );
        assert_eq!(
            deduped,
            OutboxReviewStartOutcome::ActiveDeduped {
                thread_id: String::new()
            }
        );
        assert!(
            should_record_dispatch_ledger(started),
            "Started reviewed this candidate → ledger it"
        );
        assert!(
            should_record_dispatch_ledger(OutboxReviewStartOutcome::SuppressedReplay {
                thread_id: "t1".to_string()
            }),
            "suppressed replay already started this row's candidate earlier → ledger it"
        );
        assert!(
            !should_record_dispatch_ledger(deduped),
            "active dedupe may be another head → do not ledger this candidate"
        );
    }

    // AB#1069 idempotent stop: `stop_for_outbox` re-checks the registry after a failed stop. A stop
    // error whose session has since vanished is the benign TOCTOU race → Ok (no dead-letter); a stop
    // error on a still-active session is a real failure → propagate. (The "no live session at all"
    // path is covered by `session::active_session_for` returning None → early Ok, tested in
    // `session.rs`.)
    #[test]
    fn absorb_stop_toctou_only_propagates_real_failures() {
        // Success stays success regardless of liveness.
        assert!(absorb_stop_toctou(Ok(()), true).is_ok());
        assert!(absorb_stop_toctou(Ok(()), false).is_ok());
        // Stop error + session gone → benign race, absorbed to Ok (idempotent).
        assert!(
            absorb_stop_toctou(Err(AppError::new("未找到 review 会话")), false).is_ok(),
            "session vanished mid-stop → Ok (no dead-letter)"
        );
        // Stop error + session still active → a genuine interrupt failure, propagated for retry.
        assert!(
            absorb_stop_toctou(Err(AppError::new("interrupt failed")), true).is_err(),
            "still-active session's stop error must propagate"
        );
    }

    // ── AB#1204 cross-restart dedup: the replay-suppression decision ────────────────────────────
    //
    // The closure proof (Medium runtime-guard carrier). `start_for_outbox` consults a write-ahead
    // `outbox_id` claim; on a crash-replay whose claim already carries a `thread_id`, this decision
    // resolves the prior review's durable `review_session` row to suppress-vs-rerun. Seeding a row +
    // asserting the decision is the unit-testable core of the window closure (the full async start is
    // exercised by the outbox service tests + the claim_store round-trip). Bias: suppress unless we
    // can PROVE no `pm:` comment was posted.
    fn seed_session(db: &Database, thread_id: &str, status: SessionStatus, turn_id: &str) {
        history_store::upsert_session(
            db,
            &SessionInfo {
                project_id: "p1".to_string(),
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
                pr_number: 7,
                kind: ReviewKind::Review,
                engine_kind: EngineKind::Codex,
                status,
                created_at_epoch: 0,
                comment_url: None,
            },
        )
        .expect("seed review_session");
    }

    #[test]
    fn replayed_review_suppressed_when_prior_session_done() {
        let db = Database::open_in_memory().expect("open");
        seed_session(&db, "t-done", SessionStatus::Done, "turn-1");
        assert!(
            replayed_review_should_suppress(&db, "t-done").expect("resolve"),
            "a Done prior review completed (its pm: comment posted) → suppress the replay"
        );
    }

    #[test]
    fn replayed_review_suppressed_when_failed_with_a_turn() {
        let db = Database::open_in_memory().expect("open");
        seed_session(&db, "t-fail-turn", SessionStatus::Failed, "turn-1");
        assert!(
            replayed_review_should_suppress(&db, "t-fail-turn").expect("resolve"),
            "a Failed session whose turn ran MAY have posted → suppress (no duplicate comment)"
        );
    }

    #[test]
    fn replayed_review_reruns_when_failed_before_any_turn() {
        let db = Database::open_in_memory().expect("open");
        // Empty turn_id ⇒ the turn never started (turn_id is set at set_running, before the codex
        // skill posts), so no comment could exist → safe at-least-once rerun.
        seed_session(&db, "t-fail-noturn", SessionStatus::Failed, "");
        assert!(
            !replayed_review_should_suppress(&db, "t-fail-noturn").expect("resolve"),
            "a Failed session that never ran a turn never posted → rerun (preserve at-least-once)"
        );
    }

    #[test]
    fn replayed_review_suppressed_when_session_missing() {
        let db = Database::open_in_memory().expect("open");
        // No row (pruned / never persisted): a claim carrying a thread_id means a start happened, so
        // bias to suppress rather than risk a duplicate comment.
        assert!(
            replayed_review_should_suppress(&db, "t-absent").expect("resolve"),
            "a missing session row conservatively suppresses"
        );
    }

    // F4 (AB#1204): a still-live status (`Starting` / `Interrupting`) found HERE is unexpected on
    // the normal path — `lib.rs`'s startup `fail_orphaned_sessions` reconciles every non-terminal
    // session to `Failed` BEFORE the outbox worker re-enters `start_for_outbox` (the load-bearing
    // ORDERING in `lib.rs`). Since `Failed`-with-empty-turn is the ONLY proof-of-no-comment case,
    // these statuses fall through to the conservative SUPPRESS branch (a possible already-posted
    // comment outweighs a missed retry). These pin that the non-`Failed` arms suppress.
    #[test]
    fn replayed_review_suppressed_when_session_starting() {
        let db = Database::open_in_memory().expect("open");
        // Defensive: even if the orphan flip did not (somehow) reconcile this, a `Starting` row is
        // not proof of "no comment" → suppress.
        seed_session(&db, "t-starting", SessionStatus::Starting, "");
        assert!(
            replayed_review_should_suppress(&db, "t-starting").expect("resolve"),
            "a Starting session (normally orphan-reconciled before this) conservatively suppresses"
        );
    }

    #[test]
    fn replayed_review_suppressed_when_session_interrupting() {
        let db = Database::open_in_memory().expect("open");
        seed_session(&db, "t-interrupting", SessionStatus::Interrupting, "turn-1");
        assert!(
            replayed_review_should_suppress(&db, "t-interrupting").expect("resolve"),
            "an Interrupting session (normally orphan-reconciled before this) conservatively suppresses"
        );
    }
}
