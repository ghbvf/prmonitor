//! Scheduled-pull loop + manual-pull trigger.
//!
//! A `tokio::time::interval` drives discovery, plus a manual `wake` (the "立即
//! 拉取" button) and live period changes (`reconfigure`). The loop never exits on
//! a discovery error — `discover_emit_dispatch` swallows it into a
//! [`PrEvent::Error`] and the loop continues. The first `interval` tick fires
//! immediately (t=0), so `start`/`reconfigure` each trigger an immediate
//! discovery ("启动即跑").
//!
//! **Graceful stop + unified cancellation domain (F1).** `stop()` never aborts:
//! it signals the `stop` Notify and drops the handle. The loop's inner
//! stop-select sits *both* on the idle wait *and* around the in-flight cycle, so
//! a stop mid-discovery returns immediately and drops the discovery future; the
//! `gh` child it owns dies via `kill_on_drop`. No orphaned subprocess.
//!
//! **Snapshot (F3 → #38).** The poll-cycle snapshot is now the persisted
//! `prs.json` tracked set: each successful discovery upserts the round's PRs into
//! it and emits the *retained* list (read on mount via the `get_prs` command).
//! Because the set is persisted it survives restarts, and a failed cycle leaves it
//! intact (the last good list stays readable). A transient one-round `gh` miss no
//! longer drops a row — it flips presence `Current`→`Stale` after the grace window.
//!
//! **Auto-trigger (#8).** Each cycle also passes its dispatchable candidates (the
//! clean rows) to an injected [`ProjectDispatcher`] hook. The hook is the seam that
//! keeps the `pr` slice review-agnostic: the loop knows nothing about how a review
//! starts, only that a closure consumes `(project_id, Vec<Candidate>)`. The
//! composition root ([`crate::dispatch`]) installs the real dispatcher via
//! [`SchedulerSet::set_dispatcher`] before reconciling, so even the immediate first
//! tick dispatches.
//!
//! **Multi-project (#35).** A single [`Scheduler`] drives ONE project's poll loop;
//! [`SchedulerSet`] owns a `project_id → Arc<Scheduler>` map and reconciles it to the
//! enabled projects (create/stop/reconfigure). Each scheduler captures its
//! `project_id` into the cycle closure, so every `PrEvent` it emits and every
//! dispatcher call it makes carries the routing key (#35). The dispatcher is shared:
//! [`SchedulerSet::set_dispatcher`] is installed once and cloned into each scheduler
//! on `reconcile`, so a project added later still inherits it.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde::Serialize;
use tauri::async_runtime::JoinHandle;
use tauri::{AppHandle, Emitter}; // Emitter for app.emit
use tokio::sync::Notify;
use tokio::time::MissedTickBehavior;

use crate::config::service::{self as config_service, Project};
use crate::error::AppResult;
use crate::events::{PrEvent, PRS_UPDATED_EVENT};
use crate::model::{Candidate, TrackedPrView};

use super::registry;

/// Internal poll-loop diagnostics (#62): timestamps + counters the panel reads to
/// answer "is the loop alive, when did it last run, and what happened". Pure data with
/// pure mutators (unit-tested below) — the `Scheduler` owns one behind an `Arc<StdMutex>`
/// and the cycle updates it; `SchedulerSet::poll_status` snapshots it into the wire
/// [`PollStatus`]. NO new event type: the frontend pulls this via the `poll_status`
/// command, keeping the `events.rs` union untouched. `pub(crate)` only so the
/// `pub(crate)` [`Scheduler::poll_diag`] accessor's return type is visibility-consistent;
/// it is not a public surface — the public wire type is [`PollStatus`].
#[derive(Debug, Clone, Default)]
pub(crate) struct PollDiag {
    /// Epoch of the most recent cycle entry (a tick / wake / reconfigure fired a cycle).
    last_started_epoch: Option<u64>,
    /// Epoch of the most recent successful discovery (discover returned `Ok`).
    last_success_epoch: Option<u64>,
    /// Epoch of the most recent discovery error.
    last_error_epoch: Option<u64>,
    /// The most recent discovery error message (kept until the next error overwrites it).
    last_error_message: Option<String>,
    /// Epoch of the most recent SUCCESSFUL persist of the round's list.
    last_persist_epoch: Option<u64>,
    /// PR count discovered in the most recent successful cycle.
    last_discovered_count: Option<u64>,
}

impl PollDiag {
    /// A cycle started (entered the body). Bumps `last_started_epoch`.
    fn mark_started(&mut self, now: u64) {
        self.last_started_epoch = Some(now);
    }

    /// Discovery succeeded with `count` rows. Records the success epoch + count.
    fn mark_discovered(&mut self, count: u64, now: u64) {
        self.last_success_epoch = Some(now);
        self.last_discovered_count = Some(count);
    }

    /// The round's list persisted successfully. Records the persist epoch.
    fn mark_persist(&mut self, now: u64) {
        self.last_persist_epoch = Some(now);
    }

    /// Discovery (or persist) failed. Records the error epoch + message.
    fn mark_error(&mut self, msg: String, now: u64) {
        self.last_error_epoch = Some(now);
        self.last_error_message = Some(msg);
    }
}

