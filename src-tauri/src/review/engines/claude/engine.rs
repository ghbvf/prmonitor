//! [`ReviewEngine`] implementation backed by a one-shot `claude -p` subprocess
//! (#718) — the second engine, selectable per-project alongside codex.
//!
//! [`ClaudeEngine`] is a thin adapter bundling the per-call context; the spawn +
//! parse + pump + registry-driving orchestration lives in this module (self-contained,
//! mirroring how codex's lives in `review::session`, but for a per-review subprocess
//! rather than a resident RPC connection). The dedup registry, history persistence,
//! and [`ReviewEvent`] wire contract are REUSED, not duplicated.

use tauri::Manager;
use tokio::io::{AsyncBufRead, BufReader};
use tokio::process::Child;

use super::manager::ClaudeManager;
use super::process::{self, ParsedEvent, ParserState};
use crate::config::service::ResolvedCli;
use crate::error::{AppError, AppResult};
use crate::events::{ReviewEvent, StreamEvent};
use crate::model::{EngineKind, ReviewKind};
use crate::review::engine::{ReviewEngine, ReviewStartCapability, SessionId, StartReviewOutcome};
use crate::review::history_store::{self, HistoryItemKind};
use crate::review::session::{
    commit_starting_session, CommentUrlContext, SessionInfo, SessionRegistry, SessionStatus,
};

/// Per-request engine handle. Borrows the long-lived state from `AppState` plus the
/// request's `AppHandle`; constructed fresh by each command/dispatch (cheap — all
/// borrows). `start`/`stop` run inline while the streaming pump it spawns outlives it
/// (the pump owns clones, not the engine). No skill path field — `claude` auto-discovers
/// `.claude/skills/` from the turn cwd (`repo_root`).
pub struct ClaudeEngine<'a, R: tauri::Runtime> {
    pub app: &'a tauri::AppHandle<R>,
    pub claude: &'a ClaudeManager,
    pub registry: &'a SessionRegistry,
    /// Typed claude launch credential resolved by the config slice.
    pub claude_cli: &'a ResolvedCli,
    /// Owning project id (#35): scopes the registry reservation / dedup and stamps
    /// every streamed `ReviewEvent` so the frontend routes it to the right project.
    pub project_id: &'a str,
    /// Monitored repo `owner/name` (carried for parity with codex; unused by the
    /// prompt since `claude` discovers the PR via the skill + cwd).
    pub repo: &'a str,
    /// Absolute local clone path `claude -p` runs in (the cwd; skills resolve from here).
    pub repo_root: &'a str,
    /// Hand-typed claude model name (empty = claude CLI default). Passed as `--model`
    /// to the `claude -p` subprocess when non-blank.
    pub claude_model: &'a str,
    /// IMMUTABLE comment-URL source context (AB#1042), built from the project at dispatch.
    /// Owned so it moves into `start_review` → the `Starting` session, pinning the terminal
    /// `finalize_turn`'s URL resolve to the project the review ran against (not a mid-review
    /// config edit). `pub(crate)`: crate-internal context type, in-crate constructors only.
    pub(crate) url_ctx: CommentUrlContext,
    /// The PR number for the FOLLOW-UP (`send_message`) path only — the `ReviewEngine`
    /// trait's `send_message(session, message, user_item_id)` carries no `pr_number`, so the
    /// command resolves it and sets it here. The `start`/`stop` paths take `pr_number` as a
    /// method arg and ignore this field (set to 0 at those construction sites).
    pub pr_number: u64,
    /// Full persisted session identity for the FOLLOW-UP path. It pins the creating engine,
    /// original kind, timestamp, and URL metadata across app restarts/config edits.
    pub session_info: Option<SessionInfo>,
    /// The owning outbox row id for the AB#1204 cross-restart dedup claim — `Some(outbox_id)` ONLY
    /// on the OUTBOX executor's start path ([`crate::review::commands::start_for_outbox`]), `None`
    /// on the manual / follow-up / auto-dispatch paths. When `Some`, `start` writes the claim's
    /// `thread_id` (== claude `session_id`) breadcrumb INSIDE `start_review` — right after the
    /// `system/init` line yields a stable `session_id`. The session row and claim breadcrumb commit
    /// atomically BEFORE registry promotion; linkage failure drops the kill-on-drop child and keeps
    /// the pair retryable (mirrors the codex path's F1 placement).
    pub outbox_claim_id: Option<i64>,
}

