//! Review session state machine + session manager.
//!
//! One resident `codex app-server` process (held in [`crate::state::AppState`]'s
//! `CodexManager`) drives many sessions keyed by `threadId`. Per session:
//! `thread/start` → `turn/start` (the pr-review skill) starts it; a spawned
//! *pump* task maps the codex notification stream into
//! [`crate::events::ReviewEvent`]s and emits them to the frontend; `turn/interrupt`
//! stops it (the terminal `turn/completed`, status `interrupted`, arrives on the
//! stream). The [`SessionRegistry`] tracks each session's lifecycle for
//! `list_review_sessions`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};
use tokio::sync::broadcast;

use super::engines::codex::process;
use super::engines::codex::protocol::{
    SandboxPolicy, ServerNotification, ThreadStartParams, TurnInterruptParams, TurnStartParams,
    UserInput,
};
use super::engines::codex::CodexManager;
use crate::error::{AppError, AppResult};
use crate::events::{ReviewEvent, REVIEW_EVENT};
use crate::review::engine::StartReviewOutcome;

/// A review session is identified by its codex `threadId`.
pub type ThreadId = String;

/// The skill `name` attached to every review turn (matches the local project
/// skill under `<repo_root>/<skillRelPath>`).
const PR_REVIEW_SKILL: &str = "pr-review";

/// Lifecycle of one review session (the state machine). Serialized camelCase for
/// `list_review_sessions`; `Deserialize` so the persisted `review_session.status` wire
/// string (#70) projects back into this enum in `history_store::get_pr_sessions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionStatus {
    /// `thread/start` / `turn/start` in flight (not yet observed on the stream).
    Starting,
    /// The turn is running; deltas are streaming.
    Running,
    /// `turn/interrupt` issued; awaiting the terminal `turn/completed`.
    Interrupting,
    /// The turn finished (`completed` or `interrupted`).
    Done,
    /// The turn failed, or the connection dropped mid-session.
    Failed,
}

/// A review session's public snapshot for `list_review_sessions`. Slice-private
/// wire type, mirrored in `src/review/types.ts` (NOT `model.rs` / `src/types.ts`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    /// Owning project (#35): the routing key the UI filters its session list by.
    /// A PR number is unique only *within* a project, so a session is identified
    /// to the user by `(project_id, pr_number, kind)` — `thread_id` stays the
    /// globally-unique registry key (codex assigns one per `thread/start`).
    pub project_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub pr_number: u64,
    /// `"review"` or `"check"` — the trigger-label mode the review was started in.
    pub kind: String,
    pub status: SessionStatus,
}

/// In-memory registry of review sessions keyed by `threadId`. Lives in
/// `AppState`; `Clone` (shares one `Arc`) so the command path and each pump task
/// see the same map. Every critical section is synchronous (no `.await` under the
/// lock).
#[derive(Default, Clone)]
pub struct SessionRegistry {
    inner: Arc<Mutex<RegistryState>>,
}

/// The registry's single critical section: the session map AND the set of
/// `(project_id, pr, kind)` triples RESERVED by an in-flight [`start_review`] that has
/// not yet inserted its `Starting` session. Both live under ONE mutex, so reserve /
/// promote-to-session / [`SessionRegistry::active_pairs`] are mutually atomic — the
/// reservation closes the window where a session exists conceptually (its
/// `thread/start` is mid-flight) but isn't yet in `sessions`, the gap the old
/// snapshot-then-act guard could not see (two concurrent webhook deliveries both
/// passing an empty snapshot → double review).
///
/// The reservation key carries `project_id` (#35): a PR number is unique only within a
/// project, so a PR #7 review in project A must NOT dedup against a PR #7 review in
/// project B. `sessions` stays keyed by `thread_id` alone — codex assigns a globally
/// unique `threadId` per `thread/start`, so it needs no project dimension.
#[derive(Default)]
struct RegistryState {
    sessions: HashMap<ThreadId, SessionInfo>,
    reserved: HashSet<(String, u64, String)>,
}

/// Outcome of [`SessionRegistry::begin_interrupt`] — the atomic guard that makes
/// `stop` race-free and idempotent.
enum BeginInterrupt {
    /// Was `Running`; flipped to `Interrupting`. Caller interrupts this `turn_id`.
    Proceed(String),
    /// Already interrupting / done / failed / starting — nothing to (re)interrupt.
    AlreadyHandled,
    /// No session with this id.
    NotFound,
}

impl SessionRegistry {
    /// Test-only direct session insert. Production starts a session via
    /// [`Self::promote_reservation`] (which also consumes the reservation); tests use
    /// this to seed arbitrary statuses (`Running`/`Done`/…) without a reservation.
    #[cfg(test)]
    fn insert(&self, info: SessionInfo) {
        self.inner
            .lock()
            .unwrap()
            .sessions
            .insert(info.thread_id.clone(), info);
    }

    fn set_status(&self, thread_id: &str, status: SessionStatus) {
        if let Some(info) = self.inner.lock().unwrap().sessions.get_mut(thread_id) {
            info.status = status;
        }
    }

    /// Record the turn id and flip to [`SessionStatus::Running`] once `turn/start`
    /// has returned (the session was inserted as `Starting` before the turn began).
    fn set_running(&self, thread_id: &str, turn_id: String) {
        if let Some(info) = self.inner.lock().unwrap().sessions.get_mut(thread_id) {
            info.turn_id = turn_id;
            info.status = SessionStatus::Running;
        }
    }

