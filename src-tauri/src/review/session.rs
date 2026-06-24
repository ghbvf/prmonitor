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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};
use tokio::sync::{broadcast, watch};

use super::engines::codex::process;
use super::engines::codex::protocol::{
    SandboxPolicy, ServerNotification, ThreadStartParams, TurnInterruptParams, TurnStartParams,
    UserInput,
};
use super::engines::codex::CodexManager;
use super::history_store::HistoryItemKind;
use crate::error::{AppError, AppResult};
use crate::events::{ReviewEvent, REVIEW_EVENT};
use crate::model::EngineKind;
use crate::review::engine::StartReviewOutcome;

/// A review session is identified by its codex `threadId`.
pub type ThreadId = String;

/// The result of resolving a `(project, pr, kind)` for a stop-review action (AB#1069 F4): the
/// three states an interrupt must tell apart, so a stop is never silently dropped.
///
/// - [`Live`](Self::Live): a promoted in-flight session — interrupt it by `thread_id`.
/// - [`Reserved`](Self::Reserved): a start is mid-flight ([`SessionRegistry::try_reserve_pair`] taken
///   but [`promote_reservation`](SessionRegistry::promote_reservation) not yet run), so there is no
///   `thread_id` to interrupt YET. The stop intent must NOT be dropped (it would let the start promote
///   to `Running` unimpeded) — the caller retries until it promotes to `Live` (then interrupt) or the
///   start fails (then [`Absent`](Self::Absent)).
/// - [`Absent`](Self::Absent): nothing in flight — the stop's intent ("no running turn") already
///   holds, so it is a benign idempotent success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopTarget {
    Live(ThreadId),
    Reserved,
    Absent,
}

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
    /// Engine that created this session. Follow-up chat must route back to this engine even
    /// if the project's current config changes later.
    pub engine_kind: EngineKind,
    pub status: SessionStatus,
    /// Wall-clock epoch seconds when the session was created (#70, review F10): the
    /// newest-first sort key the UI orders sessions by. Stamped at construction for a
    /// live session; the persisted `created_at` for a durable row. `threadId` is a UUID
    /// (no time), so the frontend sorting on it scrambled the list — this carries the
    /// real order (mirrors `ReviewSession.createdAtEpoch` in `src/review/types.ts`).
    pub created_at_epoch: u64,
    /// The resolved pr-review comment URL (AB#1042), filled by [`finalize_turn`] at a
    /// `completed` terminal (GitHub: exact comment URL; Azure: PR URL; else `None`).
    /// `None` for a non-terminal / non-completed session. Serializes camelCase
    /// `commentUrl`; `skip_serializing_if` omits the key when `None` so the wire matches
    /// an optional TS field rather than emitting a JSON `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment_url: Option<String>,
}

/// The terminal outcome of one review turn, delivered to a programmatic completion
/// subscriber (AB#1042). Carries the terminal [`SessionStatus`] plus the resolved
/// pr-review comment URL (if any) so a future transport can both "wait for this review"
/// AND read back the comment link without re-querying. `Clone` so the watch channel hands
/// each subscriber its own copy.
///
/// `status` is the terminal KIND ([`SessionStatus::Done`] / [`SessionStatus::Failed`]) —
/// but `Done` covers BOTH a successful `completed` and a user `interrupted` turn (see
/// [`terminal_status`]). `wire_status` carries the raw codex `turn.status` string
/// (`"completed"` / `"interrupted"` / `"failed"`, mirroring
/// [`crate::events::ReviewEvent::TurnCompleted`]'s `status`) so a subscriber CAN tell a
/// finished review from an interrupted one — which `status` alone cannot express.
#[derive(Debug, Clone)]
pub struct CompletionOutcome {
    pub status: SessionStatus,
    /// Raw codex `turn.status` — distinguishes `completed` vs `interrupted` (both map to
    /// `SessionStatus::Done`). See the struct doc.
    pub wire_status: String,
    pub comment_url: Option<String>,
}

/// The source context [`finalize_turn`] needs to resolve the pr-review comment URL (AB#1042),
/// captured IMMUTABLY at session creation. The funnel runs at the terminal — possibly minutes
/// after the review started — so it must NOT re-read the (mutable) project config there: a
/// config edit mid-review would otherwise yield a wrong/None URL (this bites the Azure path
/// especially, whose URL is built from `azure_org`/`azure_project`/`repo`). Snapshotting these
/// fields at start pins the answer to the project the review actually ran against.
///
/// Review-slice-internal: NOT serialized, NOT a DB column, NOT a wire type — it lives only in
/// the in-memory [`RegistryState::url_contexts`] map, so it touches no wire/DB contract.
/// `pub(crate)` (not `pub(super)`) ONLY because the auto-dispatch composition root
/// (`crate::lib::run_auto_dispatch`) builds the engines directly and sits OUTSIDE the `review`
/// module, so it must be able to name the type to set the engine's `url_ctx` field. It is
/// still crate-internal — never crosses the Tauri command boundary nor the DB.
#[derive(Debug, Clone)]
pub(crate) struct CommentUrlContext {
    pub source_kind: crate::model::SourceKind,
    pub repo: String,
    pub azure_org: String,
    pub azure_project: String,
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
    /// Per-`thread_id` completion broadcast (AB#1042): a `watch::Sender` whose value goes
    /// `None` → `Some(CompletionOutcome)` exactly once, when the turn reaches a terminal
    /// state via [`finalize_turn`]. Get-or-create on BOTH ends ([`SessionRegistry::subscribe_completion`]
    /// / [`SessionRegistry::signal_completion`]) so a subscriber that arrives before the
    /// signal still observes the retained terminal value (watch keeps the last value), and
    /// a signal that fires before anyone subscribes is not lost. Lives under the SAME mutex
    /// as `sessions` — `watch` send/subscribe are synchronous, so no `.await` is held under
    /// the lock (the registry's invariant). Entries are intentionally retained for the
    /// process lifetime (a session count is bounded by usage; no churn that warrants GC).
    completions: HashMap<ThreadId, watch::Sender<Option<CompletionOutcome>>>,
    /// Per-`thread_id` source context for the comment-URL resolve (AB#1042), captured
    /// IMMUTABLY at session creation in the SAME critical section as the `sessions` insert
    /// ([`SessionRegistry::promote_reservation`]) — so it is present before any pump can
    /// finalize. [`finalize_turn`] reads it via [`SessionRegistry::take_url_context`] (remove +
    /// return), which bounds the map and pins that the URL is resolved against the project the
    /// review STARTED against, never a config that changed mid-review. In-memory only.
    url_contexts: HashMap<ThreadId, CommentUrlContext>,
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

/// Outcome of [`SessionRegistry::begin_resume`] — the atomic guard that makes a chat
/// follow-up (`send_message`) race-free: only a TERMINAL (`Done`/`Failed`) session may be
/// resumed into a new `Running` turn, so two concurrent sends can't both proceed.
pub(super) enum BeginResume {
    /// Was terminal (`Done`/`Failed`); flipped to `Running` — the caller now owns the
    /// follow-up turn for this session.
    Proceed,
    /// Still `Starting`/`Running`/`Interrupting` — a turn is already in flight, so a
    /// follow-up must wait (rejected, not started). Prevents a concurrent double-send.
    Busy,
    /// No session with this id in the in-memory registry (the caller rehydrates from the
    /// durable row first, so this means it truly does not exist).
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