impl<R: tauri::Runtime> ReviewEngine for ClaudeEngine<'_, R> {
    async fn start(
        &self,
        _capability: &ReviewStartCapability,
        pr_number: u64,
        kind: ReviewKind,
    ) -> AppResult<StartReviewOutcome> {
        start_review(
            self.app,
            self.claude,
            self.registry,
            self.claude_cli,
            self.project_id,
            self.repo_root,
            self.claude_model,
            pr_number,
            kind,
            // `&self` start can't move the field; clone the owned context for this turn.
            self.url_ctx.clone(),
            // AB#1204: outbox path passes `Some(outbox_id)` so the claim breadcrumb is written
            // right after the session id is known (before the turn runs); other paths pass `None`.
            self.outbox_claim_id,
        )
        .await
    }

    async fn send_message(
        &self,
        session: &SessionId,
        message: &str,
        user_item_id: &str,
    ) -> AppResult<()> {
        resume_review(
            self.app,
            self.claude,
            self.registry,
            self.claude_cli,
            self.project_id,
            self.repo_root,
            self.claude_model,
            self.pr_number,
            self.session_info
                .as_ref()
                .expect("ClaudeEngine::send_message requires session_info"),
            session,
            message,
            user_item_id,
            // `&self` can't move the field; clone the owned context for this follow-up turn.
            self.url_ctx.clone(),
        )
        .await
    }
}