/// Poll-loop status reported to the settings panel (#62), pulled via the `poll_status`
/// command (NOT a new event type — the `events.rs` union stays untouched). `running` +
/// `interval_secs` describe the loop; the rest mirror [`PollDiag`]'s last-cycle fields.
///
/// camelCase wire type mirrored in `src/pr/types.ts` (Medium carrier per
/// `.claude/rules/prmonitor/ai-robust.md`; a `poll_status_wire_shape_*` golden test pins
/// the key shape so a rename can't silently drift the TS mirror). pr-slice-private (not a
/// cross-slice contract), same placement as [`super::webhook::WebhookStatus`].
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PollStatus {
    /// Whether this project's poll loop is currently running.
    pub running: bool,
    /// The resolved poll period (secs) — that project's `poll_interval_secs`, clamped.
    pub interval_secs: u64,
    /// Epoch of the most recent cycle entry.
    pub last_started_epoch: Option<u64>,
    /// Epoch of the most recent successful discovery.
    pub last_success_epoch: Option<u64>,
    /// Epoch of the most recent discovery error.
    pub last_error_epoch: Option<u64>,
    /// The most recent discovery error message.
    pub last_error_message: Option<String>,
    /// Epoch of the most recent successful persist.
    pub last_persist_epoch: Option<u64>,
    /// PR count discovered in the most recent successful cycle.
    pub last_discovered_count: Option<u64>,
}