    /// Atomically reserve `(project_id, pr_number, kind)` for a dispatch about to start
    /// a review, BEFORE the async `thread/start` — so the triple is visible to a
    /// concurrent dispatch's guard the instant this returns, not only after the
    /// `Starting` insert. `true` = the caller now OWNS the reservation; `false` = the
    /// triple is already covered (a prior reservation OR an in-flight session), so the
    /// caller must NOT start and must NOT release (it owns nothing). One synchronous
    /// critical section against the same mutex as `sessions`, so two concurrent
    /// reservations for the same triple cannot both win (test-and-set) — this is what
    /// makes the idempotency boundary atomic rather than a snapshot. `project_id` scopes
    /// the dedup (#35): the same PR number in two different projects reserves
    /// independently.
    pub fn try_reserve_pair(&self, project_id: &str, pr_number: u64, kind: &str) -> bool {
        let mut st = self.inner.lock().unwrap();
        let covered_by_session = st.sessions.values().any(|s| {
            s.project_id == project_id
                && s.pr_number == pr_number
                && s.kind == kind
                && matches!(
                    s.status,
                    SessionStatus::Starting | SessionStatus::Running | SessionStatus::Interrupting
                )
        });
        if covered_by_session
            || st
                .reserved
                .contains(&(project_id.to_string(), pr_number, kind.to_string()))
        {
            return false;
        }
        st.reserved
            .insert((project_id.to_string(), pr_number, kind.to_string()));
        true
    }

    /// Release a reservation taken by [`Self::try_reserve_pair`] whose review did NOT
    /// reach a `Starting` session (a `thread/start` failure / early return / panic
    /// before the insert). A successful start hands the reservation to the inserted
    /// session via [`Self::promote_reservation`], so the happy path never calls this.
    /// Idempotent (a missing triple is a no-op). Keyed by the full
    /// `(project_id, pr, kind)` so it frees exactly the triple `try_reserve_pair` took.
    fn release_pair(&self, project_id: &str, pr_number: u64, kind: &str) {
        self.inner.lock().unwrap().reserved.remove(&(
            project_id.to_string(),
            pr_number,
            kind.to_string(),
        ));
    }

    /// Insert the just-started session as `Starting` AND drop its reservation in ONE
    /// critical section, so a concurrent reserve / guard never sees the pair as
    /// unreserved-and-not-yet-a-session (the gap between `thread/start` success and the
    /// insert). The pair stays continuously covered: reserved → (this swap) →
    /// in-flight session.
    fn promote_reservation(&self, info: SessionInfo) {
        let mut st = self.inner.lock().unwrap();
        st.reserved
            .remove(&(info.project_id.clone(), info.pr_number, info.kind.clone()));
        st.sessions.insert(info.thread_id.clone(), info);
    }

    /// Atomically begin an interrupt. Only a [`SessionStatus::Running`] session
    /// transitions to [`SessionStatus::Interrupting`] and yields its `turn_id` to
    /// interrupt; an already interrupting / terminal / still-starting session is a
    /// no-op (so a double `stop` is idempotent), and a missing session is an error.
    /// The check-and-set is one synchronous critical section, so two concurrent
    /// stops can't both proceed.
    fn begin_interrupt(&self, thread_id: &str) -> BeginInterrupt {
        let mut st = self.inner.lock().unwrap();
        match st.sessions.get_mut(thread_id) {
            None => BeginInterrupt::NotFound,
            Some(info) => match info.status {
                SessionStatus::Running => {
                    info.status = SessionStatus::Interrupting;
                    BeginInterrupt::Proceed(info.turn_id.clone())
                }
                _ => BeginInterrupt::AlreadyHandled,
            },
        }
    }

    /// Revert a failed interrupt: [`SessionStatus::Interrupting`] →
    /// [`SessionStatus::Running`], so a retry can stop the still-running turn. A
    /// terminal status that raced in via the pump meanwhile is left untouched.
    fn rollback_interrupt(&self, thread_id: &str) {
        if let Some(info) = self.inner.lock().unwrap().sessions.get_mut(thread_id) {
            if info.status == SessionStatus::Interrupting {
                info.status = SessionStatus::Running;
            }
        }
    }

    /// Snapshot of all known sessions (for `list_review_sessions`).
    pub fn list(&self) -> Vec<SessionInfo> {
        self.inner
            .lock()
            .unwrap()
            .sessions
            .values()
            .cloned()
            .collect()
    }

    /// The `(pr_number, kind)` of every in-flight session
    /// (`Starting`/`Running`/`Interrupting`) PLUS every RESERVED triple **belonging to
    /// `project_id`** — the auto-trigger registry guard's view, scoped to one project
    /// (#35). A PR with an in-flight (or reserved) session of a given kind must not be
    /// re-dispatched *within the same project*; a PR #7 in project A does NOT block a PR
    /// #7 in project B. A terminal (`Done`/`Failed`) session is finished and excluded.
    /// Including reservations is what lets a not-yet-`Starting` dispatch still block a
    /// concurrent one. Owning the "what counts as active" rule here keeps
    /// [`SessionStatus`] inside the review slice — the composition-layer dispatcher
    /// consumes only the pairs, so it never imports the session state machine. The
    /// returned pairs drop the project dimension because the caller already scopes its
    /// candidate batch to this project.
    pub fn active_pairs(&self, project_id: &str) -> Vec<(u64, String)> {
        let st = self.inner.lock().unwrap();
        let mut pairs: Vec<(u64, String)> = st
            .sessions
            .values()
            .filter(|s| {
                s.project_id == project_id
                    && matches!(
                        s.status,
                        SessionStatus::Starting
                            | SessionStatus::Running
                            | SessionStatus::Interrupting
                    )
            })
            .map(|s| (s.pr_number, s.kind.clone()))
            .collect();
        // A reserved triple has no session yet (its `thread/start` is mid-flight) but is
        // every bit as "in flight" — include it (scoped to this project) so the dispatch
        // guard and a concurrent reserve both see it. A duplicate vs a just-promoted
        // session is harmless (the guard does set membership, not counting).
        pairs.extend(
            st.reserved
                .iter()
                .filter(|(pid, _, _)| pid == project_id)
                .map(|(_, pr, kind)| (*pr, kind.clone())),
        );
        pairs
    }
}