/// Start a one-shot `claude -p` review for `pr_number`, streaming its `stream-json`
/// output as [`ReviewEvent`]s. Returns [`StartReviewOutcome::Started`] with the claude
/// `session_id`, or [`StartReviewOutcome::Deduped`] when the `(project_id, pr, kind)` is
/// already covered by an in-flight (or reserved) review.
///
/// Flow (mirrors codex's two-phase start, adapted to a subprocess): reserve → spawn →
/// read the `system/init` line for the session id → `promote_reservation` (Starting) →
/// `set_running` → register the cancel channel (BEFORE the pump spawns, so a `stop` can
/// never race ahead of registration) → spawn the pump (which owns the child + reader +
/// cancel receiver and parses to the terminal `result` or a cancel). Any failure before
/// the pump spawns releases the reservation (RAII guard) and returns `Err`.
#[allow(clippy::too_many_arguments)]
async fn start_review<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    claude: &ClaudeManager,
    registry: &SessionRegistry,
    claude_cli: &ResolvedCli,
    project_id: &str,
    repo_root: &str,
    claude_model: &str,
    pr_number: u64,
    kind: ReviewKind,
    // IMMUTABLE comment-URL source context (AB#1042); handed to the `Starting` session in
    // `promote_reservation` so the terminal `finalize_turn` resolves the URL against the
    // project this review ran against (mirrors the codex path).
    url_ctx: CommentUrlContext,
    // AB#1204 outbox claim id: `Some(outbox_id)` ONLY on the outbox executor's start path, `None`
    // otherwise. When `Some`, the claim's `thread_id` (== `session_id`) breadcrumb is written right
    // after the `system/init` line yields the session id (below) — before the turn posts a comment.
    outbox_claim_id: Option<i64>,
) -> AppResult<StartReviewOutcome> {
    // Atomic test-and-set BEFORE spawning: if this `(project_id, pr, kind)` is already
    // reserved or covered by an in-flight session, do NOT start a second review (the
    // SAME idempotency boundary the codex path uses — reused, not re-implemented).
    if !registry.try_reserve_pair(project_id, pr_number, kind) {
        return Ok(StartReviewOutcome::Deduped);
    }
    // From here any early return releases the reservation via the guard's Drop;
    // `disarm()` on success hands it to the inserted session instead.
    let reservation = ReservationGuard {
        registry,
        project_id: project_id.to_string(),
        pr_number,
        kind,
        armed: true,
    };

    // Spawn the one-shot child. `?` releases the reservation (guard Drop) on failure.
    // `None` resume → a FRESH review (no `--resume`); the follow-up path is `resume_review`.
    let prompt = process::review_prompt(pr_number, kind);
    let proc = process::spawn_claude(claude_cli, repo_root, claude_model, &prompt, None)?;
    let process::ClaudeProcess {
        child,
        stdout,
        stderr,
    } = proc;

    // Drain stderr concurrently so a full pipe can't block the child.
    tauri::async_runtime::spawn(process::drain_stderr(BufReader::new(stderr)));

    // Read stdout up to the `system/init` line to learn the session id. We carry the
    // reader + parser state forward into the pump so no line is dropped.
    let mut reader = process::stdout_reader(stdout);
    let mut parser = ParserState::default();
    let session_id = match read_session_id(&mut reader, &mut parser).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            // EOF before any init line → claude exited without starting (not logged in,
            // not installed correctly, bad flags). Surface as an error; the reservation
            // releases via the guard.
            return Err(AppError::new(
                "claude 未输出会话 init（未登录或启动失败？）".to_string(),
            ));
        }
        Err(e) => return Err(e),
    };

    // Hand the reservation to a `Starting` session in ONE critical section (no gap),
    // exactly like codex. `turn_id == session_id`: claude has no separate turn concept;
    // the field is codex-interrupt-only, so we mirror the session id into it.
    let created_at_epoch = history_store::now_epoch();
    let starting = SessionInfo {
        project_id: project_id.to_string(),
        thread_id: session_id.clone(),
        turn_id: session_id.clone(),
        pr_number,
        kind,
        engine_kind: EngineKind::Claude,
        status: SessionStatus::Starting,
        created_at_epoch,
        // No comment yet — filled by `session::finalize_turn` at a `completed` terminal (AB#1042).
        comment_url: None,
    };
    // The subprocess is configured with `kill_on_drop`; if durable linkage fails here, returning
    // drops `child` before it is handed to the pump, while the still-armed reservation guard makes
    // the pair retryable. A successful transaction is promoted atomically into the registry.
    commit_starting_session(app, registry, starting.clone(), url_ctx, outbox_claim_id)?;
    reservation.disarm();

    // Flip to Running (the child is live and streaming). turn_id stays the session id.
    registry.set_running(&session_id, session_id.clone());
    let _ = persist_session(
        app,
        &SessionInfo {
            status: SessionStatus::Running,
            ..starting
        },
    );

    // Register the cancel channel BEFORE spawning the pump (closes the
    // register-after-spawn race: a `stop` landing between spawn and register would
    // otherwise miss the review). `stop` SIGNALS this channel; the pump observes it and
    // converges through `finish` (emitting the terminal event), rather than being aborted
    // mid-flight and leaving the session stuck `Running`.
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    claude.register(session_id.clone(), cancel_tx);

    // Spawn the pump (owns the child + reader + parser + cancel receiver). Capture
    // `project_id` as owned so every event is stamped without a per-event registry lookup
    // (#35), mirroring the codex pump.
    tauri::async_runtime::spawn(pump(
        reader,
        child,
        parser,
        cancel_rx,
        project_id.to_string(),
        pr_number,
        session_id.clone(),
        app.clone(),
        registry.clone(),
        claude.clone(),
    ));

    Ok(StartReviewOutcome::Started(session_id))
}