    /// `pub(super)` so the claude orchestration (`engines::claude`) can drive the
    /// same lifecycle transitions as the codex `start_review`/`pump` — the dedup
    /// registry is engine-agnostic and reused, NOT duplicated (#718). Kept
    /// `pub(super)` (not `pub`) to respect the review slice boundary.
    pub(super) fn set_status(&self, thread_id: &str, status: SessionStatus) {
        if let Some(info) = self.inner.lock().unwrap().sessions.get_mut(thread_id) {
            info.status = status;
        }
    }

    /// Terminal mutator for [`finalize_turn`] (AB#1042): set the status AND the resolved
    /// `comment_url` in one critical section, so a `list_review_sessions` snapshot taken
    /// after finalize sees both. Kept a dumb mutator (no signal — the funnel signals LAST,
    /// after the durable write); `set_status` stays for the non-terminal callers.
    pub(super) fn set_status_and_comment_url(
        &self,
        thread_id: &str,
        status: SessionStatus,
        comment_url: Option<String>,
    ) {
        if let Some(info) = self.inner.lock().unwrap().sessions.get_mut(thread_id) {
            info.status = status;
            info.comment_url = comment_url;
        }
    }

    /// Record the turn id and flip to [`SessionStatus::Running`] once the engine has
    /// confirmed the session is live (the session was inserted as `Starting` first).
    /// `pub(super)` for the reused claude orchestration (#718).
    pub(super) fn set_running(&self, thread_id: &str, turn_id: String) {
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
    /// `pub(super)` so the claude orchestration's [`ReservationGuard`] analogue can
    /// release on an early failure (#718).
    pub(super) fn release_pair(&self, project_id: &str, pr_number: u64, kind: &str) {
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
    /// `pub(super)` so the claude orchestration hands its reservation to a `Starting`
    /// session through the same gap-free swap the codex path uses (#718).
    ///
    /// `url_ctx` is the IMMUTABLE comment-URL source context (AB#1042), inserted into
    /// `url_contexts` keyed by the session's `thread_id` IN THIS SAME critical section as the
    /// session insert — so the snapshot is atomic with session creation and present before the
    /// pump can finalize. [`finalize_turn`] reads it once via [`Self::take_url_context`].
    pub(super) fn promote_reservation(&self, info: SessionInfo, url_ctx: CommentUrlContext) {
        let mut st = self.inner.lock().unwrap();
        st.reserved
            .remove(&(info.project_id.clone(), info.pr_number, info.kind.clone()));
        st.url_contexts.insert(info.thread_id.clone(), url_ctx);
        st.sessions.insert(info.thread_id.clone(), info);
    }

    /// Remove and return this `thread_id`'s captured [`CommentUrlContext`] (AB#1042). Called
    /// EXACTLY ONCE by [`finalize_turn`] at the terminal — remove-on-read bounds the map (a
    /// finalized session needs the context no more) and pins that the URL is resolved against
    /// the start-time snapshot, independent of any later config change. A second take (or a
    /// session that never captured one) yields `None`. Synchronous (no `.await` under the lock).
    pub(super) fn take_url_context(&self, thread_id: &str) -> Option<CommentUrlContext> {
        self.inner.lock().unwrap().url_contexts.remove(thread_id)
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

    /// Atomically begin a chat follow-up (`send_message`). Only a TERMINAL
    /// (`Done`/`Failed`) session flips to [`SessionStatus::Running`] and yields
    /// [`BeginResume::Proceed`]; a still-`Starting`/`Running`/`Interrupting` session is
    /// [`BeginResume::Busy`] (a turn is in flight — a concurrent double-send is rejected,
    /// never double-started); a missing session is [`BeginResume::NotFound`]. The
    /// check-and-set is one synchronous critical section (mirrors [`Self::begin_interrupt`]),
    /// so two concurrent follow-ups can't both proceed. The caller must
    /// [`Self::rehydrate`] a durable terminal row into the registry BEFORE this if the app
    /// restarted (the in-memory map is empty then) — so a `NotFound` here means the session
    /// genuinely does not exist.
    pub(super) fn begin_resume(&self, thread_id: &str) -> BeginResume {
        let mut st = self.inner.lock().unwrap();
        match st.sessions.get_mut(thread_id) {
            None => BeginResume::NotFound,
            Some(info) => match info.status {
                SessionStatus::Done | SessionStatus::Failed => {
                    info.status = SessionStatus::Running;
                    BeginResume::Proceed
                }
                _ => BeginResume::Busy,
            },
        }
    }

    /// Re-insert a durable session row into the in-memory registry, so a follow-up
    /// (`send_message`) after an app restart — when the registry is empty but a durable
    /// `Done` row exists — can drive the SAME lifecycle transitions (`begin_resume` →
    /// `set_running` → `finalize_turn`) the live path uses. Reuses the same insert path as
    /// [`Self::promote_reservation`] (sessions + a fresh [`CommentUrlContext`]): the first
    /// turn's `finalize_turn` consumed the original context via [`Self::take_url_context`],
    /// so a follow-up turn needs its own re-inserted context to resolve cleanly. A no-op
    /// overwrite if the session is somehow already present (the live path never rehydrates).
    pub(super) fn rehydrate(&self, info: SessionInfo, url_ctx: CommentUrlContext) {
        let mut st = self.inner.lock().unwrap();
        st.url_contexts.insert(info.thread_id.clone(), url_ctx);
        st.sessions.insert(info.thread_id.clone(), info);
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

    /// Snapshot ONE in-memory session by its `thread_id` (AB#1043: the local REST API's
    /// `GET /reviews/{id}` lookup). In-memory only — a session that finished before a
    /// restart is no longer here; the caller falls through to the durable by-id read
    /// (`history_store::get_session`). `pub` (sibling `local_api` module reads it, like
    /// `list`). Synchronous: clones under the lock, no `.await`.
    pub fn get(&self, thread_id: &str) -> Option<SessionInfo> {
        self.inner.lock().unwrap().sessions.get(thread_id).cloned()
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

    /// Resolve `(project_id, pr_number, kind)` to a [`StopTarget`] for the outbox stop-review action
    /// (AB#1069 F4): a [`Live`](StopTarget::Live) in-flight session (interrupt by `thread_id`), a bare
    /// [`Reserved`](StopTarget::Reserved) start mid-flight (no `thread_id` yet — the caller must retry
    /// so the stop is not dropped), or [`Absent`](StopTarget::Absent) (nothing to stop — idempotent).
    ///
    /// The inverse of [`Self::active_pairs`] (which drops the thread id and lumps reservations in with
    /// sessions). A `Live` session takes priority over a reservation for the same triple — the
    /// promote-into-session swap is atomic under this same lock, so the two never both register, but
    /// checking sessions first is the correct precedence. Project-scoped (#35). Synchronous (no
    /// `.await` under the lock).
    pub fn stop_target(&self, project_id: &str, pr_number: u64, kind: &str) -> StopTarget {
        let st = self.inner.lock().unwrap();
        if let Some(s) = st.sessions.values().find(|s| {
            s.project_id == project_id
                && s.pr_number == pr_number
                && s.kind == kind
                && matches!(
                    s.status,
                    SessionStatus::Starting | SessionStatus::Running | SessionStatus::Interrupting
                )
        }) {
            return StopTarget::Live(s.thread_id.clone());
        }
        // A reserved-but-not-yet-promoted triple: a start is mid-flight with no `thread_id` to
        // interrupt yet. NOT `Absent` — dropping the stop here would let the start promote unimpeded.
        if st
            .reserved
            .contains(&(project_id.to_string(), pr_number, kind.to_string()))
        {
            return StopTarget::Reserved;
        }
        StopTarget::Absent
    }

    /// Subscribe to this `thread_id`'s terminal completion (AB#1042). Returns a
    /// `watch::Receiver` whose value is `None` until the turn finalizes, then the retained
    /// `Some(CompletionOutcome)`. GET-OR-CREATE (a fresh sender starts at `None`): a
    /// subscriber that arrives AFTER the signal still reads the last value the watch keeps,
    /// so there is no "subscribed too late" race. Synchronous (no `.await` under the lock).
    pub fn subscribe_completion(
        &self,
        thread_id: &str,
    ) -> watch::Receiver<Option<CompletionOutcome>> {
        let mut st = self.inner.lock().unwrap();
        st.completions
            .entry(thread_id.to_string())
            .or_insert_with(|| watch::channel(None).0)
            .subscribe()
    }

    /// Signal this `thread_id`'s terminal completion (AB#1042) by setting the watch value to
    /// `Some(outcome)`. GET-OR-CREATE so a signal that fires before anyone subscribed is not
    /// lost (a later `subscribe_completion` reads the retained value). Called LAST by
    /// [`finalize_turn`], after the registry/DB terminal writes have landed, so any woken
    /// subscriber sees a fully-settled session. `pub(super)` — only the funnel signals.
    /// Synchronous (no `.await` under the lock).
    pub(super) fn signal_completion(&self, thread_id: &str, outcome: CompletionOutcome) {
        let mut st = self.inner.lock().unwrap();
        let sender = st
            .completions
            .entry(thread_id.to_string())
            .or_insert_with(|| watch::channel(None).0);
        // `send_replace` (NOT `send`) sets the value UNCONDITIONALLY — `send` would fail and
        // DISCARD the value when there are no receivers yet (signal-before-subscribe), losing
        // the outcome. `send_replace` retains it (and notifies any existing receivers), so a
        // later `subscribe_completion` reads the retained terminal value.
        let _ = sender.send_replace(Some(outcome));
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
///
/// `pub(crate)` (not `pub`): only the in-crate codex engine adapter calls it, and its
/// `url_ctx: CommentUrlContext` param is a crate-internal type — keeping both at crate
/// visibility makes the interface consistent (no `private_interfaces` leak).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_review<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    codex: &CodexManager,
    registry: &SessionRegistry,
    codex_bin: &str,
    repo: &str,
    repo_root: &str,
    skill_abs_path: &str,
    codex_model: &str,
    project_id: &str,
    pr_number: u64,
    kind: &str,
    // The IMMUTABLE comment-URL source context (AB#1042), captured by the caller from the
    // project at start. Handed to the `Starting` session in `promote_reservation` so the
    // terminal `finalize_turn` resolves the URL against the project the review ran against.
    url_ctx: CommentUrlContext,
    // AB#1204 outbox claim id: `Some(outbox_id)` ONLY on the outbox executor's start path, `None`
    // otherwise. When `Some`, the claim's `thread_id` breadcrumb is written right after
    // `thread/start` (below) — before `start_turn` runs the turn / posts a `pm:` comment.
    outbox_claim_id: Option<i64>,
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
        engine_kind: EngineKind::Codex,
        status: SessionStatus::Starting,
        created_at_epoch: super::history_store::now_epoch(),
        // No comment yet — filled by `finalize_turn` at a `completed` terminal (AB#1042).
        comment_url: None,
    };
    registry.promote_reservation(starting.clone(), url_ctx);
    // Mirror the in-memory session into the durable `review_session` table (#70) so this
    // PR's session list survives a restart and its history can be reopened. Best-effort.
    persist_session(app, &starting);
    reservation.disarm();

    // F1 (AB#1204): write the outbox claim's thread_id breadcrumb HERE — right after thread/start
    // yields a stable thread_id and the Starting session is persisted, but BEFORE `start_turn`
    // lets the turn run / post a `pm:` comment. This closes the window where a crash between the
    // turn starting and the (former) post-return attach left the claim NULL → replay duplicated.
    // Best-effort: a failure only narrows back toward the pre-AB#1204 window (no regression) and
    // must NOT fail the started review.
    if let Some(outbox_id) = outbox_claim_id {
        let db = app.state::<crate::db::Database>();
        if let Err(e) = super::claim_store::attach_thread(db.inner(), outbox_id, &thread_id) {
            eprintln!(
                "outbox review claim：记录 thread_id 失败（outbox_id={outbox_id}）：{}",
                e.message
            );
        }
    }

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
            // Per-turn model override: blank config → None → codex's configured default.
            // Trim to match the emptiness check — a padded name must not reach the RPC
            // with surrounding whitespace (e.g. `{"model":"  gpt-5.1-codex  "}`).
            model: (!codex_model.trim().is_empty()).then(|| codex_model.trim().to_string()),
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
            engine_kind: EngineKind::Codex,
            status: SessionStatus::Running,
            // Same creation instant as the `Starting` row above — `upsert_session` keys
            // `created_at` on first insert (ON CONFLICT preserves it), so this only needs
            // to stay consistent with `starting`, not re-stamp `now`.
            created_at_epoch: starting.created_at_epoch,
            // Still no comment at the Running transition (AB#1042).
            comment_url: None,
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
        pr_number,
        thread_id.clone(),
        app.clone(),
        registry.clone(),
    ));

    Ok(StartReviewOutcome::Started(thread_id))
}

/// Continue an EXISTING codex session with a follow-up user `message` (chat continuation):
/// issue a SECOND `turn/start` on the SAME `thread_id` and stream the reply through the
/// existing [`pump`], reusing the whole `ReviewEvent` pipeline. Sibling of [`start_review`].
///
/// Flow (mirrors `start_review`'s two-phase shape, adapted to a follow-up):
/// 1. `begin_resume` guard — rehydrate the durable terminal row first if the session is not
///    in the in-memory registry (after a restart the registry is empty but a `Done` row may
///    exist). A `Busy` (a turn already in flight) or a genuine `NotFound` is an error.
/// 2. `codex.connection` (the command already called `state.codex.resume()`).
/// 3. `subscribe()` BEFORE issuing the turn (no early delta missed, same as `start_review`).
/// 4. Persist the user message to history BEFORE the turn (ordered before the reply).
/// 5. A second `turn/start` on the EXISTING `thread_id` — the pr-review skill is NOT
///    re-attached (it is already in the thread context); only the raw user `message` is sent.
/// 6. On `start_turn` error: set the session `Failed` + persist + return a clear error.
/// 7. On success: `set_running` + persist Running + spawn the existing `pump`.
///
/// `pub(crate)` (not `pub`) like `start_review`: only the in-crate codex engine adapter
/// calls it, and its `url_ctx: CommentUrlContext` param is a crate-internal type.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resume_turn<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    codex: &CodexManager,
    registry: &SessionRegistry,
    codex_bin: &str,
    repo_root: &str,
    codex_model: &str,
    project_id: &str,
    pr_number: u64,
    durable_info: &SessionInfo,
    thread_id: &str,
    message: &str,
    user_item_id: &str,
    // The IMMUTABLE comment-URL source context (AB#1042), captured by the caller from the
    // project. Re-inserted via `rehydrate` (the first turn's `finalize_turn` consumed the
    // original) so this follow-up turn's `finalize_turn` resolves the URL cleanly.
    url_ctx: CommentUrlContext,
) -> AppResult<()> {
    // Rehydrate a durable terminal row into the registry if it isn't live (the registry is
    // empty after a restart, but a `Done`/`Failed` row may persist). The durable status is
    // terminal (or `fail_orphaned_sessions` made it so at startup), so `begin_resume` below
    // accepts it. If neither the registry NOR the durable row has it, `begin_resume` returns
    // NotFound. The caller resolved `pr_number`/`project_id` from the same row, so the
    // rehydrated `SessionInfo` is consistent with the durable record.
    if registry.get(thread_id).is_none() {
        registry.rehydrate(durable_info.clone(), url_ctx.clone());
    }

    // Atomic guard: only a terminal session flips to `Running` and proceeds. A turn already
    // in flight (`Busy`) or a genuinely absent session is rejected before any side effect.
    match registry.begin_resume(thread_id) {
        BeginResume::Proceed => {}
        BeginResume::Busy => {
            return Err(AppError::new(format!(
                "review 会话仍在进行中，无法续聊: {thread_id}"
            )))
        }
        BeginResume::NotFound => {
            return Err(AppError::new(format!("未找到 review 会话: {thread_id}")))
        }
    }

    // The command already called `state.codex.resume()`; `connection` spawns/reuses the
    // resident app-server. A connection failure leaves the session `Running` — flip it back
    // to `Failed` so it isn't stuck, then surface the error.
    let client = match codex.connection(codex_bin, repo_root).await {
        Ok(client) => client,
        Err(e) => {
            registry.set_status(thread_id, SessionStatus::Failed);
            persist_status(app, thread_id, SessionStatus::Failed);
            // On a failure path `finalize_turn` never runs, so it never consumes the URL
            // context the (possibly just-)`rehydrate`d session inserted — discard it so it
            // doesn't leak in `url_contexts`. A no-op `None` when this run never rehydrated
            // (the same-run path keeps the original until its own `finalize_turn` takes it).
            let _ = registry.take_url_context(thread_id);
            return Err(e);
        }
    };

    // Subscribe before the turn so the buffered broadcast yields every delta from turn start.
    let rx = client.subscribe();

    // Persist the user's typed message to history BEFORE issuing the turn, under the
    // CALLER-supplied `user_item_id` (so the frontend's optimistic bubble id == the
    // persisted id, and reopen-dedup works). History is ordered by rowid (`ORDER BY h.id`),
    // so inserting this row first places the user message before the reply. Best-effort
    // (logged + swallowed), the same contract as the delta persistence.
    persist_user_message(app, project_id, thread_id, user_item_id, message);

    // Issue a SECOND turn on the EXISTING thread. The pr-review skill is NOT re-attached —
    // it is already in this thread's context; we send only the raw user message. This is a
    // chat-answer turn, not an unattended code-action turn: keep the workspace read-only and
    // network disabled so a follow-up cannot modify files or reach external services. If a
    // user wants code changes, they should trigger the `/fix` workflow explicitly.
    let turn_id = match process::start_turn(
        &client,
        TurnStartParams {
            thread_id: thread_id.to_string(),
            input: vec![UserInput::Text {
                text: message.to_string(),
            }],
            approval_policy: "never".to_string(),
            sandbox_policy: SandboxPolicy {
                kind: "readOnly".to_string(),
                network_access: false,
                writable_roots: Vec::new(),
            },
            cwd: Some(repo_root.to_string()),
            model: (!codex_model.trim().is_empty()).then(|| codex_model.trim().to_string()),
        },
    )
    .await
    {
        Ok(turn_id) => turn_id,
        // KNOWN LIMITATION (not a bug): codex follow-up works only WITHIN the same app run.
        // The resident app-server keeps the thread in memory; the codex app-server protocol
        // has NO `thread/resume`, so after an app restart the thread is gone and this
        // `turn/start` fails. Mark the session `Failed` (so it isn't stuck `Running`) and
        // surface a clear, user-facing error directing them to re-start a review. (Claude
        // works cross-restart via `--resume` — only codex has this limit.)
        Err(_) => {
            registry.set_status(thread_id, SessionStatus::Failed);
            persist_status(app, thread_id, SessionStatus::Failed);
            // No `finalize_turn` runs on this failure, so discard the rehydrated URL context
            // to avoid leaking it in `url_contexts` (no-op `None` if never rehydrated).
            let _ = registry.take_url_context(thread_id);
            return Err(AppError::new(
                "codex 线程已失效（应用重启后无法续聊，请重新发起 review）".to_string(),
            ));
        }
    };

    registry.set_running(thread_id, turn_id.clone());
    // Mirror the Running transition into `review_session`. `upsert_session` keys `created_at`
    // on first insert (ON CONFLICT preserves it), so re-stamping `now` here is harmless.
    let live = registry
        .get(thread_id)
        .map(|info| SessionInfo {
            status: SessionStatus::Running,
            turn_id: turn_id.clone(),
            ..info
        })
        .unwrap_or_else(|| SessionInfo {
            project_id: project_id.to_string(),
            thread_id: thread_id.to_string(),
            turn_id,
            pr_number,
            kind: durable_info.kind.clone(),
            engine_kind: durable_info.engine_kind,
            status: SessionStatus::Running,
            created_at_epoch: durable_info.created_at_epoch,
            comment_url: durable_info.comment_url.clone(),
        });
    persist_session(app, &live);

    // Spawn the SAME pump as `start_review` — same signature/usage — to stream the reply.
    tauri::async_runtime::spawn(pump(
        rx,
        project_id.to_string(),
        pr_number,
        thread_id.to_string(),
        app.clone(),
        registry.clone(),
    ));

    Ok(())
}

/// Best-effort persist of a USER's follow-up chat message into the session history
/// (chat continuation), under the caller-supplied `user_item_id`. Logged + swallowed like
/// the delta persistence — a DB hiccup must not block the follow-up turn. Shared by the
/// codex `resume_turn` here and (via `pub(super)`) the claude `resume_review`, so both
/// engines persist the user message through ONE helper with the same one-time-notice
/// contract.
pub(super) fn persist_user_message<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: &str,
    thread_id: &str,
    user_item_id: &str,
    message: &str,
) {
    let db = app.state::<crate::db::Database>();
    if let Err(e) = super::history_store::append_item(
        db.inner(),
        thread_id,
        user_item_id,
        HistoryItemKind::User,
        message,
    ) {
        eprintln!(
            "review 用户消息持久化失败（{thread_id}/{user_item_id}）：{}",
            e.message
        );
        notify_persist_failure_once(app, project_id);
    }
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

/// Process-global guard for the one-time persistence-failure notice (review F9): a broken
/// DB would otherwise fire a banner on EVERY swallowed persist error (one per delta). The
/// first failure flips this and emits a single app-level notice; later failures only log.
/// Static (not per-instance) because the app runs once per process and the notice is
/// informational — there is no reset point to model.
static PERSIST_FAILURE_NOTIFIED: AtomicBool = AtomicBool::new(false);

/// Surface the FIRST review-persistence failure to the user once (review F9). The `persist_*`
/// helpers are best-effort (log + swallow), so a failing DB silently stops saving session
/// history — invisible on a desktop where nobody reads the console. This emits one app-level
/// [`ReviewEvent::DispatchError`] (the existing availability-banner channel, which already
/// covers background write failures) so the user learns their history may not survive a
/// restart. Subsequent failures only log, so a broken DB never spams a notice per delta.
///
/// `pub(super)` so the claude engine (`engines::claude`) reuses the SAME one-time notice
/// on its own persist failures (#718) — a single process-global notice across BOTH engines
/// is correct (the static `PERSIST_FAILURE_NOTIFIED` is shared, not per-engine), so a
/// broken DB still raises exactly one banner regardless of which engine hit it first.
pub(super) fn notify_persist_failure_once<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: &str,
) {
    if PERSIST_FAILURE_NOTIFIED.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = app.emit(
        REVIEW_EVENT,
        &ReviewEvent::DispatchError {
            project_id: project_id.to_string(),
            message: "review 会话持久化失败——重启后历史可能丢失（请检查磁盘空间 / 数据库文件权限）。后续失败仅记录日志。"
                .to_string(),
        },
    );
}

/// Best-effort mirror of an in-memory [`SessionInfo`] into the durable `review_session`
/// table (#70). Logs + swallows errors: a persistence hiccup must never break the live
/// session (the in-memory registry stays the authority for dedup / status). The first
/// failure also raises a one-time user-facing notice (review F9).
fn persist_session<R: tauri::Runtime>(app: &tauri::AppHandle<R>, info: &SessionInfo) {
    let db = app.state::<crate::db::Database>();
    if let Err(e) = super::history_store::upsert_session(db.inner(), info) {
        eprintln!(
            "review session 持久化失败（{}）：{}",
            info.thread_id, e.message
        );
        notify_persist_failure_once(app, &info.project_id);
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
    let (project_id, item_id, kind, text) = match event {
        ReviewEvent::MessageDelta {
            project_id,
            item_id,
            text,
            ..
        } => (project_id, item_id, HistoryItemKind::Message, text),
        ReviewEvent::ReasoningDelta {
            project_id,
            item_id,
            text,
            ..
        } => (project_id, item_id, HistoryItemKind::Reasoning, text),
        _ => return,
    };
    let db = app.state::<crate::db::Database>();
    if let Err(e) = super::history_store::append_item(db.inner(), thread_id, item_id, kind, text) {
        eprintln!(
            "review history 持久化失败（{thread_id}/{item_id}）：{}",
            e.message
        );
        notify_persist_failure_once(app, project_id);
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
    pr_number: u64,
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
                fail_connection_closed(&registry, &app, &project_id, pr_number, &thread_id).await;
                break;
            }
            Ok(note) => {
                // PEEK the raw terminal notification BEFORE mapping (AB#1042): the terminal
                // path must run the async `finalize_turn` (resolve URL, signal completion),
                // which the pure `map_notification` cannot do. Only THIS session's
                // `turn/completed` is terminal; another thread's is not ours.
                if let ServerNotification::TurnCompleted(d) = note.as_ref() {
                    if d.thread_id == thread_id {
                        let wire_status = d.turn.status.clone();
                        let terminal = terminal_status(&wire_status);
                        finalize_turn(
                            &app,
                            &registry,
                            &project_id,
                            pr_number,
                            &thread_id,
                            terminal,
                            &wire_status,
                            None,
                        )
                        .await;
                        break; // terminal — the turn is over.
                    }
                    // Another session's completion → not ours; keep pumping.
                    continue;
                }
                let Some(event) = map_notification(&note, &project_id, &thread_id) else {
                    continue;
                };
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
            // Failed terminal + error rather than risk a stuck `Running`. Routed
            // through `finalize_turn` (AB#1042) so a completion subscriber is signalled
            // (no URL on a lag-failure — the turn never reported `completed`).
            Err(broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("review pump（{thread_id}）滞后，丢弃 {n} 条通知");
                finalize_turn(
                    &app,
                    &registry,
                    &project_id,
                    pr_number,
                    &thread_id,
                    SessionStatus::Failed,
                    "failed",
                    Some(format!("codex 输出流滞后，丢弃 {n} 条消息（review 中断）")),
                )
                .await;
                break;
            }
            // The broadcast itself closed (every `Sender` dropped — i.e. the whole
            // `RpcClient` was torn down, e.g. manager shutdown). Same terminal
            // outcome as the synthetic `ConnectionClosed` above.
            Err(broadcast::error::RecvError::Closed) => {
                fail_connection_closed(&registry, &app, &project_id, pr_number, &thread_id).await;
                break;
            }
        }
    }
}

/// End a session as `Failed` with a "connection closed" error event, through the terminal
/// funnel (AB#1042). Shared by the pump's two transport-teardown paths: the synthetic
/// `ConnectionClosed` (reader exited but the `RpcClient` lives on) and `RecvError::Closed`
/// (the whole client dropped). `async` (it routes through `finalize_turn`); safe — no lock
/// is held across the await (`AppHandle: Send+Sync`, `SessionRegistry: Clone(Arc)`). No URL
/// is resolved (the connection died — the turn never reported `completed`).
async fn fail_connection_closed<R: tauri::Runtime>(
    registry: &SessionRegistry,
    app: &tauri::AppHandle<R>,
    project_id: &str,
    pr_number: u64,
    thread_id: &str,
) {
    finalize_turn(
        app,
        registry,
        project_id,
        pr_number,
        thread_id,
        SessionStatus::Failed,
        "failed",
        Some("codex 连接已关闭".to_string()),
    )
    .await;
}

/// The single terminal funnel for a review turn (AB#1042) — every terminal path (both
/// engines' pump exit points) converges here so the completion writes, the wire
/// `TurnCompleted`, and the programmatic completion signal happen in ONE fixed order.
/// Engine-agnostic (no `CodexManager`/`ClaudeManager` param): the codex pump's
/// `TurnCompleted` / lag / connection-closed exits and the claude `finish`'s exits all
/// call it; per-engine cleanup (claude's `deregister`) stays at the callsite, OUTSIDE the
/// funnel.
///
/// Steps (ORDER IS LOAD-BEARING — `signal_completion` MUST be last):
/// 1. Resolve the comment URL ONLY for a `completed` turn: read the IMMUTABLE
///    [`CommentUrlContext`] captured at session start (`registry.take_url_context`) for
///    repo/source_kind/azure org+project, then [`super::comment_url::resolve_comment_url`].
///    Reading the start-time snapshot (NOT the live, mutable config) is what makes the URL
///    correct even when the config was edited during a long review. ANY miss (no captured
///    context, gh error) degrades to `None` — the funnel NEVER fails (mirrors the best-effort
///    persist contract). interrupted / failed → `None` (no comment was posted).
/// 2. Set the in-memory terminal status AND write the resolved `comment_url` onto the
///    session (`set_status` stays a dumb mutator; the URL write is folded in here).
/// 3. Persist the terminal status + URL atomically (best-effort, the one-time notice on
///    failure), so a woken subscriber that reads the DB sees the settled row.
/// 4. Emit the optional `Error` event, then the terminal `TurnCompleted { comment_url }`.
/// 5. Signal the completion watch LAST — a subscriber woken by it then reads the registry
///    / DB and is guaranteed to see the already-landed terminal state + URL.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize_turn<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    registry: &SessionRegistry,
    project_id: &str,
    pr_number: u64,
    thread_id: &str,
    terminal: SessionStatus,
    wire_status: &str,
    error: Option<String>,
) {
    // 1. Resolve the comment URL only on a successful completion, against the IMMUTABLE
    // context captured at session start (NOT the live config) so a mid-review config edit
    // can't yield a wrong/None URL. Take it unconditionally (remove-on-read bounds the map);
    // a missing context or a non-`completed` terminal both yield None. Any resolve failure
    // also degrades to None — the funnel never fails.
    let comment_url = match registry.take_url_context(thread_id) {
        Some(ctx) if should_resolve_url(wire_status) => {
            super::comment_url::resolve_comment_url(
                ctx.source_kind,
                &ctx.repo,
                &ctx.azure_org,
                &ctx.azure_project,
                pr_number,
            )
            .await
        }
        _ => None,
    };

    // 2. In-memory terminal status + the resolved URL.
    registry.set_status_and_comment_url(thread_id, terminal, comment_url.clone());

    // 3. Durable terminal status + URL (best-effort; one-time notice on failure).
    let db = app.state::<crate::db::Database>();
    if let Err(e) = super::history_store::set_status_and_comment_url(
        db.inner(),
        thread_id,
        terminal,
        comment_url.as_deref(),
    ) {
        eprintln!(
            "review session 终态持久化失败（{thread_id}）：{}",
            e.message
        );
        notify_persist_failure_once(app, project_id);
    }

    // 4. Optional Error, then the terminal TurnCompleted carrying the URL.
    if let Some(message) = error {
        let _ = app.emit(
            REVIEW_EVENT,
            &ReviewEvent::Error {
                project_id: project_id.to_string(),
                thread_id: thread_id.to_string(),
                message,
            },
        );
    }
    let _ = app.emit(
        REVIEW_EVENT,
        &ReviewEvent::TurnCompleted {
            project_id: project_id.to_string(),
            thread_id: thread_id.to_string(),
            status: wire_status.to_string(),
            comment_url: comment_url.clone(),
        },
    );

    // 5. Signal LAST — subscribers wake to an already-settled registry/DB.
    registry.signal_completion(
        thread_id,
        CompletionOutcome {
            status: terminal,
            // The raw codex status so a subscriber distinguishes completed vs interrupted
            // (both terminal-map to `Done`) — `wire_status` is in scope here as `&str`.
            wire_status: wire_status.to_string(),
            comment_url,
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
        // Retained for the pure-mapping unit test (AB#1042): the PRODUCTION pump peeks the
        // raw `TurnCompleted` BEFORE calling `map_notification` and routes it through the
        // async `finalize_turn` (which fills `comment_url`), so this arm is never hit live —
        // the `comment_url: None` here only matters to the characterization test.
        ServerNotification::TurnCompleted(d) if d.thread_id == thread_id => {
            Some(ReviewEvent::TurnCompleted {
                project_id: project_id.to_string(),
                thread_id: d.thread_id.clone(),
                status: d.turn.status.clone(),
                comment_url: None,
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

/// Whether [`finalize_turn`] resolves a comment URL for this terminal — TRUE only for a
/// `completed` turn (AB#1042). An `interrupted` turn maps to [`SessionStatus::Done`] (same
/// as `completed`) yet posted no comment, so the gate is on the raw `wire_status`, NOT on
/// the terminal [`SessionStatus`] — pinning that an interrupted/failed turn resolves NO URL.
fn should_resolve_url(wire_status: &str) -> bool {
    wire_status == "completed"
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

    /// A throwaway [`CommentUrlContext`] for `promote_reservation` calls whose tests do not
    /// assert on the captured value (AB#1042). The capture-roundtrip test below builds its
    /// own distinguishable context instead.
    fn test_url_ctx() -> CommentUrlContext {
        CommentUrlContext {
            source_kind: crate::model::SourceKind::Github,
            repo: "owner/name".to_string(),
            azure_org: String::new(),
            azure_project: String::new(),
        }
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
    fn should_resolve_url_only_for_completed() {
        // Only a `completed` turn posted a comment to resolve. `interrupted` maps to the
        // SAME terminal `Done` as `completed`, so the gate must key on the raw wire status,
        // not the terminal kind — an interrupted/failed/empty turn resolves NO url.
        assert!(should_resolve_url("completed"));
        assert!(!should_resolve_url("interrupted"));
        assert!(!should_resolve_url("failed"));
        assert!(!should_resolve_url(""));
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
            engine_kind: EngineKind::Codex,
            status: SessionStatus::Running,
            created_at_epoch: 0,
            comment_url: None,
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
                engine_kind: EngineKind::Codex,
                status,
                created_at_epoch: 0,
                comment_url: None,
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
    fn stop_target_resolves_live_terminal_and_absent() {
        // AB#1069 F4: the stop-review executor's reverse lookup. A live in-flight session → Live(id);
        // terminal / absent / wrong-project → Absent (the idempotency hinge — Absent → the executor
        // no-ops the stop instead of erroring).
        let reg = SessionRegistry::default();
        let info = |thread: &str, project: &str, pr: u64, kind: &str, status| {
            reg.insert(SessionInfo {
                project_id: project.to_string(),
                thread_id: thread.to_string(),
                turn_id: String::new(),
                pr_number: pr,
                kind: kind.to_string(),
                engine_kind: EngineKind::Codex,
                status,
                created_at_epoch: 0,
                comment_url: None,
            });
        };
        info("a", "p1", 1, "review", SessionStatus::Running);
        info("b", "p1", 1, "check", SessionStatus::Starting); // same PR, different kind
        info("c", "p1", 2, "review", SessionStatus::Done); // terminal → not stoppable

        // In-flight pair → Live(thread_id).
        assert_eq!(
            reg.stop_target("p1", 1, "review"),
            StopTarget::Live("a".to_string())
        );
        // Kind is part of the key — review and check for the same PR are independent sessions.
        assert_eq!(
            reg.stop_target("p1", 1, "check"),
            StopTarget::Live("b".to_string())
        );
        // Terminal session → Absent (finished, nothing to stop).
        assert_eq!(reg.stop_target("p1", 2, "review"), StopTarget::Absent);
        // Absent pair → Absent.
        assert_eq!(reg.stop_target("p1", 99, "review"), StopTarget::Absent);
        // Project-scoped (#35): another project's id never matches.
        assert_eq!(reg.stop_target("p2", 1, "review"), StopTarget::Absent);
    }

    #[test]
    fn stop_target_reports_reserved_for_bare_reservation() {
        // AB#1069 F4: a reserved-but-not-yet-promoted pair has no thread_id to interrupt YET, but the
        // stop must NOT be dropped (that would let the start promote to Running unimpeded) — it is
        // `Reserved`, distinct from `Absent`, so the executor retries until promotion / start failure.
        // (`active_pairs` lumps reservations in as "taken"; `stop_target` keeps the distinction.)
        let reg = SessionRegistry::default();
        assert!(reg.try_reserve_pair("p1", 7, "review"));
        assert!(reg.active_pairs("p1").contains(&(7, "review".to_string())));
        assert_eq!(reg.stop_target("p1", 7, "review"), StopTarget::Reserved);
        // A different pair is still Absent.
        assert_eq!(reg.stop_target("p1", 7, "check"), StopTarget::Absent);
    }

    // Characterization (AB#1069 → AB#1204): try_reserve_pair consults ONLY in-memory state and
    // STAYS that way BY DESIGN — it is the atomic within-process test-and-set whose "terminal
    // sessions don't block" semantics are what let a NEW commit re-review (a prior `Done` must NOT
    // block). Making it consult a durable `(project, pr, kind)` check would over-block exactly that
    // legitimate re-review (the `review_session` table has no head_sha to tell commits apart).
    //
    // So the cross-restart duplicate window AB#1204 closes is NOT closed here — this assertion stays
    // true. It is closed one layer up, at the outbox executor entry, by a write-ahead claim keyed by
    // the OUTBOX ROW id (`review::claim_store` + `commands::start_for_outbox`): a crash-replay of the
    // same outbox row resolves its prior review's outcome instead of reserving freely. The carriers
    // for the closed window live there (`claim_store` round-trip + the `replayed_review_should_*`
    // suppression tests in `commands.rs`); this test pins that try_reserve_pair itself is unchanged.
    #[test]
    fn try_reserve_pair_is_in_memory_only_documents_restart_duplicate_window() {
        let reg = SessionRegistry::default(); // a fresh post-restart registry
        assert!(
            reg.try_reserve_pair("p1", 7, "review"),
            "a fresh (empty) registry reserves freely — try_reserve_pair stays in-memory by design; \
             the cross-restart guard (AB#1204) lives at the outbox executor, keyed by outbox_id"
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
            engine_kind: EngineKind::Codex,
            status: SessionStatus::Running,
            created_at_epoch: 0,
            comment_url: None,
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
        reg.promote_reservation(
            SessionInfo {
                project_id: "A".to_string(),
                thread_id: "tA".to_string(),
                turn_id: String::new(),
                pr_number: 9,
                kind: "review".to_string(),
                engine_kind: EngineKind::Codex,
                status: SessionStatus::Running,
                created_at_epoch: 0,
                comment_url: None,
            },
            test_url_ctx(),
        );
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
        reg.promote_reservation(
            SessionInfo {
                project_id: "p1".to_string(),
                thread_id: "t1".to_string(),
                turn_id: String::new(),
                pr_number: 7,
                kind: "review".to_string(),
                engine_kind: EngineKind::Codex,
                status: SessionStatus::Starting,
                created_at_epoch: 0,
                comment_url: None,
            },
            test_url_ctx(),
        );
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

    #[test]
    fn registry_get_by_thread_id() {
        // AB#1043: the local REST API's GET /reviews/{id} resolves an in-memory session by
        // its thread_id. A known id returns its snapshot; an unknown id is None (the caller
        // then falls through to the durable by-id read).
        let reg = SessionRegistry::default();
        assert!(reg.try_reserve_pair("p1", 7, "review"));
        reg.promote_reservation(
            SessionInfo {
                project_id: "p1".to_string(),
                thread_id: "t1".to_string(),
                turn_id: String::new(),
                pr_number: 7,
                kind: "review".to_string(),
                engine_kind: EngineKind::Codex,
                status: SessionStatus::Starting,
                created_at_epoch: 0,
                comment_url: None,
            },
            test_url_ctx(),
        );
        let got = reg.get("t1").expect("known thread_id resolves");
        assert_eq!(got.thread_id, "t1");
        assert_eq!(got.pr_number, 7);
        assert_eq!(got.status, SessionStatus::Starting);
        assert!(reg.get("missing").is_none(), "unknown thread_id → None");
    }

    #[test]
    fn promote_reservation_captures_url_context_at_start() {
        // AB#1042 (F3): the comment-URL source context is the START-TIME snapshot, captured in
        // the same critical section as the session insert and read back EXACTLY once at the
        // terminal. This pins that `finalize_turn` resolves the URL against the project the
        // review ran against, independent of any later config change (which would never touch
        // this captured value).
        let reg = SessionRegistry::default();
        assert!(reg.try_reserve_pair("p1", 7, "review"));
        let ctx = CommentUrlContext {
            source_kind: crate::model::SourceKind::Azure,
            repo: "the-repo".to_string(),
            azure_org: "the-org".to_string(),
            azure_project: "the-project".to_string(),
        };
        reg.promote_reservation(
            SessionInfo {
                project_id: "p1".to_string(),
                thread_id: "t1".to_string(),
                turn_id: String::new(),
                pr_number: 7,
                kind: "review".to_string(),
                engine_kind: EngineKind::Codex,
                status: SessionStatus::Starting,
                created_at_epoch: 0,
                comment_url: None,
            },
            ctx,
        );
        // The terminal reads the captured snapshot — the exact values supplied at start.
        let taken = reg
            .take_url_context("t1")
            .expect("context captured at start");
        assert_eq!(taken.source_kind, crate::model::SourceKind::Azure);
        assert_eq!(taken.repo, "the-repo");
        assert_eq!(taken.azure_org, "the-org");
        assert_eq!(taken.azure_project, "the-project");
        // Remove-on-read: a second take (or a session that never captured one) yields None,
        // so the map is bounded and the funnel reads it exactly once.
        assert!(reg.take_url_context("t1").is_none(), "second take is None");
        assert!(
            reg.take_url_context("never-existed").is_none(),
            "an unknown thread id yields None"
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
            engine_kind: EngineKind::Codex,
            status: SessionStatus::Running,
            created_at_epoch: 0,
            comment_url: None,
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
    fn begin_resume_only_proceeds_from_terminal() {
        // Chat-continuation guard: only a TERMINAL (Done/Failed) session flips to Running and
        // proceeds; an in-flight (Starting/Running/Interrupting) one is Busy (a concurrent
        // double-send is rejected); a missing id is NotFound.
        let reg = SessionRegistry::default();
        let seed = |thread: &str, status| {
            reg.insert(SessionInfo {
                project_id: "p1".to_string(),
                thread_id: thread.to_string(),
                turn_id: "tn".to_string(),
                pr_number: 7,
                kind: "review".to_string(),
                engine_kind: EngineKind::Codex,
                status,
                created_at_epoch: 0,
                comment_url: None,
            });
        };

        // Done → Proceed, and the status flips to Running (the follow-up turn is now live).
        seed("done", SessionStatus::Done);
        assert!(matches!(reg.begin_resume("done"), BeginResume::Proceed));
        assert_eq!(reg.get("done").unwrap().status, SessionStatus::Running);

        // Failed → Proceed too (a failed session can be retried via a follow-up).
        seed("failed", SessionStatus::Failed);
        assert!(matches!(reg.begin_resume("failed"), BeginResume::Proceed));
        assert_eq!(reg.get("failed").unwrap().status, SessionStatus::Running);

        // In-flight statuses → Busy (no transition; a turn is already running).
        for status in [
            SessionStatus::Starting,
            SessionStatus::Running,
            SessionStatus::Interrupting,
        ] {
            seed("busy", status);
            assert!(
                matches!(reg.begin_resume("busy"), BeginResume::Busy),
                "{status:?} must be Busy"
            );
            assert_eq!(reg.get("busy").unwrap().status, status, "no transition");
        }

        // Unknown id → NotFound.
        assert!(matches!(reg.begin_resume("missing"), BeginResume::NotFound));
    }

    #[test]
    fn rehydrate_inserts_durable_row_and_url_context() {
        // After a restart the registry is empty; `rehydrate` re-inserts a durable terminal
        // row (so `begin_resume` can accept it) AND a fresh `CommentUrlContext` (the first
        // turn's `finalize_turn` consumed the original) for the follow-up turn's terminal.
        let reg = SessionRegistry::default();
        assert!(reg.get("th-1").is_none(), "registry empty pre-rehydrate");
        let ctx = CommentUrlContext {
            source_kind: crate::model::SourceKind::Azure,
            repo: "the-repo".to_string(),
            azure_org: "the-org".to_string(),
            azure_project: "the-project".to_string(),
        };
        reg.rehydrate(
            SessionInfo {
                project_id: "p1".to_string(),
                thread_id: "th-1".to_string(),
                turn_id: String::new(),
                pr_number: 7,
                kind: "review".to_string(),
                engine_kind: EngineKind::Codex,
                status: SessionStatus::Done,
                created_at_epoch: 0,
                comment_url: None,
            },
            ctx,
        );
        // The row is now live and terminal → a follow-up can Proceed.
        let got = reg.get("th-1").expect("rehydrated row present");
        assert_eq!(got.pr_number, 7);
        assert_eq!(got.status, SessionStatus::Done);
        assert!(matches!(reg.begin_resume("th-1"), BeginResume::Proceed));
        // The fresh URL context is readable once at the terminal (remove-on-read).
        let taken = reg.take_url_context("th-1").expect("fresh context present");
        assert_eq!(taken.repo, "the-repo");
        assert!(reg.take_url_context("th-1").is_none(), "consumed on read");
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
            engine_kind: EngineKind::Codex,
            status: SessionStatus::Running,
            created_at_epoch: 0,
            comment_url: None,
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
            engine_kind: EngineKind::Codex,
            status: SessionStatus::Running,
            created_at_epoch: 1_700_000_000,
            // AB#1042: a resolved comment URL must surface as camelCase `commentUrl`.
            comment_url: Some("https://example.com/pr/7#c".to_string()),
        })
        .expect("SessionInfo serializes");
        assert_eq!(v["projectId"], "p1");
        assert_eq!(v["threadId"], "t1");
        assert_eq!(v["turnId"], "tn1");
        assert_eq!(v["prNumber"], 7);
        // `createdAtEpoch` (review F10) is the frontend sort key — pin its camelCase wire
        // key so a rename / drop surfaces here in lockstep with `ReviewSession` in TS.
        assert_eq!(v["createdAtEpoch"], 1_700_000_000_u64);
        // `kind` ("review"/"check") is a frontend contract field (mirrored by
        // `ReviewSession.kind` in `src/review/types.ts`); pin it so a rename / drop
        // surfaces here in lockstep with the camelCase keys.
        assert_eq!(v["kind"], "review");
        assert_eq!(v["engineKind"], "codex");
        assert_eq!(v["status"], "running");
        // AB#1042: `commentUrl` serializes camelCase; the snake_case form stays absent and
        // is mirrored by the optional `ReviewSession.commentUrl` on the TS side.
        assert_eq!(v["commentUrl"], "https://example.com/pr/7#c");
        assert!(v.get("comment_url").is_none());
        assert!(v.get("project_id").is_none());
        assert!(v.get("thread_id").is_none());
        assert!(v.get("engine_kind").is_none());
        assert!(v.get("created_at_epoch").is_none());

        // `comment_url: None` OMITS the key (skip_serializing_if) so the wire matches the
        // optional `commentUrl?` TS mirror — an absent key, not a JSON `null`.
        let no_url = serde_json::to_value(SessionInfo {
            comment_url: None,
            ..SessionInfo {
                project_id: "p1".to_string(),
                thread_id: "t1".to_string(),
                turn_id: "tn1".to_string(),
                pr_number: 7,
                kind: "review".to_string(),
                engine_kind: EngineKind::Codex,
                status: SessionStatus::Running,
                created_at_epoch: 1_700_000_000,
                comment_url: None,
            }
        })
        .expect("SessionInfo serializes");
        assert!(no_url.get("commentUrl").is_none(), "None omits commentUrl");
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
            engine_kind: EngineKind::Codex,
            status: SessionStatus::Starting,
            created_at_epoch: 0,
            comment_url: None,
        });
        reg.set_status("t1", SessionStatus::Failed);
        assert_eq!(reg.list()[0].status, SessionStatus::Failed);

        // The success path fills the turn id and flips to Running.
        reg.set_running("t1", "tn9".to_string());
        assert_eq!(reg.list()[0].turn_id, "tn9");
        assert_eq!(reg.list()[0].status, SessionStatus::Running);
    }

    // ── AB#1042: completion-notification primitive ──────────────────────────────

    #[test]
    fn subscribe_then_signal_delivers_the_outcome() {
        // Subscribe first, then signal: the receiver observes the `Some(outcome)` (status +
        // comment_url) once the funnel signals.
        let reg = SessionRegistry::default();
        let mut rx = reg.subscribe_completion("t1");
        assert!(rx.borrow().is_none(), "starts None (not yet terminal)");

        reg.signal_completion(
            "t1",
            CompletionOutcome {
                status: SessionStatus::Done,
                wire_status: "completed".to_string(),
                comment_url: Some("https://x/c".to_string()),
            },
        );
        let got = rx.borrow_and_update().clone().expect("outcome delivered");
        assert_eq!(got.status, SessionStatus::Done);
        // `wire_status` distinguishes a completed turn from an interrupted one (both Done).
        assert_eq!(got.wire_status, "completed");
        assert_eq!(got.comment_url.as_deref(), Some("https://x/c"));
    }

    #[test]
    fn signal_then_subscribe_still_sees_retained_outcome() {
        // Signal BEFORE anyone subscribes (get-or-create on the signal side): a later
        // subscriber still reads the retained terminal value — no "subscribed too late" race.
        let reg = SessionRegistry::default();
        reg.signal_completion(
            "t1",
            CompletionOutcome {
                status: SessionStatus::Failed,
                wire_status: "failed".to_string(),
                comment_url: None,
            },
        );
        let rx = reg.subscribe_completion("t1");
        let got = rx
            .borrow()
            .clone()
            .expect("retained value seen by late subscriber");
        assert_eq!(got.status, SessionStatus::Failed);
        assert!(got.comment_url.is_none());
    }

    #[test]
    fn signal_completion_carries_each_terminal_status() {
        // Done / Failed / Interrupted-as-Done each deliver a single Some with the right
        // status (the funnel maps interrupted → Done via `terminal_status`, so a completion
        // subscriber sees Done for a user stop too).
        for (status, wire) in [
            (SessionStatus::Done, "completed"),
            (SessionStatus::Failed, "failed"),
        ] {
            let reg = SessionRegistry::default();
            let rx = reg.subscribe_completion("t");
            reg.signal_completion(
                "t",
                CompletionOutcome {
                    status,
                    wire_status: wire.to_string(),
                    comment_url: None,
                },
            );
            assert_eq!(rx.borrow().as_ref().expect("delivered").status, status);
        }
        // `terminal_status("interrupted")` is Done — a user stop signals Done, not Failed.
        assert_eq!(terminal_status("interrupted"), SessionStatus::Done);
    }
}