/// RAII release of a `(pr, kind)` reservation taken by
/// [`SessionRegistry::try_reserve_pair`]. [`Self::disarm`] is called once the
/// reservation has been handed to a `Starting` session ([`SessionRegistry::promote_reservation`]);
/// an UNdisarmed guard releases on drop, so NO early `?` / error / panic between the
/// reserve and the insert can leak a reservation — leak-on-failure is made
/// unrepresentable, not merely hand-avoided on each return path.
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

/// Start a review for `pr_number` and stream its output. Returns
/// [`StartReviewOutcome::Started`] with the codex `threadId`, or
/// [`StartReviewOutcome::Deduped`] when the `(pr_number, kind)` is already covered by
/// an in-flight (or reserved) review — not started. The dispatch path skips a
/// `Deduped` (neither recorded nor a failure); the manual command surfaces it as a
/// benign "already in flight".
///
/// Subscribes to the notification stream BEFORE `turn/start` so no early delta is
/// missed, then spawns a pump task that forwards events until the turn completes.
#[allow(clippy::too_many_arguments)]
pub async fn start_review<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    codex: &CodexManager,
    registry: &SessionRegistry,
    codex_bin: &str,
    repo: &str,
    repo_root: &str,
    skill_abs_path: &str,
    project_id: &str,
    pr_number: u64,
    kind: &str,
) -> AppResult<StartReviewOutcome> {
    // Atomic test-and-set BEFORE any `.await`: if this `(project_id, pr, kind)` is
    // already reserved or covered by an in-flight session, do NOT start a second review.
    // This is the idempotency boundary — atomic, not the old snapshot-then-act guard
    // that two concurrent webhook deliveries could both pass before either's `Starting`
    // landed. `project_id` scopes the dedup (#35) so the same PR in two projects starts
    // independently.
    if !registry.try_reserve_pair(project_id, pr_number, kind) {
        return Ok(StartReviewOutcome::Deduped);
    }
    // From here, ANY early return / `?` / panic before `promote_reservation` releases
    // the reservation via the guard's `Drop` (leak-proof); `disarm()` on success hands
    // it to the inserted session instead.
    let reservation = ReservationGuard {
        registry,
        project_id: project_id.to_string(),
        pr_number,
        kind: kind.to_string(),
        armed: true,
    };

    let client = codex.connection(codex_bin, repo_root).await?;

    // Subscribe before starting the turn: the broadcast buffers from here, so the
    // pump (spawned after `turn/start` returns) sees every delta from turn start.
    let rx = client.subscribe();

    let thread_id = process::start_thread(
        &client,
        ThreadStartParams {
            cwd: Some(repo_root.to_string()),
        },
    )
    .await?;

    // Hand the reservation to a `Starting` session in ONE critical section (no gap):
    // the pair stays continuously covered (reserved → Starting), so a concurrent
    // dispatch never slips between `thread/start` success and this insert. A later
    // `turn/start` failure flips it to `Failed` (visible to `list_review_sessions`,
    // not vanished); `turn_id` is filled once the turn starts.
    let starting = SessionInfo {
        project_id: project_id.to_string(),
        thread_id: thread_id.clone(),
        turn_id: String::new(),
        pr_number,
        kind: kind.to_string(),
        status: SessionStatus::Starting,
    };
    registry.promote_reservation(starting.clone());
    // Mirror the in-memory session into the durable `review_session` table (#70) so this
    // PR's session list survives a restart and its history can be reopened. Best-effort.
    persist_session(app, &starting);
    reservation.disarm();

    let prompt = review_prompt(repo, &skill_command(pr_number, kind));
    let turn_id = match process::start_turn(
        &client,
        TurnStartParams {
            thread_id: thread_id.clone(),
            input: vec![
                UserInput::Skill {
                    name: PR_REVIEW_SKILL.to_string(),
                    path: skill_abs_path.to_string(),
                },
                UserInput::Text { text: prompt },
            ],
            // Unattended: never prompt for approval (the reader auto-answers any
            // reverse approval request as a backstop). Workspace-write + network
            // so the pr-review skill can run `git`/`gh` and post comments.
            approval_policy: "never".to_string(),
            sandbox_policy: SandboxPolicy {
                kind: "workspaceWrite".to_string(),
                network_access: true,
                writable_roots: vec![repo_root.to_string()],
            },
            cwd: Some(repo_root.to_string()),
        },
    )
    .await
    {
        Ok(turn_id) => turn_id,
        Err(e) => {
            registry.set_status(&thread_id, SessionStatus::Failed);
            persist_status(app, &thread_id, SessionStatus::Failed);
            return Err(e);
        }
    };

    registry.set_running(&thread_id, turn_id.clone());
    // Mirror the Running transition (+ the now-known turn id) into `review_session` (#70).
    persist_session(
        app,
        &SessionInfo {
            project_id: project_id.to_string(),
            thread_id: thread_id.clone(),
            turn_id,
            pr_number,
            kind: kind.to_string(),
            status: SessionStatus::Running,
        },
    );

    // Capture `project_id` as an owned String at spawn time so the pump stamps every
    // emitted `ReviewEvent` with it WITHOUT re-looking-up the session per event (#35):
    // the routing key is fixed for the session's life, and a lookup would also race the
    // terminal removal. The pump filters the shared notification stream by `thread_id`
    // but carries `project_id` to attribute each delta to the owning project.
    tauri::async_runtime::spawn(pump(
        rx,
        project_id.to_string(),
        thread_id.clone(),
        app.clone(),
        registry.clone(),
    ));

    Ok(StartReviewOutcome::Started(thread_id))
}