/// Continue an EXISTING claude session with a follow-up user `message` (chat continuation):
/// spawn `claude -p --resume <thread_id>` with the RAW user message as the prompt, and
/// stream the reply through the existing [`pump`] — reusing the whole `ReviewEvent`
/// pipeline. Sibling of [`start_review`]. `--resume` is cross-restart capable: claude
/// persists transcripts on disk, so a follow-up works even after an app restart (unlike
/// codex, whose thread lives only in the resident process).
///
/// Differences from `start_review`:
/// (a) the prompt is the RAW user `message`, NOT `/pr-review N`;
/// (b) spawned with `Some(thread_id)` so `--resume <thread_id>` is appended;
/// (c) the `claude -p --resume` `system/init` line emits a NEW session id which we
///     read-and-DISCARD — we keep using the ORIGINAL `thread_id` for the pump's session
///     stamping, history append, and registry keying (adopting the new id would orphan
///     history/dedup/stop-routing);
/// (d) a fresh cancel channel is registered (`claude.register(thread_id, …)`) so the
///     follow-up turn is stoppable;
/// (e) the user message is persisted (under `user_item_id`) BEFORE the spawn;
/// (f) the existing `pump` is spawned with the ORIGINAL `thread_id`.
#[allow(clippy::too_many_arguments)]
async fn resume_review<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    claude: &ClaudeManager,
    registry: &SessionRegistry,
    claude_cli: &ResolvedCli,
    project_id: &str,
    repo_root: &str,
    claude_model: &str,
    pr_number: u64,
    durable_info: &SessionInfo,
    thread_id: &str,
    message: &str,
    user_item_id: &str,
    url_ctx: CommentUrlContext,
) -> AppResult<()> {
    // Rehydrate a durable terminal row if the session isn't live (registry empty after a
    // restart). The first turn's `finalize_turn` consumed the original URL context, so a
    // fresh one is re-inserted for this follow-up turn's terminal resolve.
    if registry.get(thread_id).is_none() {
        registry.rehydrate(durable_info.clone(), url_ctx.clone());
    }

    // Atomic guard: only a TERMINAL session flips to `Running` and proceeds. A turn in
    // flight (`Busy`) or a genuinely absent session is rejected before any spawn.
    match registry.begin_resume(thread_id) {
        crate::review::session::BeginResume::Proceed => {}
        crate::review::session::BeginResume::Busy => {
            return Err(AppError::new(format!(
                "review 会话仍在进行中，无法续聊: {thread_id}"
            )))
        }
        crate::review::session::BeginResume::NotFound => {
            return Err(AppError::new(format!("未找到 review 会话: {thread_id}")))
        }
    }

    // Register a FRESH cancel channel before any potentially blocking spawn/init work. Unlike
    // initial review, resume already knows the stable `thread_id`, so stop can be effective
    // while `claude -p --resume` starts or emits its init line.
    let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
    claude.register(thread_id.to_string(), cancel_tx);

    // Spawn restricted `claude -p --resume <thread_id>` and send the RAW user message through
    // stdin, not argv. On a spawn/write failure flip back to `Failed` (the session was set
    // `Running` by `begin_resume`) so it isn't stuck, then surface the error.
    let proc = match process::spawn_claude_stdin_chat(
        claude_cli,
        repo_root,
        claude_model,
        message,
        thread_id,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            registry.set_status(thread_id, SessionStatus::Failed);
            persist_session_status(
                app,
                registry,
                project_id,
                thread_id,
                pr_number,
                durable_info,
                SessionStatus::Failed,
            );
            // No `finalize_turn` runs on a failure path, so it never consumes the URL context
            // the (possibly just-)`rehydrate`d session inserted — discard it so it doesn't leak
            // in `url_contexts` (no-op `None` when this run never rehydrated).
            let _ = registry.take_url_context(thread_id);
            claude.deregister(thread_id);
            return Err(e);
        }
    };
    let process::ClaudeProcess {
        mut child,
        stdout,
        stderr,
    } = proc;

    tauri::async_runtime::spawn(process::drain_stderr(BufReader::new(stderr)));

    // Read to the `system/init` line of the RESUMED run. `claude -p --resume` emits a NEW
    // session id on this line, which we DISCARD: we keep the ORIGINAL `thread_id` for the
    // pump's session stamping, history append, and registry keying. Adopting the new id
    // would orphan this session's history / dedup / stop-routing (all keyed on the original).
    let mut reader = process::stdout_reader(stdout);
    let mut parser = ParserState::default();
    tokio::select! {
        changed = cancel_rx.changed() => {
            let _ = changed;
            let _ = child.start_kill();
            finish(
                registry,
                claude,
                app,
                project_id,
                pr_number,
                thread_id,
                SessionStatus::Done,
                "interrupted",
                None,
            )
            .await;
            let _ = child.wait().await;
            return Ok(());
        }
        init = read_session_id(&mut reader, &mut parser) => {
            match init {
                // The new id is intentionally unused — we stamp everything with the original.
                Ok(Some(_new_session_id)) => {}
                Ok(None) => {
                    registry.set_status(thread_id, SessionStatus::Failed);
                    persist_session_status(
                        app,
                        registry,
                        project_id,
                        thread_id,
                        pr_number,
                        durable_info,
                        SessionStatus::Failed,
                    );
                    let _ = registry.take_url_context(thread_id);
                    claude.deregister(thread_id);
                    return Err(AppError::new(
                        "claude 未输出会话 init（续聊失败：transcript 不存在或未登录？）".to_string(),
                    ));
                }
                Err(e) => {
                    registry.set_status(thread_id, SessionStatus::Failed);
                    persist_session_status(
                        app,
                        registry,
                        project_id,
                        thread_id,
                        pr_number,
                        durable_info,
                        SessionStatus::Failed,
                    );
                    let _ = registry.take_url_context(thread_id);
                    claude.deregister(thread_id);
                    return Err(e);
                }
            }
        }
    }

    // Persist the user's typed message to history BEFORE the pump streams the reply, under
    // the CALLER-supplied `user_item_id` (so the optimistic bubble id == the persisted id,
    // and reopen-dedup works). History is ordered by rowid (`ORDER BY h.id`), so inserting
    // this row first places the user message before the reply. Reuses the shared
    // `session::persist_user_message` helper (one source for both engines).
    crate::review::session::persist_user_message(app, project_id, thread_id, user_item_id, message);

    // The session is already `Running` (set by `begin_resume`); mirror it durably.
    persist_session_status(
        app,
        registry,
        project_id,
        thread_id,
        pr_number,
        durable_info,
        SessionStatus::Running,
    );

    // Spawn the SAME pump as `start_review`, stamped with the ORIGINAL `thread_id`.
    tauri::async_runtime::spawn(pump(
        reader,
        child,
        parser,
        cancel_rx,
        project_id.to_string(),
        pr_number,
        thread_id.to_string(),
        app.clone(),
        registry.clone(),
        claude.clone(),
    ));

    Ok(())
}