/// Abstract per-cycle dispatch hook: consumes a cycle's `project_id` plus its
/// dispatchable [`Candidate`]s and drives them to completion (in practice:
/// auto-start their reviews concurrently). The leading `project_id` (#35) is the
/// routing key the composition root's dispatcher needs to resolve the project's repo
/// / engine / ledger partition. Boxed-future + `Arc` so it is `Clone`able into each
/// scheduler's cycle closure and erased of the review slice's types — the `pr` slice
/// stays review-agnostic (the only cross-slice contract it sees is `Candidate`). The
/// real implementation lives in the composition root ([`crate::dispatch`]).
pub type ProjectDispatcher =
    Arc<dyn Fn(String, Vec<Candidate>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Default poll period when config is unreadable or non-positive. Mirrors
/// `AppConfig::default().poll_interval_secs`. A 0 period would make
/// `interval(Duration::from_secs(0))` hot-spin, so [`resolve_period`] clamps it.
/// `pub(crate)` so the registry single-sources this clamp for its presence grace
/// window rather than re-hardcoding the default.
pub(crate) const DEFAULT_POLL_INTERVAL_SECS: u64 = 120;

/// The poll-loop handle for ONE project (#35). Owned by a [`SchedulerSet`] entry
/// keyed by `project_id`; all methods take `&self` and use interior mutability so the
/// set can drive it through an `Arc<Scheduler>`. Pre-#35 this was the single
/// composition-root handle in `AppState`; now `AppState` holds the [`SchedulerSet`].
#[derive(Default)]
pub struct Scheduler {
    task: StdMutex<Option<RunningTask>>,
    /// The auto-trigger dispatch hook (#8), installed via [`Self::set_dispatcher`]
    /// before `start` ([`SchedulerSet::reconcile`] clones the set's shared dispatcher
    /// into each scheduler). `Mutex<Option<_>>` defaults to `None` (so
    /// `#[derive(Default)]` still holds) — a `None` dispatcher means a cycle discovers
    /// + emits but starts no reviews (the pre-#8 behavior).
    dispatcher: StdMutex<Option<ProjectDispatcher>>,
    /// Per-loop poll diagnostics (#62), shared with the cycle closure so each cycle
    /// records its started / discovered / persist / error timestamps. `Arc<StdMutex<_>>`
    /// (defaults to an empty `PollDiag`, so `#[derive(Default)]` still holds) read by
    /// [`Self::poll_diag`] for the `poll_status` command.
    diag: Arc<StdMutex<PollDiag>>,
}

/// The live task plus the channels the loop selects on.
struct RunningTask {
    handle: JoinHandle<()>,
    wake: Arc<Notify>,
    reconfigure: Arc<Notify>,
    stop: Arc<Notify>,
}

impl Scheduler {
    /// Installs the auto-trigger dispatch hook (#8). Called *before* `start` (by
    /// [`SchedulerSet::reconcile`], cloning the set's shared dispatcher), so the
    /// immediate first tick already dispatches. Replaces any prior hook (last writer
    /// wins); a never-set dispatcher leaves cycles discover-and-emit only.
    pub fn set_dispatcher(&self, d: ProjectDispatcher) {
        *self.dispatcher.lock().unwrap() = Some(d);
    }

    /// Spawns this project's poll loop (#35), capturing `project_id` into the cycle so
    /// every emit / dispatch it makes is routed to that project. Idempotent: if a task
    /// is already live this is a no-op (no double-spawn). A finished task slot is
    /// replaced.
    pub fn start<R: tauri::Runtime>(&self, app: tauri::AppHandle<R>, project_id: String) {
        let mut slot = self.task.lock().unwrap();
        if let Some(task) = slot.as_ref() {
            if !task.handle.inner().is_finished() {
                return; // already running
            }
        }

        let wake = Arc::new(Notify::new());
        let reconfigure = Arc::new(Notify::new());
        let stop = Arc::new(Notify::new());

        // Snapshot the installed dispatcher once into the cycle closure: the loop
        // task outlives this `start` call, so it captures an owned
        // `Option<ProjectDispatcher>` rather than re-locking `self` each cycle.
        // `None` ⇒ no auto-trigger.
        let dispatcher = self.dispatcher.lock().unwrap().clone();

        // Production wiring: the period comes from THIS project's live config each
        // rebuild (resolved by id, with a default fallback if the project is gone), and
        // each cycle discovers → upserts the persisted set → emits → dispatches, all
        // scoped to `project_id`. Both are injected into the generic `run_loop` so the
        // lifecycle is testable (F4).
        let period_provider = {
            let app = app.clone();
            let project_id = project_id.clone();
            move || {
                resolve_period(
                    config_service::project(&app, &project_id).map(|p| p.poll_interval_secs),
                )
            }
        };
        // Snapshot the shared diag handle into the cycle closure (#62) so each cycle
        // records its timestamps into the same `PollDiag` `poll_diag` reads.
        let diag = self.diag.clone();
        let on_cycle = {
            let app = app.clone();
            let project_id = project_id.clone();
            move || {
                let app = app.clone();
                let project_id = project_id.clone();
                let dispatcher = dispatcher.clone();
                let diag = diag.clone();
                async move {
                    discover_emit_dispatch(&app, &project_id, dispatcher.as_ref(), &diag).await
                }
            }
        };

        let handle = tauri::async_runtime::spawn(run_loop(
            period_provider,
            on_cycle,
            Arc::clone(&wake),
            Arc::clone(&reconfigure),
            Arc::clone(&stop),
        ));

        *slot = Some(RunningTask {
            handle,
            wake,
            reconfigure,
            stop,
        });
    }

    /// Stops the poll loop gracefully (F1). No-op if not running.
    pub fn stop(&self) {
        if let Some(task) = self.task.lock().unwrap().take() {
            task.stop.notify_one();
            // No abort: the loop's stop-select tears the in-flight cycle down,
            // and gh's kill_on_drop kills the child. Dropping `task` detaches it.
        }
    }

    /// Triggers an immediate discovery on the running loop. Returns whether a
    /// running task was actually woken: `false` when stopped (task is `None`),
    /// so the caller (`poll_now`) can surface an error instead of leaving the
    /// frontend waiting for a `prs:updated` event that will never arrive.
    pub fn wake(&self) -> bool {
        if let Some(task) = self.task.lock().unwrap().as_ref() {
            task.wake.notify_one();
            true
        } else {
            false
        }
    }

    /// Asks the running loop to rebuild its ticker with a fresh period (no-op if
    /// stopped). Because the first tick fires immediately, this also triggers an
    /// immediate discovery.
    pub fn reconfigure(&self) {
        if let Some(task) = self.task.lock().unwrap().as_ref() {
            task.reconfigure.notify_one();
        }
    }

    /// Whether this scheduler's loop task is live (a slot present whose handle hasn't
    /// finished). Drives [`SchedulerSet::poll_status`]'s `running` flag (#62). A finished
    /// task slot (the loop exited) reports `false`. `pub(crate)`: only `SchedulerSet`
    /// (this module) reads it — not a public surface.
    pub(crate) fn is_running(&self) -> bool {
        self.task
            .lock()
            .unwrap()
            .as_ref()
            .map(|t| !t.handle.inner().is_finished())
            .unwrap_or(false)
    }

    /// Snapshot of this loop's diagnostics (#62) for [`SchedulerSet::poll_status`]. A
    /// clone so the lock is released before the caller maps it into the wire
    /// [`PollStatus`]. `pub(crate)` (returns the module-private [`PollDiag`]): only
    /// `SchedulerSet` reads it — the public wire surface is [`PollStatus`].
    pub(crate) fn poll_diag(&self) -> PollDiag {
        self.diag.lock().unwrap().clone()
    }
}

/// The composition-root handle for ALL projects' poll loops (#35). Lives in
/// [`crate::state::AppState`]; all methods take `&self` and use interior mutability so
/// a single shared `State<AppState>` can drive every project. Owns a
/// `project_id → Arc<Scheduler>` map plus the one shared [`ProjectDispatcher`] cloned
/// into each scheduler on [`Self::reconcile`].
#[derive(Default)]
pub struct SchedulerSet {
    /// One [`Scheduler`] per RUNNING project, keyed by `project_id`. `Arc` so a
    /// scheduler outlives a transient map borrow (the spawned loop holds no map
    /// reference; the map only holds the control handle).
    inner: StdMutex<HashMap<String, Arc<Scheduler>>>,
    /// The auto-trigger dispatch hook (#8), installed once by the composition root via
    /// [`Self::set_dispatcher`] and cloned into each scheduler on `reconcile`. `None`
    /// (the `#[derive(Default)]` value) leaves every cycle discover-and-emit only.
    dispatcher: StdMutex<Option<ProjectDispatcher>>,
}

impl SchedulerSet {
    /// Installs the shared auto-trigger dispatch hook (#8/#35). Called once by the
    /// composition root *before* the first [`Self::reconcile`], so a scheduler created
    /// by that reconcile inherits it and its immediate first tick already dispatches.
    /// Replaces any prior hook (last writer wins); already-running schedulers keep the
    /// dispatcher they were created with (a re-install only affects future creates).
    pub fn set_dispatcher(&self, d: ProjectDispatcher) {
        *self.dispatcher.lock().unwrap() = Some(d);
    }

    /// Reconciles the running schedulers to `projects` (#35). Idempotent — safe to call
    /// on every config save:
    /// - an `enabled` project NOT yet in the map → create a [`Scheduler`], install the
    ///   shared dispatcher, and `start` it (captures the project's id);
    /// - a mapped id that is no longer enabled (disabled, removed, or absent from
    ///   `projects`) → `stop` it and drop it from the map;
    /// - a surviving enabled project → `reconfigure` (re-read its period; the first
    ///   tick fires immediately, so this also re-polls).
    ///
    /// A DISABLED project is treated identically to a removed one (stopped), so the
    /// scheduler set always mirrors exactly the enabled projects.
    pub fn reconcile<R: tauri::Runtime>(&self, app: &tauri::AppHandle<R>, projects: &[Project]) {
        let dispatcher = self.dispatcher.lock().unwrap().clone();
        let mut map = self.inner.lock().unwrap();

        // The set of ids that SHOULD be running (enabled projects).
        let enabled_ids: std::collections::HashSet<&str> = projects
            .iter()
            .filter(|p| p.enabled)
            .map(|p| p.id.as_str())
            .collect();

        // Stop + drop schedulers whose project is no longer enabled (disabled / removed).
        map.retain(|id, scheduler| {
            if enabled_ids.contains(id.as_str()) {
                true
            } else {
                scheduler.stop();
                false
            }
        });

        // Create-or-reconfigure each enabled project.
        for project in projects.iter().filter(|p| p.enabled) {
            match map.get(&project.id) {
                Some(scheduler) => scheduler.reconfigure(), // survivor: re-read period + re-poll.
                None => {
                    let scheduler = Arc::new(Scheduler::default());
                    if let Some(d) = dispatcher.clone() {
                        scheduler.set_dispatcher(d);
                    }
                    scheduler.start(app.clone(), project.id.clone());
                    map.insert(project.id.clone(), scheduler);
                }
            }
        }
    }

    /// Triggers an immediate discovery on `project_id`'s running loop ("立即拉取").
    /// Returns whether a running scheduler was actually woken: `false` when that
    /// project is unknown / stopped, so the caller (`poll_now`) can surface an error
    /// instead of leaving the frontend awaiting a `prs:updated` that never arrives.
    pub fn wake(&self, project_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(project_id)
            .map(|s| s.wake())
            .unwrap_or(false)
    }

    /// Asks `project_id`'s running loop to rebuild its ticker with a fresh period
    /// (no-op if that project is unknown / stopped). Because the first tick fires
    /// immediately, this also triggers an immediate discovery.
    pub fn reconfigure(&self, project_id: &str) {
        if let Some(s) = self.inner.lock().unwrap().get(project_id) {
            s.reconfigure();
        }
    }

    /// Stops + drops EVERY project's loop (the `stop_polling`-all path + app shutdown).
    /// After this the set is empty; a later [`Self::reconcile`] re-creates the enabled
    /// schedulers from scratch.
    pub fn stop_all(&self) {
        let mut map = self.inner.lock().unwrap();
        for scheduler in map.values() {
            scheduler.stop();
        }
        map.clear();
    }

    /// Reports `project_id`'s poll-loop status (#62) for the settings panel, pulled via
    /// the `poll_status` command. When that project's scheduler exists AND is running, the
    /// diag fields are copied from its live [`PollDiag`]; otherwise `running: false` with
    /// the default (all-`None`) diag. `interval_secs` is always the resolved period for
    /// that project (so the panel shows the configured cadence even while stopped),
    /// clamped through [`resolve_period`] from the persisted `poll_interval_secs`.
    pub fn poll_status<R: tauri::Runtime>(
        &self,
        app: &AppHandle<R>,
        project_id: &str,
    ) -> PollStatus {
        let interval_secs =
            resolve_period(config_service::project(app, project_id).map(|p| p.poll_interval_secs));
        // Snapshot the running scheduler's diag (if any) without holding the map lock
        // across the projection.
        let diag = {
            let map = self.inner.lock().unwrap();
            match map.get(project_id) {
                Some(s) if s.is_running() => Some(s.poll_diag()),
                _ => None,
            }
        };
        match diag {
            Some(d) => PollStatus {
                running: true,
                interval_secs,
                last_started_epoch: d.last_started_epoch,
                last_success_epoch: d.last_success_epoch,
                last_error_epoch: d.last_error_epoch,
                last_error_message: d.last_error_message,
                last_persist_epoch: d.last_persist_epoch,
                last_discovered_count: d.last_discovered_count,
            },
            None => PollStatus {
                running: false,
                interval_secs,
                ..PollStatus::default()
            },
        }
    }
}

/// The poll loop, with its period source and per-cycle action injected (F4) so
/// the lifecycle (start/wake/reconfigure/stop) is unit-testable without `gh`,
/// config, or an `AppHandle`. Outer loop rebuilds the ticker on `reconfigure`;
/// inner loop selects between the ticker, a manual wake, reconfigure (break to
/// rebuild), and stop (return to exit).
///
/// The cycle itself runs under a second stop-select: a `stop` arriving mid-cycle
/// returns immediately, dropping the `on_cycle` future — the unified
/// cancellation domain (F1) that lets `gh`'s `kill_on_drop` reap the child.
async fn run_loop<P, C, Fut>(
    period_provider: P,
    on_cycle: C,
    wake: Arc<Notify>,
    reconfigure: Arc<Notify>,
    stop: Arc<Notify>,
) where
    P: Fn() -> u64 + Send + 'static,
    C: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    loop {
        let period = period_provider();
        let mut ticker = tokio::time::interval(Duration::from_secs(period));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick()          => {}
                _ = wake.notified()        => {}
                _ = reconfigure.notified() => break,   // rebuild ticker w/ fresh period
                _ = stop.notified()        => return,
            }
            // Unified cancellation domain: stop interrupts an in-flight cycle,
            // dropping the discovery future → gh's kill_on_drop kills the child.
            tokio::select! {
                _ = on_cycle()      => {}
                _ = stop.notified() => return,
            }
        }
    }
}