/// Interrupt a running review session. The terminal `turn/completed` (status
/// `interrupted`) arrives on the stream and the pump finishes the session.
///
/// `codex_bin`/`repo_root` are no longer used to build the connection: interrupting
/// only makes sense against an already-live process, so we use
/// [`CodexManager::existing_client`] (no spawn, no `stopped`-clear) rather than
/// `connection` — interrupting a dead/stopped session must NOT revive the
/// app-server (PR #47 F2). The params stay in the signature because the
/// `ReviewEngine` impl (`engine.rs`) passes them; Rust does not lint unused fn
/// params, so this is clippy-clean.
///
/// Keys on `session_id` (the codex `threadId`) ONLY — never reads any per-project
/// config — so the command builds its `CodexEngine` with `project_id: ""` (and empty
/// repo/skill fields, `commands.rs::stop_review`). A missing / invalid config must not
/// be able to block stopping a running review.
pub async fn stop_review(
    codex: &CodexManager,
    registry: &SessionRegistry,
    codex_bin: &str,
    repo_root: &str,
    session_id: &str,
) -> AppResult<()> {
    // `codex_bin`/`repo_root` are unused since the `existing_client` switch (F2); bind
    // them to `_` so the intent is explicit (the signature keeps them for the engine).
    let _ = (codex_bin, repo_root);
    // Atomic guard: only a `Running` session flips to `Interrupting` (and yields its
    // turn id); a repeat stop is an idempotent no-op, an unknown id an error.
    let turn_id = match registry.begin_interrupt(session_id) {
        BeginInterrupt::Proceed(turn_id) => turn_id,
        BeginInterrupt::AlreadyHandled => return Ok(()),
        BeginInterrupt::NotFound => {
            return Err(AppError::new(format!("未找到 review 会话: {session_id}")))
        }
    };
    // We're now `Interrupting`. Take ONLY an already-live connection; never spawn.
    let client = match codex.existing_client() {
        Some(client) => client,
        // No live process → the session is already terminated (a dead transport
        // makes the pump fail it via `ConnectionClosed`/`Closed`), and there is
        // nothing live to interrupt. Don't spawn / revive the app-server just to
        // interrupt a gone turn (PR #47 F2). Roll back our optimistic
        // `Interrupting` mark so we never leave a half-interrupted, un-stoppable
        // session, then report success — the stop's intent (no running turn) holds.
        None => {
            registry.rollback_interrupt(session_id);
            return Ok(());
        }
    };
    // Any failure below must roll back to `Running` so a retry can interrupt again —
    // never leave a half-interrupted, un-stoppable session (the pump still owns the
    // real terminal transition on `turn/completed`).
    if let Err(e) = process::interrupt_turn(
        &client,
        TurnInterruptParams {
            thread_id: session_id.to_string(),
            turn_id,
        },
    )
    .await
    {
        registry.rollback_interrupt(session_id);
        return Err(e);
    }
    Ok(())
}

/// Best-effort mirror of an in-memory [`SessionInfo`] into the durable `review_session`
/// table (#70). Logs + swallows errors: a persistence hiccup must never break the live
/// session (the in-memory registry stays the authority for dedup / status).
fn persist_session<R: tauri::Runtime>(app: &tauri::AppHandle<R>, info: &SessionInfo) {
    let db = app.state::<crate::db::Database>();
    if let Err(e) = super::history_store::upsert_session(db.inner(), info) {
        eprintln!(
            "review session 持久化失败（{}）：{}",
            info.thread_id, e.message
        );
    }
}

/// Best-effort mirror of a session status transition into `review_session` (#70). Used at
/// terminal transitions in the pump where only the thread id is at hand.
fn persist_status<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    thread_id: &str,
    status: SessionStatus,
) {
    let db = app.state::<crate::db::Database>();
    if let Err(e) = super::history_store::set_status(db.inner(), thread_id, status) {
        eprintln!(
            "review session 状态持久化失败（{thread_id}）：{}",
            e.message
        );
    }
}

/// Best-effort capture of a streamed delta into the persisted session history (#70).
/// Called AFTER `app.emit` so the live stream never waits on the DB; a non-delta event is
/// a no-op, and a persist error is logged + swallowed (the rendered stream is unaffected
/// — at worst the last delta before a crash is missing from the reopened history).
fn persist_delta<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    thread_id: &str,
    event: &ReviewEvent,
) {
    let (item_id, kind, text) = match event {
        ReviewEvent::MessageDelta { item_id, text, .. } => (item_id, "message", text),
        ReviewEvent::ReasoningDelta { item_id, text, .. } => (item_id, "reasoning", text),
        _ => return,
    };
    let db = app.state::<crate::db::Database>();
    if let Err(e) = super::history_store::append_item(db.inner(), thread_id, item_id, kind, text) {
        eprintln!(
            "review history 持久化失败（{thread_id}/{item_id}）：{}",
            e.message
        );
    }
}