/// Best-effort durable mirror of a session STATUS transition on the follow-up path, where
/// only the thread id (not a full live `SessionInfo`) is at hand. Reads the current
/// in-memory row (via `registry`) to preserve its `pr_number`/`kind`/`created_at_epoch`,
/// falling back to the resolved `pr_number` + empty kind if the row is absent. Logs +
/// swallows like the other claude `persist_*` helpers (the in-memory registry stays the
/// dedup/status authority). `upsert_session` keys `created_at` on first insert (ON CONFLICT
/// preserves it), so a re-stamped `created_at_epoch` in the fallback is harmless for an
/// already-persisted row.
fn persist_session_status<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    registry: &SessionRegistry,
    project_id: &str,
    thread_id: &str,
    pr_number: u64,
    durable_info: &SessionInfo,
    status: SessionStatus,
) {
    let info = registry
        .get(thread_id)
        .map(|info| SessionInfo { status, ..info })
        .unwrap_or_else(|| SessionInfo {
            project_id: project_id.to_string(),
            thread_id: thread_id.to_string(),
            turn_id: thread_id.to_string(),
            pr_number,
            kind: durable_info.kind,
            engine_kind: durable_info.engine_kind,
            status,
            created_at_epoch: durable_info.created_at_epoch,
            comment_url: durable_info.comment_url.clone(),
        });
    let _ = persist_session(app, &info);
}

/// Read stdout until the first `system/init` line, returning its session id (or `None`
/// at EOF before any init line). Non-init lines before init (rare) are parsed-and-dropped
/// — they carry no session yet, so they can't be emitted (no thread id), and the pump
/// re-reads nothing (we consumed them here). The `parser` carries forward so the pump
/// continues with the same message-id state.
async fn read_session_id<Rd: AsyncBufRead + Unpin>(
    reader: &mut Rd,
    parser: &mut ParserState,
) -> AppResult<Option<String>> {
    loop {
        let line = process::read_line(reader)
            .await
            .map_err(|e| AppError::new(format!("读取 claude 输出失败: {e}")))?;
        let Some(line) = line else { return Ok(None) };
        // The session id is unknown until init, so use an empty fallback for item ids
        // here; in practice init is the FIRST line, so no delta precedes it.
        if let Some(ParsedEvent::SessionStarted { session_id }) =
            process::parse_line(&line, parser, "")
        {
            return Ok(Some(session_id));
        }
    }
}