/// Runs one discovery cycle: upserts the round's PRs into the persisted tracked
/// set (#38), emits the *retained* list, then auto-triggers the dispatchable
/// candidates (#8). A discovery failure folds into a [`PrEvent::Error`] (the loop
/// survives it) and leaves the persisted set intact — the last good list stays
/// readable via `get_prs` — and yields no dispatchable candidates (nothing is
/// auto-started on a failed cycle). The emitted list is the persisted set projected
/// at the current epoch, so a transient miss flips a row's presence instead of
/// dropping it (the ghost-flicker fix). The upsert + persist run through the registry's
/// single serialized write seam ([`registry::mutate_tracked`], F1), so this cycle can't
/// interleave with a concurrent `set_pr_archived` and lose a write. A store failure
/// (load or save) surfaces as [`PrEvent::Error`] rather than a misleading `Updated`
/// (F2) — an un-persisted in-memory set would vanish on restart, so the cycle must not
/// report a retained-snapshot update it did not durably make. Auto-dispatch is a
/// start-review decision independent of persistence, so it still runs on a persist
/// failure.
///
/// The dispatch is *spawned detached* (not awaited) after the emit — see the body
/// for why the scheduler's stop must not be able to cancel a start in flight; an
/// empty dispatchable list skips the hook entirely.
async fn discover_emit_dispatch<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: &str,
    dispatcher: Option<&ProjectDispatcher>,
    diag: &Arc<StdMutex<PollDiag>>,
) {
    // Mark this cycle as started (#62) before the (possibly slow) discovery, so the panel
    // shows the loop is actively working even while `gh` is in flight.
    diag.lock()
        .unwrap()
        .mark_started(super::ledger::now_epoch());
    let (event, dispatchable) = match super::commands::discover(app, project_id).await {
        Ok((views, dispatchable)) => {
            // Discovery succeeded: record the success epoch + count (#62).
            diag.lock()
                .unwrap()
                .mark_discovered(views.len() as u64, super::ledger::now_epoch());
            // Upsert this round and persist it through the registry's single
            // serialized write seam (F1): `mutate_tracked` holds the cross-writer lock
            // across load→upsert→save so a concurrent `set_pr_archived` can't interleave
            // and lose a write. Scoped to `project_id` (#35) so this project's set is
            // isolated. The closure always persists (an upsert always changes the set);
            // the projection is built inside the seam from the just-upserted set.
            // `now`/`grace` are read before the lock to keep the critical section
            // minimal. A store failure (load or save) maps to Error, not a misleading
            // Updated (F2). Auto-dispatch is independent of persistence and still runs.
            let now = super::ledger::now_epoch();
            let grace = registry::presence_grace_secs(app, project_id);
            let persisted = registry::mutate_tracked(app, project_id, |tracked| {
                tracked.upsert(&views, now);
                (true, registry::to_view_list(tracked, now, grace))
            });
            // Record the persist outcome (#62): a successful persist bumps the persist
            // clock; a store failure records the error (it surfaces as `PrEvent::Error`
            // below — keep the diag's error state in lockstep with what the UI sees).
            match &persisted {
                Ok(_) => diag
                    .lock()
                    .unwrap()
                    .mark_persist(super::ledger::now_epoch()),
                Err(e) => diag
                    .lock()
                    .unwrap()
                    .mark_error(e.message.clone(), super::ledger::now_epoch()),
            }
            (persist_event(project_id, persisted), dispatchable)
        }
        // On discovery error the dispatchable list is empty — nothing auto-starts.
        Err(e) => {
            // Record the discovery error (#62) so the panel surfaces "last cycle failed".
            diag.lock()
                .unwrap()
                .mark_error(e.message.clone(), super::ledger::now_epoch());
            (
                PrEvent::Error {
                    project_id: project_id.to_string(),
                    message: e.message,
                },
                Vec::new(),
            )
        }
    };
    let _ = app.emit(PRS_UPDATED_EVENT, &event); // ignore emit error (window may be gone)

    if let Some(d) = dispatcher {
        if !dispatchable.is_empty() && auto_review_enabled(app, project_id) {
            // Spawn the dispatch DETACHED rather than awaiting it inline. This cycle
            // runs inside the loop's stop-cancellable `select!` (the F1 cancellation
            // domain that lets a stop reap the in-flight `gh` child). Awaiting
            // `start_review` here would put it in that same domain, so a
            // `stop_polling` landing mid-start would drop a half-started review —
            // leaving a `Starting` session that `stop_review` can't interrupt
            // (`begin_interrupt` is a no-op on `Starting`). The dispatcher future is
            // `Send + 'static`, so the spawned task runs to its terminal
            // (`Running`/`Failed`) regardless of the poll loop. Cross-cycle dedup is
            // unaffected: an in-flight dispatch is still covered by the next
            // discovery's ledger gate and the dispatcher's own in-flight registry
            // guard. Discovery itself stays cancellable (it is awaited above), so a
            // stop still reaps `gh`. The JoinHandle is dropped explicitly (detached):
            // the task runs to completion regardless of the poll loop.
            drop(tauri::async_runtime::spawn(d(
                project_id.to_string(),
                dispatchable,
            )));
        }
    }
}

