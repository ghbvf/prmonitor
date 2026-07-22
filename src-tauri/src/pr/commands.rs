//! PR slice Tauri commands: manual fetch + `gh` auth status.

use crate::config::service as config_service;
use crate::error::AppResult;
use crate::events::{PrEvent, StreamEvent};
use crate::model::{Candidate, CliTool, PullRequestView, SourceKind, UpdateMode};

use super::azure::{az_auth_status, AzStatus, AzureDevOpsCli};
use super::bitbucket::BitbucketServer;
use super::discover::{self, MonitorParams};
use super::gh::{gh_auth_status, GhStatus, GithubCli};
use super::ledger::{now_epoch, Ledger};
use super::scheduler::PollStatus;
use super::source::{DiscoveredEvent, EventSourceProvider};
use super::webhook::{DeliveryStatus, IngestIntent, WebhookDelivery, WebhookEvent, WebhookStatus};

/// Which registry write `ingest_webhook` performs for a parsed intent: `Upsert` is the
/// insert-or-update Track path; `UpdatePresent` is the status-only path (refresh an
/// EXISTING row, never insert). File-private + module-level (not defined inside the async
/// fn body) so the ingest reads as one straight-line decision.
enum WriteKind {
    Upsert,
    UpdatePresent,
}

/// Annotates one discovered [`DiscoveredEvent`] for the PR list and surfaces its
/// dispatchable [`Candidate`] when nothing gates it. Conflict (both trigger labels)
/// skips first, matching `router.py`'s discovery-stage drop; otherwise static gates
/// then cooldown. The live gate is reserved dead code (no second `gh` call per
/// poll), so a `None` skip reason here *is* the dispatch decision: the candidate
/// is returned for auto-trigger.
///
/// AB#1070: source-agnostic — every source arm feeds the same [`DiscoveredEvent`]
/// here, taking the display fields from its normalized [`Event`] (`title` / `labels`
/// / `url`) and the gating fields from its [`Candidate`].
///
/// Returns `(view, Some(candidate))` for a clean row, `(view, None)` for a skipped
/// one — so the caller partitions the cycle's rows into the emit list (all views)
/// and the dispatch list (clean candidates) in one pass.
fn build_view(
    de: &DiscoveredEvent,
    params: &MonitorParams,
    ledger: &Ledger,
    now: u64,
) -> (PullRequestView, Option<Candidate>) {
    build_view_parts(
        de.candidate.clone(),
        de.event
            .as_observation()
            .map(|o| o.subject.title.clone())
            .unwrap_or_default(),
        de.event
            .as_observation()
            .map(|o| o.subject.labels.clone())
            .unwrap_or_default(),
        de.event
            .as_observation()
            .map(|o| o.subject.url.clone())
            .unwrap_or_default(),
        de.conflict,
        params,
        ledger,
        now,
    )
}

/// The source-agnostic core of the discovery view + dispatch decision (#818): given a
/// gating [`Candidate`] plus its display fields (`title` / `labels` / `url`) and the
/// both-trigger-label `conflict` flag, applies the SAME gate composition the poll path
/// uses — conflict short-circuits to [`discover::BOTH_TRIGGER_LABELS_REASON`], otherwise
/// static (`should_skip`) then cooldown (`cooldown_skip`) — and surfaces the dispatchable
/// candidate only when nothing gates it (`skip_reason` None).
///
/// All three source arms feed this via [`build_view`] (over a normalized [`DiscoveredEvent`]),
/// giving every source FULL display + gating parity (title / url / all labels / kept-conflict),
/// so the only difference between sources is how the rows are fetched, not how they are shown or
/// gated. Pure (no `AppHandle`) so the gate composition is unit-tested.
#[allow(clippy::too_many_arguments)]
fn build_view_parts(
    candidate: Candidate,
    title: String,
    labels: Vec<String>,
    url: String,
    conflict: bool,
    params: &MonitorParams,
    ledger: &Ledger,
    now: u64,
) -> (PullRequestView, Option<Candidate>) {
    let skip_reason = if conflict {
        Some(discover::BOTH_TRIGGER_LABELS_REASON.to_string())
    } else {
        discover::should_skip(&candidate, params, ledger)
            .or_else(|| discover::cooldown_skip(&candidate, params, ledger, now))
    };
    // Clone the candidate for dispatch only when it passes every static + cooldown
    // gate (skip_reason None); a skipped row contributes a view but no candidate.
    let dispatchable = skip_reason.is_none().then(|| candidate.clone());
    let view = PullRequestView {
        number: candidate.number,
        title,
        labels,
        url,
        skill_key: candidate.skill_key,
        skip_reason,
    };
    (view, dispatchable)
}

/// Projects a source's discovered events into the cycle's `(views, dispatchable candidates)`:
/// every [`DiscoveredEvent`] yields a view; a clean (non-gated) one also yields a dispatch
/// [`Candidate`]. The shared per-source body of [`discover`]'s three arms (AB#1070) — so the
/// only difference between sources is how the events are fetched, not how they are gated/shown.
fn partition_events(
    events: Vec<DiscoveredEvent>,
    params: &MonitorParams,
    ledger: &Ledger,
    now: u64,
) -> (Vec<PullRequestView>, Vec<DiscoveredEvent>) {
    let mut views = Vec::with_capacity(events.len());
    let mut dispatchable = Vec::new();
    for de in events {
        let (view, cand) = build_view(&de, params, ledger, now);
        if cand.is_some() {
            dispatchable.push(de);
        }
        views.push(view);
    }
    (views, dispatchable)
}