/// The pump: owns the child + stdout reader + cancel receiver, parses each remaining
/// `stream-json` line to a [`ReviewEvent`], emits it to the frontend, and persists deltas
/// best-effort — until the terminal `result`, EOF, a stream error, OR a cancel signal.
/// EVERY exit path runs [`finish`] (so the session always gets a terminal `TurnCompleted`
/// and a terminal status — never left `Running`), then reaps the child. Mirrors the codex
/// pump's emit-then-persist ordering and best-effort persistence.
#[allow(clippy::too_many_arguments)]
async fn pump<R: tauri::Runtime>(
    mut reader: BufReader<tokio::process::ChildStdout>,
    mut child: Child,
    mut parser: ParserState,
    mut cancel: tokio::sync::watch::Receiver<bool>,
    project_id: String,
    pr_number: u64,
    session_id: String,
    app: tauri::AppHandle<R>,
    registry: SessionRegistry,
    claude: ClaudeManager,
) {
    loop {
        tokio::select! {
            // A cancel was signalled (`Ok`) by `stop`/`shutdown`, or the sender was
            // dropped (`Err`) — either way, end the review. SIGKILL the child promptly
            // (the trailing `wait()` below reaps it), then `finish` as a clean stop:
            // `Done` is terminal (so dedup releases) while the wire status shows
            // "interrupted" (a user stop, mirroring codex's interrupted turn).
            changed = cancel.changed() => {
                let _ = changed;
                let _ = child.start_kill();
                finish(
                    &registry,
                    &claude,
                    &app,
                    &project_id,
                    pr_number,
                    &session_id,
                    SessionStatus::Done,
                    "interrupted",
                    None,
                )
                .await;
                break;
            }
            read = process::read_line(&mut reader) => {
                let line = match read {
                    Ok(Some(line)) => line,
                    // EOF before a terminal `result`: the child closed stdout without a
                    // result (killed, crashed, or finished abnormally). End as Failed.
                    Ok(None) => {
                        finish(
                            &registry,
                            &claude,
                            &app,
                            &project_id,
                            pr_number,
                            &session_id,
                            SessionStatus::Failed,
                            "failed",
                            Some("claude 进程结束但未返回结果".to_string()),
                        )
                        .await;
                        break;
                    }
                    Err(e) => {
                        finish(
                            &registry,
                            &claude,
                            &app,
                            &project_id,
                            pr_number,
                            &session_id,
                            SessionStatus::Failed,
                            "failed",
                            Some(format!("读取 claude 输出失败: {e}")),
                        )
                        .await;
                        break;
                    }
                };

                let Some(parsed) = process::parse_line(&line, &mut parser, &session_id) else {
                    continue;
                };
                match parsed {
                    // A second init (shouldn't happen post-start) carries no UI content → skip.
                    ParsedEvent::SessionStarted { .. } => continue,
                    ParsedEvent::MessageDelta { item_id, text } => {
                        emit_and_persist(
                            &app,
                            &project_id,
                            &session_id,
                            &item_id,
                            HistoryItemKind::Message,
                            text,
                        );
                    }
                    ParsedEvent::ReasoningDelta { item_id, text } => {
                        emit_and_persist(
                            &app,
                            &project_id,
                            &session_id,
                            &item_id,
                            HistoryItemKind::Reasoning,
                            text,
                        );
                    }
                    // Terminal: map to a `completed`/`failed` TurnCompleted (emitting an
                    // Error first when the run errored), set the terminal status, deregister.
                    ParsedEvent::Result { is_error, message } => {
                        let error = is_error.then(|| {
                            if message.is_empty() {
                                "claude review 失败".to_string()
                            } else {
                                message
                            }
                        });
                        let (status, wire_status) = if is_error {
                            (SessionStatus::Failed, "failed")
                        } else {
                            (SessionStatus::Done, "completed")
                        };
                        finish(
                            &registry,
                            &claude,
                            &app,
                            &project_id,
                            pr_number,
                            &session_id,
                            status,
                            wire_status,
                            error,
                        )
                        .await;
                        break;
                    }
                }
            }
        }
    }

    // Reap the child so it can't linger as a zombie: on the result/EOF paths it has
    // already exited; on the cancel path we just `start_kill`ed it, so this `wait` reaps
    // the SIGKILLed child. (`kill_on_drop(true)` is the backstop if this future is dropped
    // before reaching here.)
    let _ = child.wait().await;
}