/// Pump task: forward this session's notifications to the frontend as
/// [`ReviewEvent`]s until the turn completes (or the connection drops). Filters by
/// `thread_id` since the broadcast carries every session's stream; stamps every
/// emitted event with `project_id` (#35), the owning-project routing key captured at
/// spawn (fixed for the session's life — never re-looked-up per event).
async fn pump<R: tauri::Runtime>(
    mut rx: broadcast::Receiver<Arc<ServerNotification>>,
    project_id: String,
    thread_id: String,
    app: tauri::AppHandle<R>,
    registry: SessionRegistry,
) {
    loop {
        match rx.recv().await {
            // Synthetic signal the rpc reader broadcasts when its loop exits (codex
            // EOF / IO error): the transport is gone, so every session dies with it
            // regardless of thread. End this one as `Failed` rather than wait on
            // deltas that will never arrive (the `RpcClient` keeps the broadcast
            // `Sender` alive across a dead reader, so `RecvError::Closed` below never
            // fires for a process-death; this is what catches that case).
            Ok(note) if matches!(note.as_ref(), ServerNotification::ConnectionClosed) => {
                fail_connection_closed(&registry, &app, &project_id, &thread_id);
                break;
            }
            Ok(note) => {
                let Some(event) = map_notification(&note, &project_id, &thread_id) else {
                    continue;
                };
                if let ReviewEvent::TurnCompleted { status, .. } = &event {
                    let terminal = terminal_status(status);
                    registry.set_status(&thread_id, terminal);
                    let _ = app.emit(REVIEW_EVENT, &event);
                    persist_status(&app, &thread_id, terminal); // mirror terminal to DB (#70)
                    break; // terminal — the turn is over.
                }
                // Emit FIRST (streaming latency must not wait on the DB), THEN persist the
                // delta to the session history (#70) best-effort — a persist error is
                // logged, never breaks the live stream.
                let _ = app.emit(REVIEW_EVENT, &event);
                persist_delta(&app, &thread_id, &event);
            }
            // The pump fell behind the shared ring and `n` notifications were
            // evicted. The terminal `turn/completed` may have been among them
            // (another session can flood the ring after ours), which would hang
            // this pump on `recv()` forever — and the dropped deltas already make
            // the rendered stream incomplete. So end the session honestly with a
            // Failed terminal + error rather than risk a stuck `Running`.
            Err(broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("review pump（{thread_id}）滞后，丢弃 {n} 条通知");
                registry.set_status(&thread_id, SessionStatus::Failed);
                persist_status(&app, &thread_id, SessionStatus::Failed); // mirror to DB (#70)
                let _ = app.emit(
                    REVIEW_EVENT,
                    &ReviewEvent::Error {
                        project_id: project_id.clone(),
                        thread_id: thread_id.clone(),
                        message: format!("codex 输出流滞后，丢弃 {n} 条消息（review 中断）"),
                    },
                );
                break;
            }
            // The broadcast itself closed (every `Sender` dropped — i.e. the whole
            // `RpcClient` was torn down, e.g. manager shutdown). Same terminal
            // outcome as the synthetic `ConnectionClosed` above.
            Err(broadcast::error::RecvError::Closed) => {
                fail_connection_closed(&registry, &app, &project_id, &thread_id);
                break;
            }
        }
    }
}

/// End a session as `Failed` with a "connection closed" error event. Shared by the
/// pump's two transport-teardown paths: the synthetic `ConnectionClosed` (reader
/// exited but the `RpcClient` lives on) and `RecvError::Closed` (the whole client
/// dropped).
fn fail_connection_closed<R: tauri::Runtime>(
    registry: &SessionRegistry,
    app: &tauri::AppHandle<R>,
    project_id: &str,
    thread_id: &str,
) {
    registry.set_status(thread_id, SessionStatus::Failed);
    persist_status(app, thread_id, SessionStatus::Failed); // mirror to DB (#70)
    let _ = app.emit(
        REVIEW_EVENT,
        &ReviewEvent::Error {
            project_id: project_id.to_string(),
            thread_id: thread_id.to_string(),
            message: "codex 连接已关闭".to_string(),
        },
    );
}

/// Map one codex notification to a [`ReviewEvent`] for this session, or `None`
/// if it belongs to another thread / is not a streamed unit we forward. Every
/// produced event is stamped with `project_id` (#35), the owning-project routing key
/// the pump captured at spawn. Pure — unit-tested below.
fn map_notification(
    note: &ServerNotification,
    project_id: &str,
    thread_id: &str,
) -> Option<ReviewEvent> {
    match note {
        ServerNotification::AgentMessageDelta(d) if d.thread_id == thread_id => {
            Some(ReviewEvent::MessageDelta {
                project_id: project_id.to_string(),
                thread_id: d.thread_id.clone(),
                item_id: d.item_id.clone(),
                text: d.delta.clone(),
            })
        }
        ServerNotification::ReasoningTextDelta(d) if d.thread_id == thread_id => {
            Some(ReviewEvent::ReasoningDelta {
                project_id: project_id.to_string(),
                thread_id: d.thread_id.clone(),
                item_id: d.item_id.clone(),
                text: d.delta.clone(),
            })
        }
        ServerNotification::TurnCompleted(d) if d.thread_id == thread_id => {
            Some(ReviewEvent::TurnCompleted {
                project_id: project_id.to_string(),
                thread_id: d.thread_id.clone(),
                status: d.turn.status.clone(),
            })
        }
        _ => None,
    }
}