/// Discovers `project_id`'s monitored repo's open trigger-labelled PRs now (#35),
/// returning both the annotated views (for the PR list / snapshot) and the
/// dispatchable [`Candidate`]s — the clean rows (`skip_reason` None), which already
/// exclude conflict / draft / cross-repo / disallowed-author / already-dispatched /
/// within-cooldown PRs. Reads THAT project's config (repo, labels, authors, cooldown)
/// and its ledger partition; performs two `gh pr list` calls (review + check labels).
///
/// This is the shared discovery body driven by the scheduler's per-project poll loop
/// (`scheduler::discover_emit_dispatch`), its only caller; there is no manual-fetch
/// command — the frontend triggers a refresh via `poll_now`. The dispatchable
/// candidates flow to the durable event sink.
pub(crate) async fn discover<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: &str,
) -> AppResult<(Vec<PullRequestView>, Vec<DiscoveredEvent>)> {
    // Cross-slice read of the config slice's public service (function-level, not
    // a type contract — `AppConfig` stays config-private; we resolve THIS project by
    // id and snapshot the fields the pr slice needs into `MonitorParams`).
    let project = config_service::project(app, project_id)?;
    // Capture the source-select fields before `project` is consumed into `params` (#818,
    // AB#717). `label_source` is needed by every source arm; the bitbucket_* fields only
    // by the Bitbucket arm.
    let source_kind = project.source_kind;
    let azure_org = project.azure_org.clone();
    let azure_project = project.azure_project.clone();
    let label_source = project.label_source;
    let bitbucket_host = project.bitbucket_host.clone();
    let bitbucket_project = project.bitbucket_project.clone();
    let bitbucket_token = project.bitbucket_token.clone();
    let params = MonitorParams {
        repo: project.repo,
        authors: project.authors,
        pr_cooldown_seconds: project.pr_cooldown_seconds,
    };
    let cfg = config_service::load(app)?;
    let trigger_labels = config_service::rule_interest_labels(&cfg.rules, project_id);

    // Lock-free ledger read (no `LEDGER_WRITE_LOCK`): a stale-by-one-round snapshot is
    // fine here — this gate is an optimization, and the session registry's atomic
    // `try_reserve_pair` test-and-set is the real double-dispatch backstop (see
    // `Ledger::load`). The write path (`record_dispatched`) is the half that locks.
    let ledger = Ledger::load(app, project_id)?;
    let now = now_epoch();

    // Source selection (#818): branch on `source_kind` via an EXHAUSTIVE match (no
    // wildcard, no `dyn`) so a new `SourceKind` variant fails to compile here until it is
    // wired. Each source's `discover_events` (AB#1070) returns normalized `DiscoveredEvent`s
    // (an `Event` carrying title / url / all-labels + the gating `Candidate` + a both-label
    // `conflict` flag), so the view path is IDENTICAL across sources — each arm just feeds
    // its events to `build_view`.
    let (views, dispatchable) = match source_kind {
        SourceKind::Github => {
            let source = GithubCli::new(
                config_service::resolve_cli(app, CliTool::Gh, false)?,
                params.repo.clone(),
                trigger_labels.clone(),
                label_source,
            );
            partition_events(source.discover_events().await?, &params, &ledger, now)
        }
        SourceKind::Azure => {
            // Defense-in-depth (#818 F2): the reschedule/reconcile path reaches here via the
            // NON-validated `config_service::load`, so a hand-edited / partially-migrated
            // Azure project could carry empty org/project. Guard before building the URL —
            // `az` would otherwise emit a confusing CLI error. (`validate_project` is the
            // primary gate on the save path; this is the belt-and-braces backstop.)
            if azure_org.trim().is_empty() || azure_project.trim().is_empty() {
                return Err(crate::error::AppError::new(
                    "Azure 源未配置 azureOrg / azureProject（请在设置中补全）",
                ));
            }
            let source = AzureDevOpsCli::new(
                config_service::resolve_cli(app, CliTool::Az, false)?,
                azure_org,
                azure_project,
                params.repo.clone(),
                trigger_labels.clone(),
                label_source,
            );
            partition_events(source.discover_events().await?, &params, &ledger, now)
        }
        SourceKind::Bitbucket => {
            // Defense-in-depth (parity with the Azure arm): the reschedule/reconcile path
            // reaches here via the NON-validated `config_service::load`, so a hand-edited /
            // partially-migrated Bitbucket project could carry empty host/project/token.
            // Guard before building the HTTP client. (`validate_project` is the primary gate
            // on the save path; this is the belt-and-braces backstop.)
            if bitbucket_host.trim().is_empty()
                || bitbucket_project.trim().is_empty()
                || bitbucket_token.trim().is_empty()
                || params.repo.trim().is_empty()
            {
                return Err(crate::error::AppError::new(
                    "Bitbucket 源未配置 bitbucketHost / bitbucketProject / bitbucketToken / repo（请在设置中补全）",
                ));
            }
            let source = BitbucketServer::new(super::bitbucket::BitbucketSourceConfig {
                host: bitbucket_host,
                project: bitbucket_project,
                repo: params.repo.clone(),
                token: bitbucket_token,
                trigger_labels: trigger_labels.clone(),
                label_source,
            });
            partition_events(source.discover_events().await?, &params, &ledger, now)
        }
    };
    Ok((views, dispatchable))
}

/// Reconciles the per-project scheduler set to the enabled projects, only when the
/// persisted config validates — returning the validation error (without starting any
/// loop) otherwise.
///
/// **Multi-project semantics (#35):** this is "start ALL enabled projects". It
/// delegates to [`crate::pr::scheduler::SchedulerSet::reconcile`], which is
/// idempotent — it creates a loop for each newly-enabled project, stops + drops loops
/// for disabled/removed ones, and reconfigures survivors. So a launch, a `start_polling`,
/// and a post-save `reschedule` all funnel through the same reconcile.
///
/// The single enforcement point for the "no poll loop under an invalid config"
/// funnel (PR #41 F1). BOTH entry paths go through here, so neither can start any loop
/// on a config that fails [`config_service::load_validated`]:
/// - launch (`lib.rs` setup) discards the `Err` so a first launch (empty `projects`
///   default) routes to onboarding instead of polling;
/// - the public `start_polling` command surfaces the `Err` to the frontend.
///
/// `load_validated` returns the validated [`AppConfig`], so `reconcile` reads its
/// `projects` directly (no second load — the validated snapshot is the source).
pub(crate) fn start_if_config_valid<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &crate::state::AppState,
) -> AppResult<()> {
    let cfg = config_service::load_validated(app)?;
    state.scheduler.reconcile(app, &cfg.projects);
    Ok(())
}

/// Starts polling for ALL enabled projects (#35). Idempotent — reconciles the
/// scheduler set to the enabled projects (creates new ones, stops removed/disabled
/// ones, reconfigures survivors). Errors without starting any loop when the persisted
/// config is invalid (see [`start_if_config_valid`]), so no loop runs under a bad
/// config — the frontend surfaces the returned error. Takes no `project_id`: the
/// set always mirrors exactly the enabled projects, so there is no per-project
/// "start" — enabling a project (then saving + reconciling) is how it joins.
#[tauri::command]
pub async fn start_polling<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<()> {
    start_if_config_valid(&app, state.inner())
}

/// Stops polling for ALL projects (#35) — `stop_all` tears down every running loop
/// and empties the set (no-op if none running). This is the global pause; a later
/// `start_polling` / `reschedule` rebuilds the enabled loops from scratch. There is
/// deliberately no per-project stop command: disabling a project in config + saving
/// (which reconciles) stops just that one, keeping config the single source of which
/// projects run.
#[tauri::command]
pub async fn stop_polling(state: tauri::State<'_, crate::state::AppState>) -> AppResult<()> {
    state.scheduler.stop_all();
    Ok(())
}

/// The `poll_now` decision (#818 F6): what an immediate-pull request should DO, computed
/// purely from `(loop_running, update_mode)` so every branch is unit-tested without a
/// Tauri handle. [`poll_now`] is the thin IO shell that realizes the chosen action.
#[derive(Debug, PartialEq, Eq)]
enum PollNowAction {
    /// A running loop (pull-only / hybrid) → wake it; its cycle emits `prs:updated`.
    Wake,
    /// No loop, manual mode → run ONE one-shot CLI discovery (manual's only pull path).
    OneShot,
    /// No loop, webhook-only → reject: there is no CLI source to pull from in this mode.
    RejectWebhookOnly,
    /// No loop, pull-only / hybrid → the loop is paused (e.g. after `stop_polling`);
    /// reject so the frontend prompts to resume.
    RejectPaused,
}

/// PURE `poll_now` decision over `(loop_running, mode)` (#818 F6). EXHAUSTIVE match on
/// [`UpdateMode`] (no wildcard) so a new mode must be classified here. A running loop
/// always [`Wake`](PollNowAction::Wake)s regardless of mode (it only runs for pull-only /
/// hybrid anyway); with no loop the mode decides: `Manual` → one-shot, `WebhookOnly` →
/// reject (no source), `PullOnly`/`Hybrid` → paused.
fn poll_now_action(loop_running: bool, mode: UpdateMode) -> PollNowAction {
    if loop_running {
        return PollNowAction::Wake;
    }
    match mode {
        UpdateMode::Manual => PollNowAction::OneShot,
        UpdateMode::WebhookOnly => PollNowAction::RejectWebhookOnly,
        UpdateMode::PullOnly | UpdateMode::Hybrid => PollNowAction::RejectPaused,
    }
}