/// Emit a delta [`ReviewEvent`] to the frontend FIRST (streaming latency must not wait on
/// the DB), then persist it to the session history best-effort. A persist error is logged,
/// never breaks the live stream — same contract as the codex pump.
fn emit_and_persist<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: &str,
    session_id: &str,
    item_id: &str,
    kind: HistoryItemKind,
    text: String,
) {
    let event = match kind {
        HistoryItemKind::Message => ReviewEvent::MessageDelta {
            project_id: project_id.to_string(),
            thread_id: session_id.to_string(),
            item_id: item_id.to_string(),
            text: text.clone(),
        },
        HistoryItemKind::Reasoning => ReviewEvent::ReasoningDelta {
            project_id: project_id.to_string(),
            thread_id: session_id.to_string(),
            item_id: item_id.to_string(),
            text: text.clone(),
        },
        // `User` is a stored-only kind (a follow-up message the user typed) — it is persisted
        // directly by `session::persist_user_message`, NEVER streamed through this delta path.
        // The pump only ever calls this with the two delta kinds above; this arm is
        // unreachable by construction. `debug_assert!(false)` trips a future regression that
        // starts streaming a `User` kind in dev/test, while staying a no-op `return` in release.
        HistoryItemKind::User => {
            debug_assert!(
                false,
                "User kind must not reach emit_and_persist (persisted directly by persist_user_message)"
            );
            return;
        }
    };
    crate::stream::emit(app, StreamEvent::Review(event));
    let db = app.state::<crate::db::Database>();
    if let Err(e) = history_store::append_item(db.inner(), session_id, item_id, kind, &text) {
        eprintln!(
            "claude review history 持久化失败（{session_id}/{item_id}）：{}",
            e.message
        );
        // Symmetry with codex (review F9): the first silent persist failure raises one
        // app-level notice; later failures only log (the shared process-global guard).
        crate::review::session::notify_persist_failure_once(app, project_id);
    }
}

/// Finish a session by routing its terminal through the SHARED engine-agnostic funnel
/// [`crate::review::session::finalize_turn`] (AB#1042) — so the claude exits land the
/// terminal status + resolved comment URL, emit the optional `Error` + terminal
/// `TurnCompleted`, and signal the completion watch in the SAME order as the codex pump (one
/// source for the terminal sequence). The terminal in-memory `status` + `wire_status` are
/// passed EXPLICITLY per exit path (the caller knows the right pairing): result-success →
/// `(Done, "completed")` (the only path that resolves a comment URL), result-error / EOF /
/// read-error → `(Failed, "failed")`, cancel → `(Done, "interrupted")` — `Done` so dedup
/// releases, wire "interrupted" so the UI shows a user stop (mirrors codex).
///
/// `claude.deregister(session_id)` stays OUTSIDE the funnel (called here after it), since the
/// funnel is engine-agnostic and takes no `ClaudeManager` — the cancel-sender cleanup is
/// claude-specific. `async` because the funnel resolves the comment URL via a subprocess.
#[allow(clippy::too_many_arguments)]
async fn finish<R: tauri::Runtime>(
    registry: &SessionRegistry,
    claude: &ClaudeManager,
    app: &tauri::AppHandle<R>,
    project_id: &str,
    pr_number: u64,
    session_id: &str,
    status: SessionStatus,
    wire_status: &str,
    error: Option<String>,
) {
    // The shared funnel does: resolve URL (completed only) → set status + URL (registry +
    // DB) → emit Error?+TurnCompleted → signal completion LAST. claude's `session_id` is the
    // funnel's `thread_id`.
    crate::review::session::finalize_turn(
        app,
        registry,
        project_id,
        pr_number,
        session_id,
        status,
        wire_status,
        error,
    )
    .await;
    // Deregister WITHOUT signal (the pump is finishing on its own; the funnel already
    // signalled completion); a later `stop` for this id then finds nothing and returns false
    // (correct: nothing to stop). OUTSIDE the funnel — engine-specific cleanup.
    claude.deregister(session_id);
}