/// Map a codex `turn.status` to the session's terminal state. `completed` /
/// `interrupted` are both successful endings (the latter is a user stop);
/// anything else (`failed`, …) is [`SessionStatus::Failed`].
fn terminal_status(status: &str) -> SessionStatus {
    match status {
        "completed" | "interrupted" => SessionStatus::Done,
        _ => SessionStatus::Failed,
    }
}

/// The skill command the review turn instructs codex to run: `/pr-review <N>` for
/// a review, `/pr-review <N> --check` for a check.
fn skill_command(pr_number: u64, kind: &str) -> String {
    if kind == "check" {
        format!("/{PR_REVIEW_SKILL} {pr_number} --check")
    } else {
        format!("/{PR_REVIEW_SKILL} {pr_number}")
    }
}

/// The turn's instruction text. Ported from `router.py:590-597`; the
/// machine-block clause is dropped because prmonitor's pr-review skill posts plain
/// `pm:` comments (no machine block — see #24).
fn review_prompt(repo: &str, skill_command: &str) -> String {
    format!(
        "Use the attached local project skill `{PR_REVIEW_SKILL}` exactly. \
         Execute `{skill_command}` for repository `{repo}`. \
         Complete the full skill workflow, including posting the pm:pr-review \
         comment and actually applying the label transition required by the skill. \
         Do not stop at a label-transition suggestion. Do not use the built-in \
         Codex review command."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::engines::codex::protocol::{
        AgentMessageDelta, OutputDelta, ReasoningTextDelta, TurnCompletedNotification,
        TurnStatusRef,
    };

    fn msg_delta(thread: &str) -> ServerNotification {
        ServerNotification::AgentMessageDelta(AgentMessageDelta {
            thread_id: thread.to_string(),
            turn_id: "tn".to_string(),
            item_id: "it".to_string(),
            delta: "hello".to_string(),
        })
    }

    #[test]
    fn map_notification_forwards_own_thread_message_delta() {
        match map_notification(&msg_delta("t1"), "p1", "t1") {
            Some(ReviewEvent::MessageDelta {
                project_id,
                thread_id,
                item_id,
                text,
            }) => {
                // The pump stamps the captured owning-project id on every event (#35).
                assert_eq!(project_id, "p1");
                assert_eq!(thread_id, "t1");
                assert_eq!(item_id, "it");
                assert_eq!(text, "hello");
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    #[test]
    fn map_notification_drops_other_thread() {
        // A delta for a different session must not leak into this pump.
        assert!(map_notification(&msg_delta("other"), "p1", "t1").is_none());
    }

    #[test]
    fn map_notification_ignores_non_forwarded_notifications() {
        // OutputDelta (command/exec output) and Other (unknown) are not streamed
        // to the UI in PR6 — the `_ => None` arm must hold for them.
        let output = ServerNotification::OutputDelta(OutputDelta {
            process_id: Some("p".to_string()),
            process_handle: None,
            stream: "stdout".to_string(),
            delta_base64: "dGhl".to_string(),
            cap_reached: false,
        });
        assert!(map_notification(&output, "p1", "t1").is_none());

        let other = ServerNotification::Other {
            method: "thread/futureThing".to_string(),
            params: serde_json::json!({}),
        };
        assert!(map_notification(&other, "p1", "t1").is_none());
    }

    #[test]
    fn map_notification_maps_reasoning_delta() {
        let n = ServerNotification::ReasoningTextDelta(ReasoningTextDelta {
            thread_id: "t1".to_string(),
            turn_id: "tn".to_string(),
            item_id: "r1".to_string(),
            delta: "thinking".to_string(),
        });
        assert!(matches!(
            map_notification(&n, "p1", "t1"),
            Some(ReviewEvent::ReasoningDelta { .. })
        ));
    }

    #[test]
    fn map_notification_maps_turn_completed_status() {
        let n = ServerNotification::TurnCompleted(TurnCompletedNotification {
            thread_id: "t1".to_string(),
            turn: TurnStatusRef {
                status: "interrupted".to_string(),
            },
        });
        match map_notification(&n, "p1", "t1") {
            Some(ReviewEvent::TurnCompleted {
                project_id, status, ..
            }) => {
                assert_eq!(project_id, "p1");
                assert_eq!(status, "interrupted");
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }
    }

    #[test]
    fn terminal_status_maps_codex_status() {
        assert_eq!(terminal_status("completed"), SessionStatus::Done);
        assert_eq!(terminal_status("interrupted"), SessionStatus::Done);
        assert_eq!(terminal_status("failed"), SessionStatus::Failed);
        assert_eq!(terminal_status("anythingElse"), SessionStatus::Failed);
    }

    #[test]
    fn skill_command_matches_kind() {
        assert_eq!(skill_command(7, "review"), "/pr-review 7");
        assert_eq!(skill_command(7, "check"), "/pr-review 7 --check");
    }

    #[test]
    fn review_prompt_names_skill_command_and_repo() {
        let p = review_prompt("owner/name", "/pr-review 7");
        assert!(p.contains("/pr-review 7"));
        assert!(p.contains("owner/name"));
        assert!(p.contains("pr-review"));
        // The dropped router.py machine-block clause must stay dropped.
        assert!(!p.to_lowercase().contains("machine block"));
    }

    #[test]
    fn registry_insert_status_and_list_roundtrip() {
        let reg = SessionRegistry::default();
        reg.insert(SessionInfo {
            project_id: "p1".to_string(),
            thread_id: "t1".to_string(),
            turn_id: "tn1".to_string(),
            pr_number: 7,
            kind: "review".to_string(),
            status: SessionStatus::Running,
        });
        assert_eq!(reg.list()[0].turn_id, "tn1");
        reg.set_status("t1", SessionStatus::Done);
        let list = reg.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].status, SessionStatus::Done);
        assert!(matches!(
            reg.begin_interrupt("missing"),
            BeginInterrupt::NotFound
        ));
    }

    #[test]
    fn active_pairs_returns_only_in_flight_sessions() {
        let reg = SessionRegistry::default();
        let info = |thread: &str, pr: u64, kind: &str, status| {
            reg.insert(SessionInfo {
                project_id: "p1".to_string(),
                thread_id: thread.to_string(),
                turn_id: String::new(),
                pr_number: pr,
                kind: kind.to_string(),
                status,
            });
        };
        info("a", 1, "review", SessionStatus::Starting);
        info("b", 2, "check", SessionStatus::Running);
        info("c", 3, "review", SessionStatus::Interrupting);
        info("d", 4, "review", SessionStatus::Done); // terminal → excluded
        info("e", 5, "check", SessionStatus::Failed); // terminal → excluded

        let mut pairs = reg.active_pairs("p1");
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                (1, "review".to_string()),
                (2, "check".to_string()),
                (3, "review".to_string()),
            ]
        );
    }

    #[test]
    fn try_reserve_pair_is_atomic_test_and_set() {
        let reg = SessionRegistry::default();
        assert!(
            reg.try_reserve_pair("p1", 7, "review"),
            "first reservation wins"
        );
        assert!(
            !reg.try_reserve_pair("p1", 7, "review"),
            "second is rejected while reserved"
        );
        // A reservation shows up in active_pairs BEFORE any Starting session exists —
        // exactly the gap the old snapshot-then-act guard could not see.
        assert!(reg.active_pairs("p1").contains(&(7, "review".to_string())));
        // A different kind for the same PR is independent (key is (project, pr, kind)).
        assert!(reg.try_reserve_pair("p1", 7, "check"));
        // Release frees it for a later cycle.
        reg.release_pair("p1", 7, "review");
        assert!(
            reg.try_reserve_pair("p1", 7, "review"),
            "reservable again after release"
        );
    }

    #[test]
    fn try_reserve_pair_rejects_when_session_in_flight() {
        let reg = SessionRegistry::default();
        // An in-flight (Running) session covers the pair even with no reservation.
        reg.insert(SessionInfo {
            project_id: "p1".to_string(),
            thread_id: "t1".to_string(),
            turn_id: "tn".to_string(),
            pr_number: 7,
            kind: "review".to_string(),
            status: SessionStatus::Running,
        });
        assert!(!reg.try_reserve_pair("p1", 7, "review"));
        // A different kind is still reservable; a terminal session would not block
        // (covered by the active_pairs in-flight filter, exercised elsewhere).
        assert!(reg.try_reserve_pair("p1", 7, "check"));
    }

    #[test]
    fn reservations_and_active_pairs_are_isolated_per_project() {
        // #35: a PR number is unique only WITHIN a project. The same `(pr, kind)` in two
        // projects must reserve independently, and `active_pairs` must scope to its
        // project — a PR #7 review in project A must never block PR #7 in project B,
        // nor leak into B's active-pairs snapshot.
        let reg = SessionRegistry::default();
        assert!(reg.try_reserve_pair("A", 7, "review"), "A reserves freely");
        assert!(
            reg.try_reserve_pair("B", 7, "review"),
            "B reserves the same (pr, kind) independently of A"
        );
        // Re-reserving within the SAME project still dedups (the within-project guard).
        assert!(
            !reg.try_reserve_pair("A", 7, "review"),
            "dedup within a project is preserved"
        );

        // Each project's active_pairs sees ONLY its own reservation.
        assert_eq!(reg.active_pairs("A"), vec![(7, "review".to_string())]);
        assert_eq!(reg.active_pairs("B"), vec![(7, "review".to_string())]);
        assert!(
            reg.active_pairs("C").is_empty(),
            "a project with nothing in flight sees an empty snapshot"
        );

        // An in-flight SESSION (not just a reservation) is likewise project-scoped:
        // A's promoted session does not appear in B's active_pairs and does not block B.
        reg.promote_reservation(SessionInfo {
            project_id: "A".to_string(),
            thread_id: "tA".to_string(),
            turn_id: String::new(),
            pr_number: 9,
            kind: "review".to_string(),
            status: SessionStatus::Running,
        });
        assert!(
            reg.active_pairs("A").contains(&(9, "review".to_string())),
            "A's session shows in A"
        );
        assert!(
            !reg.active_pairs("B").contains(&(9, "review".to_string())),
            "A's session must not leak into B"
        );
        assert!(
            reg.try_reserve_pair("B", 9, "review"),
            "A's in-flight (9, review) session does not block B's (9, review)"
        );

        // Releasing A's reservation leaves B's untouched (full-triple keying).
        reg.release_pair("A", 7, "review");
        assert!(
            reg.try_reserve_pair("A", 7, "review"),
            "A reservable again after its own release"
        );
        assert!(
            !reg.try_reserve_pair("B", 7, "review"),
            "B's reservation was not disturbed by A's release"
        );
    }

    #[test]
    fn promote_reservation_hands_off_without_a_gap() {
        let reg = SessionRegistry::default();
        assert!(reg.try_reserve_pair("p1", 7, "review"));
        reg.promote_reservation(SessionInfo {
            project_id: "p1".to_string(),
            thread_id: "t1".to_string(),
            turn_id: String::new(),
            pr_number: 7,
            kind: "review".to_string(),
            status: SessionStatus::Starting,
        });
        // After promotion the pair is covered by the Starting session, not the reserved
        // set — and a concurrent reserve still loses (continuous coverage, no gap).
        assert!(!reg.try_reserve_pair("p1", 7, "review"));
        // The reservation was CONSUMED, not double-counted: exactly one active pair.
        let pairs = reg.active_pairs("p1");
        assert_eq!(
            pairs
                .iter()
                .filter(|p| **p == (7, "review".to_string()))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn concurrent_reservations_admit_exactly_one() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // The end-to-end invariant the finding asks for: N concurrent dispatches for
        // the SAME (pr, kind) → exactly ONE reserves (and so exactly one would start).
        // `SessionRegistry: Clone` shares the `Arc<Mutex>`, faithful to production.
        let reg = SessionRegistry::default();
        let winners = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..32 {
            let reg = reg.clone();
            let winners = Arc::clone(&winners);
            handles.push(tokio::spawn(async move {
                if reg.try_reserve_pair("p1", 7, "review") {
                    winners.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            winners.load(Ordering::SeqCst),
            1,
            "exactly one of N concurrent dispatches reserves the pair"
        );
    }

    #[test]
    fn begin_interrupt_is_atomic_and_idempotent() {
        let reg = SessionRegistry::default();
        reg.insert(SessionInfo {
            project_id: "p1".to_string(),
            thread_id: "t1".to_string(),
            turn_id: "tn1".to_string(),
            pr_number: 7,
            kind: "review".to_string(),
            status: SessionStatus::Running,
        });
        // Running → Proceed(turn_id), status flips to Interrupting.
        match reg.begin_interrupt("t1") {
            BeginInterrupt::Proceed(turn_id) => assert_eq!(turn_id, "tn1"),
            _ => panic!("a Running session must Proceed"),
        }
        assert_eq!(reg.list()[0].status, SessionStatus::Interrupting);
        // A repeat stop while Interrupting is an idempotent no-op.
        assert!(matches!(
            reg.begin_interrupt("t1"),
            BeginInterrupt::AlreadyHandled
        ));
        // Unknown id is an error.
        assert!(matches!(
            reg.begin_interrupt("missing"),
            BeginInterrupt::NotFound
        ));
    }

    #[test]
    fn rollback_interrupt_reverts_only_interrupting() {
        let reg = SessionRegistry::default();
        reg.insert(SessionInfo {
            project_id: "p1".to_string(),
            thread_id: "t1".to_string(),
            turn_id: "tn1".to_string(),
            pr_number: 7,
            kind: "review".to_string(),
            status: SessionStatus::Running,
        });
        reg.begin_interrupt("t1"); // → Interrupting
        reg.rollback_interrupt("t1"); // failed interrupt → back to Running (retry-able)
        assert_eq!(reg.list()[0].status, SessionStatus::Running);

        // A terminal status that raced in via the pump must NOT be reverted.
        reg.begin_interrupt("t1");
        reg.set_status("t1", SessionStatus::Done);
        reg.rollback_interrupt("t1");
        assert_eq!(reg.list()[0].status, SessionStatus::Done);
    }

    #[test]
    fn session_info_wire_shape_is_camel_case() {
        // Locks the contract with `src/review/types.ts` (Medium carrier). The
        // `projectId` routing key (#35) must serialize camelCase and mirror
        // `ReviewSession.projectId` on the TS side; snake_case must stay absent.
        let v = serde_json::to_value(SessionInfo {
            project_id: "p1".to_string(),
            thread_id: "t1".to_string(),
            turn_id: "tn1".to_string(),
            pr_number: 7,
            kind: "review".to_string(),
            status: SessionStatus::Running,
        })
        .expect("SessionInfo serializes");
        assert_eq!(v["projectId"], "p1");
        assert_eq!(v["threadId"], "t1");
        assert_eq!(v["turnId"], "tn1");
        assert_eq!(v["prNumber"], 7);
        // `kind` ("review"/"check") is a frontend contract field (mirrored by
        // `ReviewSession.kind` in `src/review/types.ts`); pin it so a rename / drop
        // surfaces here in lockstep with the camelCase keys.
        assert_eq!(v["kind"], "review");
        assert_eq!(v["status"], "running");
        assert!(v.get("project_id").is_none());
        assert!(v.get("thread_id").is_none());
    }

    #[test]
    fn session_status_wire_strings_are_pinned() {
        // Every variant's wire string is mirrored by `SessionStatus` in
        // `src/review/types.ts`; a rename here must be matched there (Medium lock).
        for (status, wire) in [
            (SessionStatus::Starting, "starting"),
            (SessionStatus::Running, "running"),
            (SessionStatus::Interrupting, "interrupting"),
            (SessionStatus::Done, "done"),
            (SessionStatus::Failed, "failed"),
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), wire);
        }
    }

    #[test]
    fn start_failure_marks_session_failed() {
        // The two-phase start: a session inserted as `Starting` is flipped to
        // `Failed` if the turn never starts (so it stays visible, not vanished).
        let reg = SessionRegistry::default();
        reg.insert(SessionInfo {
            project_id: "p1".to_string(),
            thread_id: "t1".to_string(),
            turn_id: String::new(),
            pr_number: 7,
            kind: "review".to_string(),
            status: SessionStatus::Starting,
        });
        reg.set_status("t1", SessionStatus::Failed);
        assert_eq!(reg.list()[0].status, SessionStatus::Failed);

        // The success path fills the turn id and flips to Running.
        reg.set_running("t1", "tn9".to_string());
        assert_eq!(reg.list()[0].turn_id, "tn9");
        assert_eq!(reg.list()[0].status, SessionStatus::Running);
    }
}