/// Triggers an immediate discovery cycle for `project_id` ("立即拉取", #35, #818). Thin IO
/// shell over the pure [`poll_now_action`]: it realizes the chosen [`PollNowAction`].
///
/// `wake` both probes AND wakes atomically — when it returns `true` a running loop was
/// woken (the `Wake` action; its cycle emits `prs:updated`). Only when no loop is running
/// do we read the project's [`UpdateMode`] and dispatch the no-loop branch:
/// - `Manual` → one-shot discovery ([`SchedulerSet::discover_once`]): manual has no
///   periodic loop, so this explicit pull (or an inbound webhook) is its only refresh.
/// - `WebhookOnly` → reject (no CLI source to pull from; the list updates on webhooks).
/// - `PullOnly` / `Hybrid` → the loop is paused; reject so the frontend prompts to resume.
#[tauri::command]
pub async fn poll_now<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
    project_id: &str,
) -> AppResult<()> {
    // `wake` is atomic probe-and-wake. On success the loop is running → `Wake` realized.
    if state.scheduler.wake(project_id) {
        return Ok(());
    }
    // No running loop: read the mode and realize the no-loop action. The wake-check above
    // and this config read are NOT one atomic step, but the window is a KNOWN ACCEPTABLE
    // one (#818 F12): during a mode switch this could at most mis-respond ONCE (e.g. report
    // "paused" for a loop that just started, or skip a wake for one that just stopped) — a
    // user retry succeeds. It is not a correctness bug, so no lock spans the two reads.
    let mode = config_service::project(&app, project_id)?.update_mode;
    match poll_now_action(false, mode) {
        // `false` here: we only reach this after `wake` returned false (no running loop).
        PollNowAction::Wake => Ok(()), // unreachable with loop_running=false, but total.
        PollNowAction::OneShot => {
            // One-shot CLI discovery (manual has no periodic loop). Drives the same
            // per-cycle body the loop would, so it discovers / persists / emits / dispatches.
            state.scheduler.discover_once(&app, project_id).await
        }
        PollNowAction::RejectWebhookOnly => Err(crate::error::AppError::new(
            "webhook-only 模式不支持手动拉取",
        )),
        PollNowAction::RejectPaused => Err(crate::error::AppError::new("轮询已暂停，请先恢复轮询")),
    }
}

/// Re-reads every project's poll period and reconciles the scheduler set after a
/// config save (#35). Reconcile is idempotent: it adds loops for newly-enabled
/// projects, stops loops for disabled/removed ones, and rebuilds each survivor's
/// ticker (the immediate first tick also re-polls). Reads the persisted config to get
/// the current `projects`; a read failure surfaces to the frontend. NOT validated
/// (unlike `start_polling`): a save already validated, and reconcile only acts on
/// `enabled` projects whose fields the save checked.
#[tauri::command]
pub async fn reschedule<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<()> {
    let cfg = config_service::load(&app)?;
    state.scheduler.reconcile(&app, &cfg.projects);
    Ok(())
}

/// Reports `gh` CLI auth status for the StatusBar.
#[tauri::command]
pub async fn gh_status<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> AppResult<GhStatus> {
    Ok(
        match config_service::resolve_cli(&app, CliTool::Gh, false) {
            Ok(gh) => gh_auth_status(&gh).await,
            Err(error) => GhStatus {
                authenticated: false,
                message: error.message,
            },
        },
    )
}

/// Reports `az` CLI auth status for the StatusBar (Azure source). Mirrors `gh_status`:
/// no args, hardcodes the PATH-resolved `az` binary; the probe never errors.
#[tauri::command]
pub async fn az_status<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> AppResult<AzStatus> {
    Ok(
        match config_service::resolve_cli(&app, CliTool::Az, false) {
            Ok(az) => az_auth_status(&az).await,
            Err(error) => AzStatus {
                authenticated: false,
                message: error.message,
            },
        },
    )
}

/// Returns `project_id`'s retained tracked-PR list (#35) — that project's persisted
/// `tracked_pr` rows projected at the current epoch — so the frontend can render
/// the active project's state on mount without waiting for the next `prs:updated`
/// event (closes the startup lost-event race). Reads only `app` (+ the project id):
/// the persisted set survives restarts, so this no longer depends on the scheduler
/// having run this session. The grace window is resolved from THAT project's period.
#[tauri::command]
pub fn get_prs<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: &str,
) -> AppResult<Vec<crate::model::TrackedPrView>> {
    let tracked = super::registry::TrackedPrs::load(&app, project_id)?;
    Ok(super::registry::project_snapshot(
        &tracked, &app, project_id,
    ))
}

/// Sets a tracked PR's `archived` flag within `project_id`'s set (#35) and re-emits
/// that project's retained list immediately so the UI reflects the archive/unarchive
/// without waiting for the next poll round. PRs are never auto-evicted; archiving is
/// how users retire inactive rows.
///
/// Routes through the registry's single serialized write seam ([`registry::mutate_tracked`],
/// F1) scoped to `project_id` so this can't interleave with a poll-cycle upsert and lose
/// a write. An unknown / raced `number` is a benign no-op: `set_archived` reports no
/// change, the closure returns `persist == false` so the seam skips the store write, and
/// we emit nothing — no phantom write/event — returning `Ok` rather than erroring. On a
/// real change we re-emit the retained projection (built inside the seam, under the lock,
/// carrying `project_id`) so the list updates immediately.
#[tauri::command]
pub fn set_pr_archived<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: String,
    number: u64,
    archived: bool,
) -> AppResult<()> {
    let emitted = super::registry::mutate_tracked(&app, &project_id, |tracked| {
        if tracked.set_archived(number, archived) {
            (
                true,
                Some(super::registry::project_snapshot(
                    tracked,
                    &app,
                    &project_id,
                )),
            )
        } else {
            (false, None) // unknown number — nothing changed, skip persist + emit.
        }
    })?;
    if let Some(list) = emitted {
        crate::stream::emit(
            &app,
            StreamEvent::Pr(PrEvent::Updated {
                project_id,
                prs: list,
            }),
        );
    }
    Ok(())
}

/// Annotate a webhook-sourced candidate into a [`PullRequestView`] + dispatch decision
/// (#61), applying the SAME static + cooldown gates the poll path applies via
/// [`build_view`] — minus the conflict branch (the caller handles conflict / StatusOnly
/// upstream from the parsed [`IngestIntent`], so this only ever sees a single-label
/// candidate). The push-path counterpart to `build_view`: `skip_reason = should_skip(..)
/// .or_else(|| cooldown_skip(..))`; `dispatchable = skip_reason.is_none().then(..)`, so a
/// clean candidate dispatches and a gated one (draft / fork / disallowed-author /
/// within-cooldown) becomes a skipped row with NO dispatch — dispatch parity with that
/// project's scheduler, so nothing slips through just because it arrived by webhook.
///
/// The view's title / labels / url come from the webhook payload (passed in by the
/// caller), so the list row is built without a `gh pr view` round trip.
///
/// Pure (no `AppHandle`) so the gate composition is unit-tested without a Tauri handle —
/// it replaces the deleted `gate_candidates` parity test (it asserts BOTH the static and
/// the cooldown gate apply; the predicates themselves are tested at their source in
/// `discover.rs`).
fn webhook_view(
    cand: Candidate,
    title: String,
    labels: Vec<String>,
    url: String,
    params: &MonitorParams,
    ledger: &Ledger,
    now: u64,
) -> (PullRequestView, Option<Candidate>) {
    let skip_reason = discover::should_skip(&cand, params, ledger)
        .or_else(|| discover::cooldown_skip(&cand, params, ledger, now));
    // Clone for dispatch only when it passes every static + cooldown gate; a gated row
    // contributes a view but no dispatch candidate (parity with `build_view`).
    let dispatchable = skip_reason.is_none().then(|| cand.clone());
    let view = PullRequestView {
        number: cand.number,
        title,
        labels,
        url,
        skill_key: cand.skill_key,
        skip_reason,
    };
    (view, dispatchable)
}