/// Best-effort mirror of an in-memory [`SessionInfo`] into the durable `review_session`
/// table. Logs + swallows errors so a persistence hiccup never breaks the live session —
/// the in-memory registry stays the authority for dedup / status (same contract as codex).
fn persist_session<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    info: &SessionInfo,
) -> AppResult<()> {
    let db = app.state::<crate::db::Database>();
    if let Err(e) = history_store::upsert_session(db.inner(), info) {
        eprintln!(
            "claude review session 持久化失败（{}）：{}",
            info.thread_id, e.message
        );
        // Symmetry with codex (review F9): the first silent persist failure raises one
        // app-level notice; later failures only log (the shared process-global guard).
        crate::review::session::notify_persist_failure_once(app, &info.project_id);
        return Err(e);
    }
    Ok(())
}

/// RAII release of a `(pr, kind)` reservation taken by `try_reserve_pair`. An undisarmed
/// guard releases on drop, so NO early `?` / error between the reserve and the session
/// insert can leak a reservation — leak-on-failure is unrepresentable, not hand-avoided
/// (the same discipline codex's `ReservationGuard` enforces; this one is claude-local).
struct ReservationGuard<'a> {
    registry: &'a SessionRegistry,
    project_id: String,
    pr_number: u64,
    kind: ReviewKind,
    armed: bool,
}

impl ReservationGuard<'_> {
    /// The reservation has been handed to a `Starting` session — stop owning it.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for ReservationGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.registry
                .release_pair(&self.project_id, self.pr_number, self.kind);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `read_session_id` returns the id from the first init line and consumes only up to
    /// it, leaving the rest of the stream for the pump. (Pure over an in-memory reader — no
    /// subprocess.)
    #[tokio::test]
    async fn read_session_id_captures_init_then_stops() {
        let data = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sid-7\"}\n",
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"message_start\",\"message\":{\"id\":\"m1\"}}}\n",
        );
        let mut reader = tokio::io::BufReader::new(data.as_bytes());
        let mut parser = ParserState::default();
        let id = read_session_id(&mut reader, &mut parser).await.expect("ok");
        assert_eq!(id, Some("sid-7".to_string()));
        // The next line (message_start) is still unread → the pump would consume it.
        let next = process::read_line(&mut reader).await.unwrap();
        assert!(next.unwrap().contains("message_start"));
    }

    /// A non-init line (e.g. `system/status`) BEFORE the init line is parsed-and-dropped;
    /// `read_session_id` keeps reading to the init line, returns its session id, and leaves
    /// the following line for the pump. (Defends the "init is not necessarily the very
    /// first line" path.)
    #[tokio::test]
    async fn read_session_id_skips_leading_status_then_captures_init() {
        let data = concat!(
            "{\"type\":\"system\",\"subtype\":\"status\",\"message\":\"warming up\"}\n",
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sid-9\"}\n",
            "{\"type\":\"stream_event\",\"event\":{\"type\":\"message_start\",\"message\":{\"id\":\"m1\"}}}\n",
        );
        let mut reader = tokio::io::BufReader::new(data.as_bytes());
        let mut parser = ParserState::default();
        let id = read_session_id(&mut reader, &mut parser).await.expect("ok");
        assert_eq!(id, Some("sid-9".to_string()));
        // The init line was consumed; the NEXT unread line is the message_start (pump's).
        let next = process::read_line(&mut reader).await.unwrap();
        assert!(next.unwrap().contains("message_start"));
    }

    /// EOF before any init line → `None` (the engine maps this to an error + releases
    /// the reservation).
    #[tokio::test]
    async fn read_session_id_eof_before_init_is_none() {
        let mut reader = tokio::io::BufReader::new(&b""[..]);
        let mut parser = ParserState::default();
        assert_eq!(
            read_session_id(&mut reader, &mut parser).await.expect("ok"),
            None
        );
    }
}