/// 每轮重读 `project_id` 的自动 review 开关（运行时切换无需重启，#35 按项目）。
/// 这是 pr→config 的**函数级跨切片读**（走 config 公有 service `project`，`AppConfig`
/// 仍 config 私有；`auto_review` 现为 [`Project`] 字段）。load / 找不到项目 → 返回 false
/// （不派发）：config 不可读或项目缺失时不擅自消耗 review 额度/算力，宁可漏触发也不误触发；
/// 下一轮 load 成功即恢复。
/// `pub(crate)`：webhook trigger（[`crate::pr::webhook`]）的派发闭包复用同一开关，
/// 与本轮询调用点一致——两条 auto-trigger 路径共用同一 per-project autoReview gate。
pub(crate) fn auto_review_enabled<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: &str,
) -> bool {
    config_service::project(app, project_id)
        .map(|p| p.auto_review)
        .unwrap_or(false)
}

/// Maps the locked persist seam's result to the cycle event (F2), scoped to
/// `project_id` (#35). A successful load→upsert→save yields the retained projection
/// (`Updated`); any store failure (load or save, surfaced as `Err` by
/// [`registry::mutate_tracked`]) yields `Error` instead of a misleading `Updated` —
/// the in-memory set was not durably persisted (it vanishes on restart), so claiming
/// "retained snapshot updated" would lie to the UI. Pure over the seam's result, so
/// the decision is unit-testable without an `AppHandle` or a store (the emit +
/// dispatch around it need a live app).
fn persist_event(project_id: &str, result: AppResult<Vec<TrackedPrView>>) -> PrEvent {
    match result {
        Ok(list) => PrEvent::Updated {
            project_id: project_id.to_string(),
            prs: list,
        },
        Err(e) => PrEvent::Error {
            project_id: project_id.to_string(),
            message: format!("PR 列表持久化失败：{}", e.message),
        },
    }
}