/// The full ingest decision for ONE routed [`WebhookEvent`], computed PURELY from plain
/// data by [`decide_ingest`] (no `AppHandle`) so EVERY branch is unit-tested. The
/// AppHandle-bound [`ingest_webhook`] becomes a thin shell that just performs the IO this
/// describes: `write` the row through the serialized seam, emit, return the gated candidate for
/// rule processing, and record the delivery with `status` / `message`.
struct IngestDecision {
    /// The list-row view to persist (Upsert) or refresh (UpdatePresent).
    view: PullRequestView,
    /// Which registry write to perform for `view`.
    write: WriteKind,
    /// The candidate that passed the same static + cooldown gate as polling. Rule execution may
    /// consume this; gated / conflict / status-only paths return `None`.
    dispatchable: Option<Candidate>,
    /// The single terminal delivery diagnostic status (#62) — finalized HERE from the gate/list
    /// decision, so the shell records it verbatim.
    status: DeliveryStatus,
    /// The delivery diagnostic's human-readable note (skip reason / `None` for a clean
    /// dispatch).
    message: Option<String>,
}

/// PURE webhook-ingest decision (#61/#62): maps a parsed [`IngestIntent`] + the project's
/// gating inputs to the FULL [`IngestDecision`] (view + write + gated candidate + terminal
/// delivery status + message), WITHOUT any `AppHandle` so every branch is unit-tested.
/// Extracted from the old inline `ingest_webhook` body so the decision path — including the
/// #61 core "webhook still LISTS the PR before rule actions" — has automated coverage rather than
/// only a "verified via integration / manual verify" note. Behavior-preserving: the IO shell
/// ([`ingest_webhook`]) feeds it the same data the inline match consumed and acts on its
/// output verbatim.
///
/// Branch semantics (each preserved from the inline body):
/// - `Track { candidate: Some(cand), .. }`: run the SAME static + cooldown gates as the
///   poll path (via [`webhook_view`]). `write = Upsert`. Gated (skip_reason `Some`) →
///   `dispatchable = None`, `status = Gated`, `message = skip_reason`. Clean (skip_reason
///   `None`) → `dispatchable = Some(cand)`, `status = ListUpdated`, `message = None`.
/// - `Track { candidate: None, conflict: .. }`: both trigger labels → a skipped "review"
///   row with the conflict reason; `write = Upsert`; no dispatch; `status = Gated`.
/// - `StatusOnly { skill_key }`: refresh an EXISTING row's status (`write = UpdatePresent`),
///   never insert / dispatch. Reason text + terminal status BOTH come from the type-locked
///   [`StatusOnlyKind`] (no string compare — see ai-robust.md).
// The flat plain-data arg list (the row metadata + the gating inputs) is deliberate: this
// is a PURE decision seam whose whole point is to be callable from a `#[test]` with no
// `AppHandle`, so it takes exactly the data the IO shell already holds rather than an
// AppHandle-bound bundle. Bundling into a struct would just move the arg count around and
// add a single-use type — same as `review::session::start_review`'s allow.
#[allow(clippy::too_many_arguments)]
fn decide_ingest(
    intent: IngestIntent,
    number: u64,
    title: String,
    labels: Vec<String>,
    url: String,
    params: &MonitorParams,
    ledger: &Ledger,
    now: u64,
) -> IngestDecision {
    match intent {
        IngestIntent::Track {
            candidate: Some(cand),
            ..
        } => {
            let (view, dispatchable) = webhook_view(cand, title, labels, url, params, ledger, now);
            // A single trigger label. A gated one (draft/fork/author/cooldown) is a `Gated`
            // row carrying the reason. A clean candidate enters the list and is returned to the
            // rule engine; rules decide whether to enqueue review/check/notify.
            match (dispatchable, &view.skip_reason) {
                (Some(cand), _) => IngestDecision {
                    status: DeliveryStatus::ListUpdated,
                    message: None,
                    write: WriteKind::Upsert,
                    dispatchable: Some(cand),
                    view,
                },
                (None, reason) => IngestDecision {
                    status: DeliveryStatus::Gated,
                    message: reason.clone(),
                    write: WriteKind::Upsert,
                    dispatchable: None,
                    view,
                },
            }
        }
        IngestIntent::Track {
            candidate: None,
            conflict: _,
        } => {
            // Both trigger labels (conflict): a skipped row, never dispatched. Kind "review"
            // for the view (the parse picked review for the conflict view).
            let view = PullRequestView {
                number,
                title,
                labels,
                url,
                skill_key: crate::model::SkillInvocation::skill_key("pr-review", ""),
                skip_reason: Some(discover::BOTH_TRIGGER_LABELS_REASON.to_string()),
            };
            IngestDecision {
                view,
                write: WriteKind::Upsert,
                dispatchable: None,
                status: DeliveryStatus::Gated,
                message: Some(discover::BOTH_TRIGGER_LABELS_REASON.to_string()),
            }
        }
        IngestIntent::StatusOnly {
            skill_key: status_kind,
        } => {
            // Closed/merged or trigger-label-removed: refresh an existing row's status,
            // never insert, never dispatch. `skill_key` from the current labels (check vs the
            // review default). The reason text + terminal delivery status both come from the
            // type-locked `StatusOnlyKind` (no string compare — see FIX 1 / ai-robust.md).
            let reason = status_kind.reason().to_string();
            let view = PullRequestView {
                number,
                title,
                labels,
                url,
                skill_key: crate::model::SkillInvocation::skill_key("pr-review", ""),
                skip_reason: Some(reason.clone()),
            };
            IngestDecision {
                view,
                write: WriteKind::UpdatePresent,
                dispatchable: None,
                status: status_kind.delivery_status(),
                message: Some(reason),
            }
        }
    }
}

