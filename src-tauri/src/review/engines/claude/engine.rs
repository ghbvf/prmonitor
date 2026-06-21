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
use crate::review::session::{CommentUrlContext, SessionInfo, SessionRegistry, SessionStatus};

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
    /// Hand-typed claude model name (empty = claude CLI default). Passed as `--model`
    /// to the `claude -p` subprocess when non-blank.
    pub claude_model: &'a str,
    /// IMMUTABLE comment-URL source context (AB#1042), built from the project at dispatch.
    /// Owned so it moves into `start_review` → the `Starting` session, pinning the terminal
    /// `finalize_turn`'s URL resolve to the project the review ran against (not a mid-review
    /// config edit). `pub(crate)`: crate-internal context type, in-crate constructors only.
    pub(crate) url_ctx: CommentUrlContext,
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
            self.claude_model,
            pr_number,
            kind,
            // `&self` start can't move the field; clone the owned context for this turn.
            self.url_ctx.clone(),
        )
        .await
    }

    async fn stop(&self, session: &SessionId) -> AppResult<()> {
        // The manager owns the in-flight session's cancel channel. `stop` SIGNALS cancel;
        // the pump observes it, kills its child, and emits the terminal event via `finish`
        // (so the session is never left `Running`). A `false` (unknown id) is benign here —
        // the command-level `stop_review` is the funnel that decides claude-vs-codex; an
        // engine-level stop against a gone session is a no-op.
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
/// `set_running` → register the cancel channel (BEFORE the pump spawns, so a `stop` can
/// never race ahead of registration) → spawn the pump (which owns the child + reader +
/// cancel receiver and parses to the terminal `result` or a cancel). Any failure before
/// the pump spawns releases the reservation (RAII guard) and returns `Err`.
#[allow(clippy::too_many_arguments)]
async fn start_review<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    claude: &ClaudeManager,
    registry: &SessionRegistry,
    claude_bin: &str,
    project_id: &str,
    repo_root: &str,
    claude_model: &str,
    pr_number: u64,
    kind: &str,
    // IMMUTABLE comment-URL source context (AB#1042); handed to the `Starting` session in
    // `promote_reservation` so the terminal `finalize_turn` resolves the URL against the
    // project this review ran against (mirrors the codex path).
    url_ctx: CommentUrlContext,
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
    let proc = process::spawn_claude(claude_bin, repo_root, claude_model, &prompt)?;
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
        // No comment yet — filled by `session::finalize_turn` at a `completed` terminal (AB#1042).
        comment_url: None,
    };
    registry.promote_reservation(starting.clone(), url_ctx);
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
    };
    let _ = app.emit(REVIEW_EVENT, &event);
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
fn persist_session<R: tauri::Runtime>(app: &tauri::AppHandle<R>, info: &SessionInfo) {
    let db = app.state::<crate::db::Database>();
    if let Err(e) = history_store::upsert_session(db.inner(), info) {
        eprintln!(
            "claude review session 持久化失败（{}）：{}",
            info.thread_id, e.message
        );
        // Symmetry with codex (review F9): the first silent persist failure raises one
        // app-level notice; later failures only log (the shared process-global guard).
        crate::review::session::notify_persist_failure_once(app, &info.project_id);
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