/// Clamps a loaded poll period to a usable value: a positive load passes
/// through; 0 or an error falls back to [`DEFAULT_POLL_INTERVAL_SECS`] (a 0
/// period would hot-spin the interval). `pub(crate)` so the registry reuses this
/// single clamp for its presence grace window.
pub(crate) fn resolve_period(loaded: AppResult<u64>) -> u64 {
    match loaded {
        Ok(s) if s > 0 => s,
        _ => DEFAULT_POLL_INTERVAL_SECS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppError;
    use tokio::sync::mpsc;

    #[test]
    fn resolve_period_clamps_zero_to_default() {
        assert_eq!(resolve_period(Ok(0)), DEFAULT_POLL_INTERVAL_SECS);
    }

    #[test]
    fn resolve_period_clamps_error_to_default() {
        assert_eq!(
            resolve_period(Err(AppError::new("x"))),
            DEFAULT_POLL_INTERVAL_SECS
        );
    }

    #[test]
    fn resolve_period_passes_through_positive() {
        assert_eq!(resolve_period(Ok(30)), 30);
    }

    // F2: a successful persist emits the retained projection (`Updated`); a store
    // failure (load or save, surfaced as `Err` by `mutate_tracked`) must surface as
    // `Error`, never a misleading `Updated`. The pre-fix code (`let _ =
    // tracked.save(app)`) emitted `Updated` regardless, so the UI treated an
    // un-persisted in-memory set as the retained snapshot (silently lost on restart).
    #[test]
    fn persist_event_on_persist_ok_is_updated() {
        let ev = persist_event("p1", Ok(Vec::new()));
        // #35: the event carries the routing `project_id`.
        match ev {
            PrEvent::Updated { project_id, .. } => assert_eq!(project_id, "p1"),
            other => panic!("a successful persist emits Updated, not {other:?}"),
        }
    }

    #[test]
    fn persist_event_on_store_failure_is_error_not_updated() {
        let ev = persist_event("p1", Err(AppError::new("写入 PR 存储失败: disk full")));
        match ev {
            PrEvent::Error {
                project_id,
                message,
            } => {
                assert_eq!(project_id, "p1", "Error is routed to the project (#35)");
                assert!(
                    message.contains("持久化失败"),
                    "Error carries a persist-failure message, got {message:?}"
                );
            }
            other => panic!("a store failure must emit Error, not {other:?}"),
        }
    }

    #[test]
    fn default_scheduler_is_not_running() {
        let scheduler = Scheduler::default();
        assert!(scheduler.task.lock().unwrap().is_none());
    }

    #[test]
    fn default_scheduler_has_no_dispatcher() {
        // `#[derive(Default)]` must keep working with the new field: an
        // un-installed dispatcher is `None`, so cycles discover-and-emit only.
        let scheduler = Scheduler::default();
        assert!(scheduler.dispatcher.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn set_dispatcher_stores_and_retrieves_the_hook() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex as TestMutex;

        // The hook plumbing in isolation: `set_dispatcher` stores a counting
        // closure; retrieving it (the same `lock().clone()` `start` does) and
        // invoking it with a project id + sample candidates must run the closure.
        // Verifies storage/retrieval AND that the leading `project_id` (#35) reaches
        // the hook — without needing an `AppHandle` or real dispatch.
        let scheduler = Scheduler::default();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(AtomicUsize::new(0));
        let seen_pid: Arc<TestMutex<Option<String>>> = Arc::new(TestMutex::new(None));
        let dispatcher: ProjectDispatcher = {
            let count = Arc::clone(&count);
            let seen = Arc::clone(&seen);
            let seen_pid = Arc::clone(&seen_pid);
            Arc::new(move |project_id: String, cands: Vec<Candidate>| {
                let count = Arc::clone(&count);
                let seen = Arc::clone(&seen);
                let seen_pid = Arc::clone(&seen_pid);
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    seen.fetch_add(cands.len(), Ordering::SeqCst);
                    *seen_pid.lock().unwrap() = Some(project_id);
                })
            })
        };
        scheduler.set_dispatcher(dispatcher);

        let stored = scheduler
            .dispatcher
            .lock()
            .unwrap()
            .clone()
            .expect("set_dispatcher stores the hook");
        stored(
            "p1".to_string(),
            vec![candidate(1, "review"), candidate(2, "check")],
        )
        .await;

        assert_eq!(count.load(Ordering::SeqCst), 1, "hook ran once");
        assert_eq!(seen.load(Ordering::SeqCst), 2, "hook saw both candidates");
        assert_eq!(
            seen_pid.lock().unwrap().as_deref(),
            Some("p1"),
            "the routing project_id reached the hook (#35)"
        );
    }

    // ── SchedulerSet (#35) ──────────────────────────────────────────────────
    // The map-management methods that don't need an `AppHandle` (`reconcile` does,
    // so it's exercised in the live app). These lock the multi-project invariants:
    // a default set is empty + dispatcher-less, `wake`/`reconfigure` on an unknown
    // project are safe no-ops, and `stop_all` clears the map.

    #[test]
    fn default_scheduler_set_is_empty_and_dispatcherless() {
        let set = SchedulerSet::default();
        assert!(set.inner.lock().unwrap().is_empty());
        assert!(set.dispatcher.lock().unwrap().is_none());
    }

    #[test]
    fn scheduler_set_wake_unknown_project_is_false() {
        // `poll_now` relies on this: waking a project with no running scheduler
        // returns false so the command can surface "paused" rather than hang.
        let set = SchedulerSet::default();
        assert!(!set.wake("nope"));
    }

    #[test]
    fn scheduler_set_reconfigure_and_stop_all_unknown_are_noops() {
        let set = SchedulerSet::default();
        set.reconfigure("nope"); // no panic on an empty map
        set.stop_all(); // no panic on an empty map
        assert!(set.inner.lock().unwrap().is_empty());
    }

    #[test]
    fn scheduler_set_set_dispatcher_stores_shared_hook() {
        // The set's shared dispatcher is what `reconcile` clones into each scheduler;
        // installing it before any reconcile is what lets a later-added project
        // inherit auto-dispatch.
        let set = SchedulerSet::default();
        let dispatcher: ProjectDispatcher =
            Arc::new(|_pid: String, _cands: Vec<Candidate>| Box::pin(async {}));
        set.set_dispatcher(dispatcher);
        assert!(set.dispatcher.lock().unwrap().is_some());
    }

    #[test]
    fn scheduler_set_stop_all_clears_running_entries() {
        // Seed the map directly with a never-started scheduler (no `AppHandle`
        // needed): `stop_all` must drop every entry so a later reconcile rebuilds.
        let set = SchedulerSet::default();
        set.inner
            .lock()
            .unwrap()
            .insert("p1".to_string(), Arc::new(Scheduler::default()));
        assert_eq!(set.inner.lock().unwrap().len(), 1);
        set.stop_all();
        assert!(set.inner.lock().unwrap().is_empty());
    }

    fn candidate(number: u64, kind: &str) -> Candidate {
        Candidate {
            number,
            head_sha: "sha".to_string(),
            head_ref: "ref".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            kind: kind.to_string(),
        }
    }

    // ── Lifecycle tests (F4) ───────────────────────────────────────────────
    // Drive `run_loop` directly with injected boundaries: a period provider
    // returning 3600s (the real ticker won't fire during the test, so every
    // cycle observed is driven by an explicit `wake`/`reconfigure`/immediate
    // first tick), and an `on_cycle` that signals an `mpsc` channel. Assertions
    // use `timeout` on `recv`/the JoinHandle, never sleeps, so they're
    // deterministic. These cover the seam F1 (graceful stop) and F4 (DI) open.

    /// Spawns `run_loop` with an injected cycle-counter channel and a 3600s
    /// period (so only explicit signals or the immediate first tick drive a
    /// cycle). Returns the cycle-recv channel, the three control Notifies, and
    /// the JoinHandle.
    #[allow(clippy::type_complexity)]
    fn spawn_test_loop() -> (
        mpsc::UnboundedReceiver<()>,
        Arc<Notify>,
        Arc<Notify>,
        Arc<Notify>,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel::<()>();
        let wake = Arc::new(Notify::new());
        let reconfigure = Arc::new(Notify::new());
        let stop = Arc::new(Notify::new());

        let on_cycle = move || {
            let tx = tx.clone();
            async move {
                let _ = tx.send(());
            }
        };

        let handle = tokio::spawn(run_loop(
            || 3600, // huge period: real ticker never fires during the test
            on_cycle,
            Arc::clone(&wake),
            Arc::clone(&reconfigure),
            Arc::clone(&stop),
        ));
        (rx, wake, reconfigure, stop, handle)
    }

    /// Awaits one cycle signal, failing the test if none arrives in time.
    async fn expect_cycle(rx: &mut mpsc::UnboundedReceiver<()>) {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a cycle should run before timeout")
            .expect("on_cycle channel should stay open");
    }

    #[tokio::test]
    async fn run_loop_runs_immediate_first_cycle() {
        let (mut rx, _wake, _reconfigure, stop, handle) = spawn_test_loop();
        // The interval's first tick fires at t=0 → "启动即跑".
        expect_cycle(&mut rx).await;
        stop.notify_one();
        handle.await.expect("loop should join after stop");
    }

    #[tokio::test]
    async fn run_loop_wake_triggers_a_cycle() {
        let (mut rx, wake, _reconfigure, stop, handle) = spawn_test_loop();
        expect_cycle(&mut rx).await; // immediate first tick
        wake.notify_one();
        expect_cycle(&mut rx).await; // manual wake → another cycle
        stop.notify_one();
        handle.await.expect("loop should join after stop");
    }

    #[tokio::test]
    async fn run_loop_reconfigure_rebuilds_and_runs_immediate_cycle() {
        let (mut rx, _wake, reconfigure, stop, handle) = spawn_test_loop();
        expect_cycle(&mut rx).await; // immediate first tick
        reconfigure.notify_one();
        // Rebuilding the ticker fires a fresh immediate first tick → a cycle.
        expect_cycle(&mut rx).await;
        stop.notify_one();
        handle.await.expect("loop should join after stop");
    }

    #[tokio::test]
    async fn run_loop_stop_makes_the_task_finish() {
        let (mut rx, _wake, _reconfigure, stop, handle) = spawn_test_loop();
        expect_cycle(&mut rx).await; // ensure the loop is up
        stop.notify_one();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("stop should let the spawned task finish")
            .expect("loop task should not panic");
    }

    // ── Poll diagnostics (#62) ─────────────────────────────────────────────
    // The pure `PollDiag` mutators in isolation (the cycle wiring that calls them
    // needs an `AppHandle`, so it runs in the live app). Each mutator sets exactly
    // its fields, so a snapshot taken by `poll_status` reflects the last cycle.

    #[test]
    fn default_scheduler_has_empty_poll_diag() {
        // `#[derive(Default)]` must keep working with the new `diag` field: a fresh
        // scheduler is not running and its diag is all-`None`.
        let scheduler = Scheduler::default();
        assert!(
            !scheduler.is_running(),
            "a default scheduler is not running"
        );
        let d = scheduler.poll_diag();
        assert!(d.last_started_epoch.is_none());
        assert!(d.last_success_epoch.is_none());
        assert!(d.last_error_epoch.is_none());
        assert!(d.last_persist_epoch.is_none());
        assert!(d.last_discovered_count.is_none());
    }

    #[test]
    fn poll_diag_mark_started_sets_started_epoch_only() {
        let mut d = PollDiag::default();
        d.mark_started(100);
        assert_eq!(d.last_started_epoch, Some(100));
        assert!(d.last_success_epoch.is_none());
        assert!(d.last_error_epoch.is_none());
    }

    #[test]
    fn poll_diag_mark_discovered_records_count_and_success() {
        let mut d = PollDiag::default();
        d.mark_discovered(7, 200);
        assert_eq!(d.last_success_epoch, Some(200));
        assert_eq!(d.last_discovered_count, Some(7));
        // mark_discovered does not touch the error fields.
        assert!(d.last_error_epoch.is_none());
    }

    #[test]
    fn poll_diag_mark_persist_sets_persist_epoch() {
        let mut d = PollDiag::default();
        d.mark_persist(300);
        assert_eq!(d.last_persist_epoch, Some(300));
    }

    #[test]
    fn poll_diag_mark_error_records_epoch_and_message() {
        let mut d = PollDiag::default();
        d.mark_started(100);
        d.mark_error("gh exploded".to_string(), 400);
        assert_eq!(d.last_error_epoch, Some(400));
        assert_eq!(d.last_error_message.as_deref(), Some("gh exploded"));
        // A prior started epoch is untouched by an error (they are independent clocks).
        assert_eq!(d.last_started_epoch, Some(100));
        // An error does not advance the success clock.
        assert!(d.last_success_epoch.is_none());
    }

    // Wire-shape lock for `PollStatus` (#62, Medium carrier per ai-robust.md): the
    // `poll_status` command's wire type, mirrored in `src/pr/types.ts`; a field rename
    // would drift the TS mirror silently. Same pattern as
    // `webhook_status_wire_shape_is_camel_case`.
    #[test]
    fn poll_status_wire_shape_is_camel_case() {
        let s = PollStatus {
            running: true,
            interval_secs: 120,
            last_started_epoch: Some(1_700_000_000),
            last_success_epoch: Some(1_700_000_010),
            last_error_epoch: None,
            last_error_message: None,
            last_persist_epoch: Some(1_700_000_011),
            last_discovered_count: Some(3),
        };
        let v = serde_json::to_value(&s).expect("PollStatus serializes");

        // camelCase keys present.
        assert!(v.get("running").is_some());
        assert!(v.get("intervalSecs").is_some());
        assert!(v.get("lastStartedEpoch").is_some());
        assert!(v.get("lastSuccessEpoch").is_some());
        assert!(v.get("lastErrorEpoch").is_some());
        assert!(v.get("lastErrorMessage").is_some());
        assert!(v.get("lastPersistEpoch").is_some());
        assert!(v.get("lastDiscoveredCount").is_some());

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("interval_secs").is_none());
        assert!(v.get("last_started_epoch").is_none());
        assert!(v.get("last_discovered_count").is_none());

        // `None` fields serialize to JSON null (not omitted), so the TS mirror's
        // optional-or-null contract stays closed.
        assert_eq!(v["lastErrorEpoch"], serde_json::Value::Null);
        assert_eq!(v["lastErrorMessage"], serde_json::Value::Null);
    }
}