/// The AppHandle-bound webhook ingest (#61): the body of the [`WebhookIngestor`] the
/// composition root installs. A THIN IO shell around the pure [`decide_ingest`]: it
/// resolves config + ledger (fail-closed), calls `decide_ingest`, then performs only the
/// AppHandle-bound IO — the registry `write`, the `prs:updated` emit, and the single delivery
/// record. The branch LOGIC (view + write + dispatchable +
/// terminal status + message) lives in `decide_ingest` and is unit-tested there; the
/// remaining `mutate_tracked` / `emit` here is the untestable AppHandle shell.
/// Takes ONE parsed, routed [`WebhookEvent`] and (a) upserts /
/// updates the persisted PR list row, (b) emits `prs:updated` so the list reflects the
/// push WITHOUT waiting for the next poll round, and (c) returns the gated-clean candidate to the
/// composition root for rule processing. Records EXACTLY ONE delivery diagnostic at the end
/// (#62) — the handler records the early-exit classifications, this records the routable
/// terminal status.
///
/// **Fail-closed (parity with the deleted `gate_dispatchable`'s fail-closed reads):** an
/// unresolvable project / unreadable config OR an unreadable ledger records a `Gated`
/// delivery with the error message and RETURNS without upsert/emit/rule processing. The ledger
/// is the dedup/cooldown source of truth — degrading it to an empty ledger would pass
/// EVERY cooldown/dedup gate and re-review storm, so an unprovable "not a recent
/// duplicate" must fail closed, matching the poll path (`discover` uses
/// `Ledger::load(app, project_id)?`).
///
/// Reuses the registry's single serialized write seam ([`registry::mutate_tracked`]) for
/// the upsert + emit (so this can't interleave with a poll-cycle upsert / `set_pr_archived`
/// and lose a write).
pub(crate) async fn ingest_webhook<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    ev: WebhookEvent,
) -> AppResult<Option<Candidate>> {
    let WebhookEvent {
        project_id,
        received_at,
        event,
        action,
        repo,
        number,
        title,
        labels,
        url,
        intent,
    } = ev;

    // Resolve config + ledger, failing closed on either error (see the fn doc). On a
    // failure we record a `Gated` delivery with the error and return without touching the
    // list — fail-closed parity with the deleted `gate_dispatchable`. `received_at` is the
    // handler-stamped receipt time, so this terminal record shares the early-exit time base.
    let record_failclosed = |app: &tauri::AppHandle<R>, msg: String| {
        record_webhook_delivery(
            app,
            WebhookDelivery {
                received_at_epoch: received_at,
                event: event.clone(),
                action: action.clone(),
                repo: Some(repo.clone()),
                pr_number: Some(number),
                skill_key: None,
                status: DeliveryStatus::Gated,
                message: Some(msg),
            },
        );
    };
    let project = match config_service::project(app, &project_id) {
        Ok(p) => p,
        Err(e) => {
            record_failclosed(app, format!("项目配置不可读：{}", e.message));
            return Ok(None);
        }
    };
    let params = MonitorParams {
        repo: project.repo,
        authors: project.authors,
        pr_cooldown_seconds: project.pr_cooldown_seconds,
    };
    let ledger = match Ledger::load(app, &project_id) {
        Ok(l) => l,
        Err(e) => {
            record_failclosed(app, format!("ledger 不可读：{}", e.message));
            return Ok(None);
        }
    };
    let now = now_epoch();

    // The full ingest decision (view + write + dispatch + terminal status + message) is
    // computed by the PURE `decide_ingest` (unit-tested per branch); the rest of this fn is
    // the thin AppHandle-bound IO shell that acts on it.
    let IngestDecision {
        view,
        write: write_kind,
        dispatchable,
        status: final_status,
        message,
    } = decide_ingest(intent, number, title, labels, url, &params, &ledger, now);

    // Persist + emit through the single serialized write seam. The Upsert path always
    // persists + emits (an upsert always changes the set); the UpdatePresent path persists
    // + emits ONLY when the row existed (mirrors `set_pr_archived`'s no-op skip), so a
    // status-only event for an untracked PR is a benign no-op.
    let emitted = super::registry::mutate_tracked(app, &project_id, |tracked| match write_kind {
        WriteKind::Upsert => {
            tracked.upsert(std::slice::from_ref(&view), now);
            (
                true,
                Some(super::registry::project_snapshot(tracked, app, &project_id)),
            )
        }
        WriteKind::UpdatePresent => {
            if tracked.update_present(&view, now) {
                (
                    true,
                    Some(super::registry::project_snapshot(tracked, app, &project_id)),
                )
            } else {
                (false, None) // untracked PR — nothing changed, skip persist + emit.
            }
        }
    });
    match emitted {
        Ok(Some(list)) => {
            crate::stream::emit(
                app,
                StreamEvent::Pr(PrEvent::Updated {
                    project_id: project_id.clone(),
                    prs: list,
                }),
            );
        }
        // Persist no-op (untracked status-only PR) — nothing to emit.
        Ok(None) => {}
        // A store failure surfaces via `PrEvent::Error` (the project's error banner) —
        // SYMMETRIC with the poll path (`scheduler::persist_event` emits `PrEvent::Error`
        // on a persist failure). The delivery record below still reflects the gate decision; rule
        // processing happens in the composition root after this function returns. The list is
        // unchanged, but the banner tells the user the persist failed.
        Err(e) => {
            crate::stream::emit(
                app,
                StreamEvent::Pr(PrEvent::Error {
                    project_id: project_id.clone(),
                    message: format!("PR 列表持久化失败：{}", e.message),
                }),
            );
        }
    }

    // Record the single terminal delivery diagnostic (#62) for this routable event. The
    // status came straight from `decide_ingest` — the dispatch decision above only acts on
    // it, it does not override it.
    record_webhook_delivery(
        app,
        WebhookDelivery {
            received_at_epoch: received_at,
            event,
            action,
            repo: Some(repo),
            pr_number: Some(number),
            skill_key: Some(view.skill_key.to_string()),
            status: final_status,
            message,
        },
    );
    Ok(dispatchable)
}

/// Record one webhook-delivery diagnostic into the manager's ring (#62) via `AppState`. A thin
/// AppState-lookup wrapper: callers build the [`WebhookDelivery`] (so `received_at_epoch` is the
/// handler-stamped receipt time, consistent with the early-exit records, and the secret/token
/// is never put in the record by construction).
fn record_webhook_delivery<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    delivery: WebhookDelivery,
) {
    use tauri::Manager;
    app.state::<crate::state::AppState>()
        .webhook
        .record_delivery(delivery);
}

/// Whether a project's [`UpdateMode`] accepts inbound webhook routes (#124 F1). The
/// push-update counterpart to `scheduler::periodic_polling`: a wildcard-free EXHAUSTIVE
/// match (so a new mode must be classified) — `WebhookOnly` / `Hybrid` / `Manual` accept
/// webhooks (Manual lists webhooks as one of its refresh paths per the [`UpdateMode`] doc),
/// while `PullOnly` is EXCLUDED: its periodic CLI poll is the SOLE update source, so a push
/// must not enter / auto-dispatch it. Locked by `webhook_route_eligible_excludes_only_pull_only`.
fn webhook_route_eligible(mode: UpdateMode) -> bool {
    match mode {
        UpdateMode::WebhookOnly | UpdateMode::Hybrid | UpdateMode::Manual => true,
        UpdateMode::PullOnly => false,
    }
}

fn resolve_webhook_cloudflared<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    mode: crate::model::WebhookTunnelMode,
    tunnel_command: &str,
) -> AppResult<Option<config_service::ResolvedCli>> {
    let needs_cloudflared = mode == crate::model::WebhookTunnelMode::Quick
        || (mode == crate::model::WebhookTunnelMode::Command
            && config_service::tunnel_command_uses_bare_cloudflared(tunnel_command));
    if needs_cloudflared {
        config_service::resolve_cli(app, CliTool::Cloudflared, false).map(Some)
    } else {
        Ok(None)
    }
}

/// Starts the webhook receiver + Cloudflare Quick Tunnel. Requires `webhook_enabled`
/// in the persisted config; returns the resolved status (incl. the public
/// `*.trycloudflare.com` URL to paste into GitHub). The local server binds
/// `127.0.0.1` only — the public path is the tunnel.
///
/// **Multi-project (#35):** ONE receiver serves every project. The handler routes each
/// incoming event by repo, so this builds a [`super::webhook::ProjectRoute`] from every
/// `enabled` project (its repo + trigger labels + id) and hands the snapshot to
/// [`super::webhook::WebhookManager::start`]. The receiver params (port / secret /
/// cloudflared credential / tunnel) stay GLOBAL on [`AppConfig`]. The route snapshot is fixed
/// for the runtime's life — a project add/remove requires a webhook restart (the
/// composition root wires that on `set_config`).
///
/// Uses `load_validated` (not bare `load`) so the SAME `validate` the wizard / poll
/// path enforce gates the start: when `webhook_enabled`, an empty `webhookSecret` or a
/// zero `webhookPort` (a hand-edited config that would bind a random port) is rejected
/// HERE, before cloudflared spawns — parity with `start_polling` / `start_review`.
/// `validate` only constrains the webhook fields WHEN enabled, so the explicit
/// `webhook_enabled` check below still owns the "enable it first" message.
#[tauri::command]
pub async fn start_webhook<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<WebhookStatus> {
    let cfg = config_service::load_validated(&app)?;
    if !cfg.webhook_enabled {
        return Err(crate::error::AppError::new(
            "请先在设置中启用 Webhook 并保存配置",
        ));
    }
    // One route per enabled, webhook-eligible project (#124 F1): a `pull-only` project's
    // periodic CLI poll is its SOLE update source, so it gets NO route (a push must not
    // update / auto-dispatch it). The handler matches an event's repo against these and
    // tags the dispatched candidate with the owning project's id (#35).
    //
    // AB#717: Bitbucket Server has NO inbound webhook handler, so a Bitbucket project never
    // gets a route — without this filter a manual-mode Bitbucket project (webhook-eligible)
    // would be added but every delivery would silently `WrongRepo`. (Config also rejects
    // webhook-only / hybrid for Bitbucket, so only manual could otherwise reach here.)
    let routes: Vec<super::webhook::ProjectRoute> = cfg
        .projects
        .iter()
        .filter(|p| {
            p.enabled
                && webhook_route_eligible(p.update_mode)
                && p.source_kind != SourceKind::Bitbucket
        })
        .map(|p| super::webhook::ProjectRoute {
            id: p.id.clone(),
            // AB#822: the handler routes Azure events only to Azure routes and GitHub events
            // only to GitHub routes; azure_project guards an Azure event to the matching
            // project (config rejects duplicate bare repos, so repo is globally unique).
            source_kind: p.source_kind,
            repo: p.repo.clone(),
            azure_project: p.azure_project.clone(),
            // AB#717: the GitHub webhook path classifies from the payload, so it must honor
            // the project's label source (native vs title-parsed). The Azure path re-runs
            // `az` discovery (which already honors labelSource in `parse_rows`), so it does
            // not read this field. (Bitbucket has no route — filtered out above.)
            label_source: p.label_source,
        })
        .collect();
    let cloudflared =
        resolve_webhook_cloudflared(&app, cfg.webhook_tunnel_mode, &cfg.webhook_tunnel_command);
    state
        .webhook
        .start(
            cfg.webhook_port,
            cfg.webhook_secret,
            routes,
            cloudflared,
            super::webhook::TunnelSpec {
                mode: cfg.webhook_tunnel_mode,
                command: cfg.webhook_tunnel_command,
                public_url: cfg.webhook_public_url,
            },
        )
        .await
}

