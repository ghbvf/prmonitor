//! [`ReviewEngine`] implementation backed by a one-shot `claude -p` subprocess
//! (#718) — the second engine, selectable per-project alongside codex.
//!
//! [`ClaudeEngine`] is a thin adapter bundling the per-call context; the spawn +
//! parse + pump + registry-driving orchestration lives in this module (self-contained,
//! mirroring how codex's lives in `review::session`, but for a per-review subprocess
//! rather than a resident RPC connection). The dedup registry, history persistence,
//! and [`ReviewEvent`] wire contract are REUSED, not duplicated.

use tauri::{Emitter, Manager};
use tokio::io::{AsyncBufRead, BufReader};
use tokio::process::Child;

use super::manager::ClaudeManager;
use super::process::{self, ParsedEvent, ParserState};
use crate::error::{AppError, AppResult};
use crate::events::{ReviewEvent, REVIEW_EVENT};
use crate::review::engine::{ReviewEngine, SessionId, StartReviewOutcome};
use crate::review::history_store::{self, HistoryItemKind};
use crate::review::session::{SessionInfo, SessionRegistry, SessionStatus};

/// Per-request engine handle. Borrows the long-lived state from `AppState` plus the
/// request's `AppHandle`; constructed fresh by each command/dispatch (cheap — all
/// borrows). `start`/`stop` run inline while the streaming pump it spawns outlives it
/// (the pump owns clones, not the engine). No skill path field — `claude` auto-discovers
/// `.claude/skills/` from the turn cwd (`repo_root`).
pub struct ClaudeEngine<'a, R: tauri::Runtime> {
    pub app: &'a tauri::AppHandle<R>,
    pub claude: &'a ClaudeManager,
    pub registry: &'a SessionRegistry,
    /// The claude binary name (PATH-resolved; mirrors codex's `codex_bin`).
    pub claude_bin: &'a str,
    /// Owning project id (#35): scopes the registry reservation / dedup and stamps
    /// every streamed `ReviewEvent` so the frontend routes it to the right project.
    pub project_id: &'a str,
    /// Monitored repo `owner/name` (carried for parity with codex; unused by the
    /// prompt since `claude` discovers the PR via the skill + cwd).
    pub repo: &'a str,
    /// Absolute local clone path `claude -p` runs in (the cwd; skills resolve from here).
    pub repo_root: &'a str,
}

impl<R: tauri::Runtime> ReviewEngine for ClaudeEngine<'_, R> {
    async fn start(&self, pr_number: u64, kind: &str) -> AppResult<StartReviewOutcome> {
        start_review(
            self.app,
            self.claude,
            self.registry,
            self.claude_bin,
            self.project_id,
            self.repo_root,
            pr_number,
            kind,
        )
        .await
    }

    async fn stop(&self, session: &SessionId) -> AppResult<()> {
        // The manager owns the in-flight session's kill handle. Aborting the pump task
        // drops the `kill_on_drop` child (SIGKILLs `claude -p`). A `false` (unknown id)
        // is benign here — the command-level `stop_review` is the funnel that decides
        // claude-vs-codex; an engine-level stop against a gone session is a no-op.
        self.claude.stop(session);
        Ok(())
    }
}

/// Start a one-shot `claude -p` review for `pr_number`, streaming its `stream-json`
/// output as [`ReviewEvent`]s. Returns [`StartReviewOutcome::Started`] with the claude
/// `session_id`, or [`StartReviewOutcome::Deduped`] when the `(project_id, pr, kind)` is
/// already covered by an in-flight (or reserved) review.
///
/// Flow (mirrors codex's two-phase start, adapted to a subprocess): reserve → spawn →
/// read the `system/init` line for the session id → `promote_reservation` (Starting) →
/// register the pump's abort handle → `set_running` → spawn the pump (which owns the
/// child + reader and continues parsing to the terminal `result`). Any failure before
/// the pump spawns releases the reservation (RAII guard) and returns `Err`.
#[allow(clippy::too_many_arguments)]
async fn start_review<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    claude: &ClaudeManager,
    registry: &SessionRegistry,
    claude_bin: &str,
    project_id: &str,
    repo_root: &str,
    pr_number: u64,
    kind: &str,
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
        kind: kind.to_string(),
        armed: true,
    };

    // Spawn the one-shot child. `?` releases the reservation (guard Drop) on failure.
    let prompt = process::review_prompt(pr_number, kind);
    let proc = process::spawn_claude(claude_bin, repo_root, &prompt)?;
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
        kind: kind.to_string(),
        status: SessionStatus::Starting,
        created_at_epoch,
    };
    registry.promote_reservation(starting.clone());
    persist_session(app, &starting);
    reservation.disarm();

    // Flip to Running (the child is live and streaming). turn_id stays the session id.
    registry.set_running(&session_id, session_id.clone());
    persist_session(
        app,
        &SessionInfo {
            status: SessionStatus::Running,
            ..starting
        },
    );

    // Spawn the pump (owns the child + reader + parser). Register its abort handle so
    // `stop`/`shutdown` can kill this review: aborting the pump drops the `kill_on_drop`
    // child. Capture `project_id` as owned so every event is stamped without a per-event
    // registry lookup (#35), mirroring the codex pump. Use `tokio::spawn` (not
    // `tauri::async_runtime::spawn`) because the manager keys on a tokio `AbortHandle`,
    // which tauri's `JoinHandle` does not expose — both run on the same tokio runtime
    // tauri drives, so the task lands identically.
    let pump_handle = tokio::spawn(pump(
        reader,
        child,
        parser,
        project_id.to_string(),
        session_id.clone(),
        app.clone(),
        registry.clone(),
        claude.clone(),
    ));
    claude.register(session_id.clone(), pump_handle.abort_handle());

    Ok(StartReviewOutcome::Started(session_id))
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