/// Stops the webhook receiver + tunnel (no-op if not running). Returns the post-stop
/// status (so the UI reflects `running: false` + the current cloudflared install state).
#[tauri::command]
pub async fn stop_webhook<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<WebhookStatus> {
    state.webhook.stop().await;
    let cfg = config_service::load(&app)?;
    let cloudflared =
        resolve_webhook_cloudflared(&app, cfg.webhook_tunnel_mode, &cfg.webhook_tunnel_command);
    Ok(state
        .webhook
        .status(
            cloudflared,
            cfg.webhook_tunnel_mode,
            &cfg.webhook_tunnel_command,
        )
        .await)
}

/// Reports webhook receiver + tunnel status (running, public URL, cloudflared install)
/// for the settings panel.
#[tauri::command]
pub async fn webhook_status<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<WebhookStatus> {
    let cfg = config_service::load(&app)?;
    let cloudflared =
        resolve_webhook_cloudflared(&app, cfg.webhook_tunnel_mode, &cfg.webhook_tunnel_command);
    Ok(state
        .webhook
        .status(
            cloudflared,
            cfg.webhook_tunnel_mode,
            &cfg.webhook_tunnel_command,
        )
        .await)
}

/// Snapshot of the webhook-delivery diagnostics ring (#62) for the settings panel —
/// the recent window of "did GitHub reach us, and what did we do with each delivery".
/// Oldest→newest; capped at the manager's ring size. Never carries the secret/token.
#[tauri::command]
pub async fn webhook_deliveries(
    state: tauri::State<'_, crate::state::AppState>,
) -> AppResult<Vec<WebhookDelivery>> {
    Ok(state.webhook.deliveries_snapshot())
}

/// Reports `project_id`'s poll-loop status (#62) for the settings panel: whether the
/// loop is running, its resolved interval, and the last cycle's diagnostics (started /
/// success / error / persist epochs + discovered count). Pulled on demand — NO new event
/// type, so the `events.rs` union stays untouched.
#[tauri::command]
pub async fn poll_status<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, crate::state::AppState>,
    project_id: &str,
) -> AppResult<PollStatus> {
    Ok(state.scheduler.poll_status(&app, project_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Candidate, ReviewActionKey};

    fn action_key(pr: u64, head: &str, skill_key: impl AsRef<str>) -> String {
        let skill_key = skill_key.as_ref();
        ReviewActionKey::for_parts(
            pr,
            head,
            crate::model::SkillInvocation::migrate_legacy_skill_key(skill_key),
        )
        .unwrap()
        .into_inner()
    }

    fn params() -> MonitorParams {
        MonitorParams {
            repo: "o/r".to_string(),
            authors: vec![],
            pr_cooldown_seconds: 1800,
        }
    }

    // AB#1070: the source-agnostic discovered row is now a `DiscoveredEvent` (normalized
    // `Event` + gating `Candidate`). `.candidate` still surfaces for the gating-only tests.
    fn row(number: u64, skill_key: impl AsRef<str>, conflict: bool) -> DiscoveredEvent {
        let skill_key = skill_key.as_ref();
        use crate::model::{EventEnvelope, EventSubject, EventType, InboxDedupeKey};
        let candidate = Candidate {
            number,
            head_sha: "sha".to_string(),
            head_ref: "ref".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            skill_key: crate::model::SkillInvocation::migrate_legacy_skill_key(skill_key),
        };
        DiscoveredEvent {
            event: EventEnvelope::observation(
                InboxDedupeKey::new(format!("github:pullRequest:o/r#{number}@sha")).unwrap(),
                SourceKind::Github,
                "discovery",
                "o/r",
                EventType::PullRequest,
                EventSubject {
                    number: Some(number),
                    title: format!("PR {number}"),
                    body: String::new(),
                    labels: vec!["review-label".to_string()],
                    url: format!("https://x/{number}"),
                },
                0,
            )
            .unwrap(),
            candidate,
            conflict,
        }
    }

    // #818 / AB#717: `build_view_parts` is the source-agnostic core all three arms feed —
    // GitHub (`GhRow`), Azure (`AzRow`), and Bitbucket (`BbRow`). It applies the SAME gate
    // composition as the old per-source
    // builders — conflict short-circuits to the both-labels reason, otherwise static then
    // cooldown — and carries the row's real title / url / labels through to the view, so an
    // Azure row now has FULL display parity with GitHub. A clean row dispatches; a conflict
    // / gated one is a skipped row with no dispatch.
    #[test]
    fn build_view_parts_clean_row_carries_display_fields_and_is_dispatchable() {
        let cand = row(1, "review", false).candidate;
        let (view, disp) = build_view_parts(
            cand,
            "Real title".to_string(),
            vec!["review-label".to_string(), "area/ui".to_string()],
            "https://dev.azure.com/o/p/_git/r/pullrequest/1".to_string(),
            false,
            &params(),
            &Ledger::default(),
            0,
        );
        assert_eq!(view.number, 1);
        assert_eq!(
            view.skill_key,
            crate::model::SkillInvocation::skill_key("pr-review", "")
        );
        assert_eq!(view.title, "Real title");
        assert_eq!(view.url, "https://dev.azure.com/o/p/_git/r/pullrequest/1");
        assert_eq!(
            view.labels,
            vec!["review-label".to_string(), "area/ui".to_string()]
        );
        assert_eq!(view.skip_reason, None);
        let disp = disp.expect("clean row is dispatchable");
        assert_eq!(disp.number, 1);
    }

    #[test]
    fn build_view_parts_conflict_row_is_skipped_with_both_labels_reason() {
        let cand = row(2, "review", false).candidate;
        let (view, disp) = build_view_parts(
            cand,
            "Both".to_string(),
            vec!["review-label".to_string(), "check-label".to_string()],
            "https://x/2".to_string(),
            true, // conflict
            &params(),
            &Ledger::default(),
            0,
        );
        assert_eq!(
            view.skip_reason,
            Some(discover::BOTH_TRIGGER_LABELS_REASON.to_string())
        );
        assert!(disp.is_none(), "a conflict row is not dispatchable");
    }

    #[test]
    fn build_view_parts_draft_is_skipped_and_not_dispatchable() {
        let mut cand = row(3, "review", false).candidate;
        cand.is_draft = true;
        let (view, disp) = build_view_parts(
            cand,
            "Draft".to_string(),
            vec!["review-label".to_string()],
            "https://x/3".to_string(),
            false,
            &params(),
            &Ledger::default(),
            0,
        );
        assert_eq!(view.skip_reason, Some("draft PR".to_string()));
        assert!(disp.is_none(), "a gated row is not dispatchable");
    }

    // #818 F6: the pure `poll_now_action` decision table — every (loop_running, mode) cell.
    #[test]
    fn poll_now_action_running_loop_always_wakes() {
        // A running loop wakes regardless of mode (it only runs for pull-only/hybrid anyway).
        for mode in [
            UpdateMode::PullOnly,
            UpdateMode::Hybrid,
            UpdateMode::Manual,
            UpdateMode::WebhookOnly,
        ] {
            assert_eq!(poll_now_action(true, mode), PollNowAction::Wake);
        }
    }

    // #124 F1: a webhook route is built ONLY for a project whose mode accepts push updates.
    // `PullOnly`'s contract makes the periodic CLI poll the SOLE update source, so it must NOT
    // get a route (else a push could update + auto-dispatch it). WebhookOnly / Hybrid / Manual
    // all accept webhooks (Manual per the UpdateMode doc — webhook is one of its refresh paths).
    #[test]
    fn webhook_route_eligible_excludes_only_pull_only() {
        assert!(webhook_route_eligible(UpdateMode::WebhookOnly));
        assert!(webhook_route_eligible(UpdateMode::Hybrid));
        assert!(webhook_route_eligible(UpdateMode::Manual));
        assert!(!webhook_route_eligible(UpdateMode::PullOnly));
    }

    #[test]
    fn poll_now_action_no_loop_branches_by_mode() {
        assert_eq!(
            poll_now_action(false, UpdateMode::Manual),
            PollNowAction::OneShot
        );
        assert_eq!(
            poll_now_action(false, UpdateMode::WebhookOnly),
            PollNowAction::RejectWebhookOnly
        );
        assert_eq!(
            poll_now_action(false, UpdateMode::PullOnly),
            PollNowAction::RejectPaused
        );
        assert_eq!(
            poll_now_action(false, UpdateMode::Hybrid),
            PollNowAction::RejectPaused
        );
    }

    #[test]
    fn build_view_clean_row_has_no_skip_reason_and_is_dispatchable() {
        let (view, cand) = build_view(&row(1, "review", false), &params(), &Ledger::default(), 0);
        assert_eq!(view.number, 1);
        assert_eq!(
            view.skill_key,
            crate::model::SkillInvocation::skill_key("pr-review", "")
        );
        // AB#1070: display fields (title / url / labels) come from the `DiscoveredEvent.event`,
        // not the `Candidate` (which has no title/url/labels) — locks the event-as-display source.
        assert_eq!(view.title, "PR 1");
        assert_eq!(view.url, "https://x/1");
        assert_eq!(view.labels, vec!["review-label".to_string()]);
        assert_eq!(view.skip_reason, None);
        // Clean row (skip_reason None) → surfaced as a dispatchable candidate.
        let cand = cand.expect("clean row yields a dispatchable candidate");
        assert_eq!(cand.number, 1);
        assert_eq!(
            cand.skill_key,
            crate::model::SkillInvocation::skill_key("pr-review", "")
        );
    }

    #[test]
    fn build_view_conflict_row_skips_and_is_not_dispatchable() {
        let (view, cand) = build_view(&row(2, "check", true), &params(), &Ledger::default(), 0);
        assert_eq!(
            view.skip_reason,
            Some("both review and check trigger labels are present".to_string())
        );
        // Skipped row → no candidate for dispatch.
        assert!(cand.is_none());
    }

    #[test]
    fn build_view_propagates_static_gate_skip_and_omits_candidate() {
        let mut r = row(3, "review", false);
        r.candidate.is_draft = true;
        let (view, cand) = build_view(&r, &params(), &Ledger::default(), 0);
        assert_eq!(view.skip_reason, Some("draft PR".to_string()));
        assert!(cand.is_none());
    }

    #[test]
    fn build_view_propagates_cooldown_skip_and_omits_candidate() {
        use crate::pr::ledger::DispatchEvent;
        use std::collections::HashSet;

        let r = row(4, "review", false);
        let key = action_key(4, &r.candidate.head_sha, &r.candidate.skill_key);
        let ledger = Ledger {
            dispatched: HashSet::new(),
            events: vec![DispatchEvent {
                pr: 4,
                skill_key: r.candidate.skill_key.clone(),
                head_sha: r.candidate.head_sha.clone(),
                key,
                dispatched_at_epoch: 1_000,
            }],
        };
        // 1800s cooldown, dispatched 500s before `now` → within window.
        let (view, cand) = build_view(&r, &params(), &ledger, 1_500);
        assert!(
            view.skip_reason
                .as_deref()
                .unwrap()
                .contains("within cooldown"),
            "{:?}",
            view.skip_reason
        );
        assert!(cand.is_none());
    }

    // Migrated from the deleted `gate_candidates` parity test: `webhook_view` (#61) must
    // apply BOTH the static (already-dispatched) and the cooldown gate to a webhook-sourced
    // candidate — a clean one dispatches (Some), a gated one is a skipped row (None) — so a
    // push trigger has dispatch parity with the poll path. The predicates themselves are
    // tested at their source in `discover.rs`.
    #[test]
    fn webhook_view_applies_both_static_and_cooldown_gates() {
        use crate::pr::ledger::DispatchEvent;
        use std::collections::HashSet;

        let clean = row(1, "review", false).candidate;
        let dispatched = row(2, "review", false).candidate;
        let cooled = row(3, "review", false).candidate;

        let ledger = Ledger {
            // #2 already dispatched at this head_sha → should_skip drops it.
            dispatched: HashSet::from([action_key(2, &dispatched.head_sha, "review")]),
            // #3 dispatched 500s before `now` (1800s cooldown) → cooldown_skip drops it.
            events: vec![DispatchEvent {
                pr: 3,
                skill_key: crate::model::SkillInvocation::skill_key("pr-review", ""),
                head_sha: cooled.head_sha.clone(),
                key: action_key(3, &cooled.head_sha, "review"),
                dispatched_at_epoch: 1_000,
            }],
        };

        let meta = |n: u64| {
            (
                format!("PR {n}"),
                vec!["review-label".to_string()],
                format!("https://x/{n}"),
            )
        };

        // Clean candidate → no skip_reason → dispatchable Some.
        let (v1, d1) = {
            let (t, l, u) = meta(1);
            webhook_view(clean, t, l, u, &params(), &ledger, 1_500)
        };
        assert_eq!(v1.number, 1);
        assert_eq!(v1.title, "PR 1");
        assert_eq!(v1.skip_reason, None);
        assert!(d1.is_some(), "a clean candidate is dispatchable");

        // Already-dispatched (static gate) → skip_reason Some → dispatchable None.
        let (v2, d2) = {
            let (t, l, u) = meta(2);
            webhook_view(dispatched, t, l, u, &params(), &ledger, 1_500)
        };
        assert!(
            v2.skip_reason
                .as_deref()
                .is_some_and(|r| r.contains("already dispatched")),
            "static gate fires: {:?}",
            v2.skip_reason
        );
        assert!(
            d2.is_none(),
            "a statically-gated candidate is not dispatchable"
        );

        // Within cooldown (cooldown gate) → skip_reason Some → dispatchable None.
        let (v3, d3) = {
            let (t, l, u) = meta(3);
            webhook_view(cooled, t, l, u, &params(), &ledger, 1_500)
        };
        assert!(
            v3.skip_reason
                .as_deref()
                .is_some_and(|r| r.contains("within cooldown")),
            "cooldown gate fires: {:?}",
            v3.skip_reason
        );
        assert!(
            d3.is_none(),
            "a cooldown-gated candidate is not dispatchable"
        );
    }

    // ── `decide_ingest` (F7): the PURE ingest-decision seam extracted from the
    // AppHandle-bound `ingest_webhook` body so EVERY branch has automated coverage rather
    // than only the old "verified via integration / NOT a unit test" note. Each test asserts
    // the FULL decision: view fields + write + terminal status + message.
    use super::super::webhook::StatusOnlyKind;

    /// A clean single-label candidate wrapped as `IngestIntent::Track { candidate: Some }`.
    /// `row` builds a non-draft, non-fork, allowed-author candidate → clean under the
    /// empty-ledger `params()` gates.
    fn wrap_some(number: u64) -> IngestIntent {
        IngestIntent::Track {
            candidate: Some(row(number, "review", false).candidate),
            conflict: false,
        }
    }

    #[test]
    fn decide_ingest_clean_candidate_lists_for_rule_engine() {
        // A clean candidate updates the list. Action enqueueing is owned by the rule engine,
        // so webhook ingest itself only reports list updates.
        let d = decide_ingest(
            wrap_some(1),
            1,
            "PR 1".to_string(),
            vec!["review-label".to_string()],
            "https://x/1".to_string(),
            &params(),
            &Ledger::default(),
            0,
        );
        assert_eq!(d.view.number, 1);
        assert_eq!(
            d.view.skill_key,
            crate::model::SkillInvocation::skill_key("pr-review", "")
        );
        assert_eq!(d.view.skip_reason, None);
        assert!(matches!(d.write, WriteKind::Upsert));
        assert!(matches!(d.status, DeliveryStatus::ListUpdated));
        assert_eq!(d.message, None);
    }

    #[test]
    fn decide_ingest_static_gated_candidate_is_gated_no_dispatch() {
        // A draft candidate is gated by `should_skip` → Upsert a skipped row, no candidate for
        // rule processing, status Gated, message = the skip reason.
        let mut cand = row(3, "review", false).candidate;
        cand.is_draft = true;
        let intent = IngestIntent::Track {
            candidate: Some(cand),
            conflict: false,
        };
        let d = decide_ingest(
            intent,
            3,
            "PR 3".to_string(),
            vec!["review-label".to_string()],
            "https://x/3".to_string(),
            &params(),
            &Ledger::default(),
            0,
        );
        assert_eq!(d.view.skip_reason, Some("draft PR".to_string()));
        assert!(matches!(d.write, WriteKind::Upsert));
        assert!(matches!(d.status, DeliveryStatus::Gated));
        assert_eq!(d.message, Some("draft PR".to_string()));
    }

    #[test]
    fn decide_ingest_cooldown_gated_candidate_is_gated_no_dispatch() {
        // A candidate within its dispatch cooldown is gated by `cooldown_skip` → Gated row,
        // no candidate for rule processing, message = the cooldown reason.
        use crate::pr::ledger::DispatchEvent;
        use std::collections::HashSet;

        let cand = row(4, "review", false).candidate;
        let ledger = Ledger {
            dispatched: HashSet::new(),
            events: vec![DispatchEvent {
                pr: 4,
                skill_key: crate::model::SkillInvocation::skill_key("pr-review", ""),
                head_sha: cand.head_sha.clone(),
                key: action_key(4, &cand.head_sha, "review"),
                dispatched_at_epoch: 1_000,
            }],
        };
        let intent = IngestIntent::Track {
            candidate: Some(cand),
            conflict: false,
        };
        // dispatched 500s before `now` (1800s cooldown) → within window.
        let d = decide_ingest(
            intent,
            4,
            "PR 4".to_string(),
            vec!["review-label".to_string()],
            "https://x/4".to_string(),
            &params(),
            &ledger,
            1_500,
        );
        assert!(
            d.view
                .skip_reason
                .as_deref()
                .is_some_and(|r| r.contains("within cooldown")),
            "cooldown gate fires: {:?}",
            d.view.skip_reason
        );
        assert!(matches!(d.write, WriteKind::Upsert));
        assert!(matches!(d.status, DeliveryStatus::Gated));
        assert!(d
            .message
            .as_deref()
            .is_some_and(|m| m.contains("within cooldown")));
    }

    #[test]
    fn decide_ingest_conflict_is_gated_with_both_labels_reason() {
        // Both trigger labels (`candidate: None, conflict: true`) → Upsert a skipped "review"
        // row carrying the BOTH-labels reason, NO dispatch, status Gated.
        let intent = IngestIntent::Track {
            candidate: None,
            conflict: true,
        };
        let d = decide_ingest(
            intent,
            5,
            "PR 5".to_string(),
            vec!["review-label".to_string(), "check-label".to_string()],
            "https://x/5".to_string(),
            &params(),
            &Ledger::default(),
            0,
        );
        assert_eq!(d.view.number, 5);
        assert_eq!(
            d.view.skill_key,
            crate::model::SkillInvocation::skill_key("pr-review", "")
        );
        assert_eq!(
            d.view.skip_reason,
            Some(discover::BOTH_TRIGGER_LABELS_REASON.to_string())
        );
        assert!(matches!(d.write, WriteKind::Upsert));
        assert!(matches!(d.status, DeliveryStatus::Gated));
        assert_eq!(
            d.message,
            Some(discover::BOTH_TRIGGER_LABELS_REASON.to_string())
        );
    }

    #[test]
    fn decide_ingest_status_only_closed_is_not_open_update_present() {
        // A closed/merged PR → StatusOnly { ClosedOrMerged }: UpdatePresent (refresh an
        // existing row, never insert / dispatch), status NotOpen, reason "PR 已关闭或合并".
        let intent = IngestIntent::StatusOnly {
            skill_key: StatusOnlyKind::ClosedOrMerged,
        };
        let d = decide_ingest(
            intent,
            6,
            "PR 6".to_string(),
            vec!["review-label".to_string()],
            "https://x/6".to_string(),
            &params(),
            &Ledger::default(),
            0,
        );
        assert_eq!(d.view.number, 6);
        assert_eq!(
            d.view.skill_key,
            crate::model::SkillInvocation::skill_key("pr-review", "")
        );
        assert_eq!(d.view.skip_reason, Some("PR 已关闭或合并".to_string()));
        assert!(matches!(d.write, WriteKind::UpdatePresent));
        assert!(matches!(d.status, DeliveryStatus::NotOpen));
        assert_eq!(d.message, Some("PR 已关闭或合并".to_string()));
    }

    #[test]
    fn decide_ingest_status_only_trigger_label_removed_is_no_trigger_label_update_present() {
        // An open PR with the trigger label removed → StatusOnly { TriggerLabelRemoved }:
        // UpdatePresent, status NoTriggerLabel, reason "触发 label 已移除". The action skill_key is
        // no longer inferred from labels at ingest time.
        let intent = IngestIntent::StatusOnly {
            skill_key: StatusOnlyKind::TriggerLabelRemoved,
        };
        let d = decide_ingest(
            intent,
            7,
            "PR 7".to_string(),
            vec!["check-label".to_string()],
            "https://x/7".to_string(),
            &params(),
            &Ledger::default(),
            0,
        );
        assert_eq!(d.view.number, 7);
        assert_eq!(
            d.view.skill_key,
            crate::model::SkillInvocation::skill_key("pr-review", "")
        );
        assert_eq!(d.view.skip_reason, Some("触发 label 已移除".to_string()));
        assert!(matches!(d.write, WriteKind::UpdatePresent));
        assert!(matches!(d.status, DeliveryStatus::NoTriggerLabel));
        assert_eq!(d.message, Some("触发 label 已移除".to_string()));
    }
}