/// The pump: owns the child + stdout reader, parses each remaining `stream-json` line to
/// a [`ReviewEvent`], emits it to the frontend, and persists deltas best-effort — until
/// the terminal `result` (or EOF / stream error). On a terminal it sets the session's
/// terminal status, deregisters from the manager, and reaps the child. Mirrors the codex
/// pump's emit-then-persist ordering and best-effort persistence.
#[allow(clippy::too_many_arguments)]
async fn pump<R: tauri::Runtime>(
    mut reader: BufReader<tokio::process::ChildStdout>,
    mut child: Child,
    mut parser: ParserState,
    project_id: String,
    session_id: String,
    app: tauri::AppHandle<R>,
    registry: SessionRegistry,
    claude: ClaudeManager,
) {
    loop {
        let line = match process::read_line(&mut reader).await {
            Ok(Some(line)) => line,
            // EOF before a terminal `result`: the child closed stdout without a result
            // (killed, crashed, or finished abnormally). End the session as Failed.
            Ok(None) => {
                finish(
                    &registry,
                    &claude,
                    &app,
                    &project_id,
                    &session_id,
                    SessionStatus::Failed,
                    Some("claude 进程结束但未返回结果".to_string()),
                );
                break;
            }
            Err(e) => {
                finish(
                    &registry,
                    &claude,
                    &app,
                    &project_id,
                    &session_id,
                    SessionStatus::Failed,
                    Some(format!("读取 claude 输出失败: {e}")),
                );
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
            // Terminal: map to a `failed`/`completed` TurnCompleted (emitting an Error
            // first when the run errored), set the terminal status, deregister, stop.
            ParsedEvent::Result { is_error, message } => {
                let error = is_error.then(|| {
                    if message.is_empty() {
                        "claude review 失败".to_string()
                    } else {
                        message
                    }
                });
                let status = if is_error {
                    SessionStatus::Failed
                } else {
                    SessionStatus::Done
                };
                finish(
                    &registry,
                    &claude,
                    &app,
                    &project_id,
                    &session_id,
                    status,
                    error,
                );
                break;
            }
        }
    }

    // Reap the child so it can't linger as a zombie (it has already exited by the time
    // we see EOF/result; on an abort path this future is dropped before reaching here and
    // `kill_on_drop` reaps instead).
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
    };
    let _ = app.emit(REVIEW_EVENT, &event);
    let db = app.state::<crate::db::Database>();
    if let Err(e) = history_store::append_item(db.inner(), session_id, item_id, kind, &text) {
        eprintln!(
            "claude review history 持久化失败（{session_id}/{item_id}）：{}",
            e.message
        );
    }
}

/// Finish a session: set its terminal status in the registry + DB, emit an optional
/// `Error` event (for a failed/aborted run) followed by the terminal `TurnCompleted`, and
/// deregister from the manager so it no longer holds a handle. The terminal wire status
/// mirrors codex's: `Done → "completed"`, `Failed → "failed"`.
fn finish<R: tauri::Runtime>(
    registry: &SessionRegistry,
    claude: &ClaudeManager,
    app: &tauri::AppHandle<R>,
    project_id: &str,
    session_id: &str,
    status: SessionStatus,
    error: Option<String>,
) {
    registry.set_status(session_id, status);
    let db = app.state::<crate::db::Database>();
    if let Err(e) = history_store::set_status(db.inner(), session_id, status) {
        eprintln!(
            "claude review session 状态持久化失败（{session_id}）：{}",
            e.message
        );
    }
    if let Some(message) = error {
        let _ = app.emit(
            REVIEW_EVENT,
            &ReviewEvent::Error {
                project_id: project_id.to_string(),
                thread_id: session_id.to_string(),
                message,
            },
        );
    }
    let wire_status = match status {
        SessionStatus::Done => "completed",
        _ => "failed",
    };
    let _ = app.emit(
        REVIEW_EVENT,
        &ReviewEvent::TurnCompleted {
            project_id: project_id.to_string(),
            thread_id: session_id.to_string(),
            status: wire_status.to_string(),
        },
    );
    // Deregister WITHOUT abort (the pump is finishing on its own); a later `stop` for
    // this id then finds nothing and returns false (correct: nothing to stop).
    claude.deregister(session_id);
}

/// Best-effort mirror of an in-memory [`SessionInfo`] into the durable `review_session`
/// table. Logs + swallows errors so a persistence hiccup never breaks the live session —
/// the in-memory registry stays the authority for dedup / status (same contract as codex).
fn persist_session<R: tauri::Runtime>(app: &tauri::AppHandle<R>, info: &SessionInfo) {
    let db = app.state::<crate::db::Database>();
    if let Err(e) = history_store::upsert_session(db.inner(), info) {
        eprintln!(
            "claude review session 持久化失败（{}）：{}",
            info.thread_id, e.message
        );
    }
}

/// RAII release of a `(pr, kind)` reservation taken by `try_reserve_pair`. An undisarmed
/// guard releases on drop, so NO early `?` / error between the reserve and the session
/// insert can leak a reservation — leak-on-failure is unrepresentable, not hand-avoided
/// (the same discipline codex's `ReservationGuard` enforces; this one is claude-local).
struct ReservationGuard<'a> {
    registry: &'a SessionRegistry,
    project_id: String,
    pr_number: u64,
    kind: String,
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
                .release_pair(&self.project_id, self.pr_number, &self.kind);
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
