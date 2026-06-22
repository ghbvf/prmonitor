//! prmonitor — composition root.
//!
//! Vertical-slice layout: each slice (`config`, `pr`, `review`) is self-contained
//! and sits directly under `src/` (no `features/` wrapper). Horizontal concerns
//! are flat files (`error`, `events`, `model`, `state`, `dispatch`). Slices never
//! import each other's internals — cross-slice types live in [`model`], the only
//! contract.
//!
//! Slice *assembly* is the composition layer's job: this file (the root — it
//! `manage`s [`state::AppState`], registers Tauri commands, and installs the
//! scheduler's dispatch hook) plus the root-level horizontal modules it enables —
//! notably [`dispatch`], which glues the `pr` slice's gating output to the
//! `review` engine. Those composition modules may consume several slices' public
//! APIs (that is what makes them composition, not slices); the invariant that
//! stays structural is that *slices* never cross slice lines — only the
//! composition layer does.

// Modules are `pub` so forward-looking seams and shared types (e.g.
// `pr::source::PrSource`, `review::engine::ReviewEngine`) count as reachable API
// in this skeleton rather than tripping `dead_code` before their first use.
pub mod config;
pub mod db;
pub mod dispatch;
pub mod error;
pub mod events;
pub mod model;
pub mod pr;
pub mod review;
pub mod state;

use std::sync::Arc;

use model::{Candidate, EngineKind};
use state::AppState;
use tauri::{Emitter, Manager};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .manage(AppState::default())
        .setup(|app| {
            // Open + migrate the unified SQLite store and manage it as a `tauri::State`
            // BEFORE anything that reads persistence (config load / poll start). It is a
            // `State` rather than an `AppState` field because `app_data_dir()` only
            // resolves here in `setup`, while `AppState` is `.manage()`d at builder time.
            // Then run the one-time legacy JSON → SQLite import (#70) so existing users'
            // config / tracked PRs / ledger carry over before the first read.
            app.manage(db::Database::open(app.handle())?);
            import_legacy_stores(app.handle())?;
            // Reconcile sessions left non-terminal by a dead previous process (pr-review F1):
            // a persisted starting/running/interrupting status has no live pump, so mark it
            // failed rather than letting the UI restore it as still running.
            let stale =
                review::history_store::fail_orphaned_sessions(app.state::<db::Database>().inner())?;
            if stale > 0 {
                eprintln!("启动：{stale} 个遗留未完成 review 会话已标记 failed");
            }

            let state = app.state::<AppState>();
            // Install the auto-trigger dispatcher BEFORE starting the loop, so the
            // immediate first tick already auto-starts dispatchable reviews. The
            // closure is the `pr` slice's review-agnostic `Dispatcher` seam; its body
            // is [`run_auto_dispatch`], the composition-root assembly that picks the
            // concrete engine and injects it into the engine-agnostic
            // [`dispatch::auto_dispatch`] (so adding an engine never edits `dispatch`).
            state
                .scheduler
                .set_dispatcher(make_dispatcher(app.handle().clone()));
            // Install the WEBHOOK trigger's ingest hook (#9 / #61). The webhook is a
            // second auto-trigger source: its axum handler parses + routes a push payload
            // into a `WebhookEvent` and hands it here. The ingest (`pr::commands::ingest_webhook`)
            // upserts the persisted PR list + emits `prs:updated` (so webhook PRs enter the
            // list even when autoReview is OFF — the #61 fix), applies that project's
            // static/cooldown gates (`webhook_view`) + the SAME per-project `autoReview`
            // gate the scheduler uses, and dispatches the clean candidate by reusing the
            // very same `run_auto_dispatch` via the shared `make_dispatcher` helper (the
            // SAME `ProjectDispatcher` the scheduler gets). Keeping all this in the ingest (not the handler) is
            // what lets `pr::webhook` stay runtime-agnostic (never names AppHandle); the
            // ingest also records the terminal delivery diagnostic (#62).
            let webhook_dispatcher = make_dispatcher(app.handle().clone());
            state.webhook.set_ingestor(Arc::new({
                let app = app.handle().clone();
                move |ev| {
                    let app = app.clone();
                    let dispatcher = webhook_dispatcher.clone();
                    Box::pin(async move {
                        pr::commands::ingest_webhook(&app, &dispatcher, ev).await;
                    })
                }
            }));
            // Install the AZURE refresh hook (AB#822). Azure DevOps PR Service Hooks carry no
            // labels and don't fire on label changes, so an Azure webhook can't classify a
            // candidate from its payload — it's a refresh SIGNAL. The handler routes the event
            // to a `project_id` and hands it here; this re-runs the SAME `az` discovery the
            // poll path uses (`SchedulerSet::discover_once`, in-flight-coalesced), reading the
            // authoritative current labels and dispatching via the shared dispatcher. Keeps
            // `pr::webhook` runtime-agnostic (the closure holds the concrete AppHandle).
            state.webhook.set_refresher(Arc::new({
                let app = app.handle().clone();
                move |project_id: String| {
                    let app = app.clone();
                    Box::pin(async move {
                        use tauri::Manager;
                        app.state::<AppState>()
                            .scheduler
                            .discover_once(&app, &project_id)
                            .await;
                    })
                }
            }));
            // Auto-start the poll loop only when the persisted config is valid, via the
            // shared start_if_config_valid gate — the SINGLE funnel point (PR #41 F1) the
            // public start_polling command also goes through. On first launch (empty
            // repoRoot default) or an invalid hand-edit, the gate errors → we discard it
            // (no loop, so no gh poll fires and no per-cycle DispatchError spams the
            // banner). The frontend onboarding/Settings save calls start_polling once a
            // valid config lands, running the same gate. The dispatcher hook above stays
            // installed, so the first tick after a later start already dispatches.
            let _ = pr::commands::start_if_config_valid(app.handle(), state.inner());
            // Start the RESIDENT local REST API (AB#1043): a 127.0.0.1-only axum listener that
            // lets a third party (curl/CLI) trigger a review + poll for completion + comment URL.
            // NEVER tunneled — this is the inbound trigger control plane, strictly separate from
            // the public `webhook` receiver. The bound PORT is read once here (a change needs an
            // app restart, like the webhook port); the TOKEN is read live per request, so
            // setting/clearing it in Settings takes effect without a restart. A bind failure is
            // logged + swallowed inside the spawned task (a port clash must not crash the app).
            state.local_api.start(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            config::commands::app_version,
            config::commands::get_config,
            config::commands::set_config,
            pr::commands::start_polling,
            pr::commands::stop_polling,
            pr::commands::poll_now,
            pr::commands::reschedule,
            pr::commands::gh_status,
            pr::commands::get_prs,
            pr::commands::set_pr_archived,
            pr::commands::start_webhook,
            pr::commands::stop_webhook,
            pr::commands::webhook_status,
            pr::commands::webhook_deliveries,
            pr::commands::poll_status,
            review::commands::get_codex_status,
            review::commands::start_codex,
            review::commands::stop_codex,
            review::commands::start_review,
            review::commands::trigger_review,
            review::commands::stop_review,
            review::commands::list_review_sessions,
            review::commands::get_session_history,
            review::commands::get_pr_sessions,
            config::commands::set_active_project,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // Kill the resident codex app-server when the app exits, so the child
            // process never outlives the app ("软件关闭时一起关闭"). The manager's
            // shutdown is idempotent and `kill_on_drop(true)` is the backstop.
            if matches!(event, tauri::RunEvent::Exit) {
                let state = app_handle.state::<AppState>();
                state.codex.shutdown();
                // Abort every in-flight `claude -p` review's pump task (each drops its
                // `kill_on_drop` child → SIGKILL), so no review subprocess outlives the
                // app — same "软件关闭时一起关闭" contract as codex (#718).
                state.claude.shutdown();
                // Kill the cloudflared tunnel + abort the receiver so neither outlives
                // the app (same "软件关闭时一起关闭" contract as codex).
                state.webhook.shutdown();
                // Stop the resident local REST API listener (AB#1043): same shutdown contract.
                state.local_api.shutdown();
            }
        });
}

/// One-time legacy JSON → SQLite import (#70). Reads the pre-SQLite `tauri-plugin-store`
/// files (`config.json` / `prs.json` / `ledger.json`) and hands each value to the OWNING
/// slice's `import_legacy_*` helper (column knowledge stays in the slice), inserting all
/// rows + the done-guard in ONE transaction so a crash mid-import rolls back and re-runs
/// cleanly. A no-op once [`db::Database::legacy_imported`] is set, and on fresh installs
/// (the legacy stores are empty, so nothing imports). The old JSON files are LEFT in
/// place (recoverable / downgradeable); the guard makes them inert.
///
/// This is composition (it spans `config` + `pr` slices), so it lives at the root, not
/// in `db` (which stays a pure horizontal owning only schema + connection).
fn import_legacy_stores<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> error::AppResult<()> {
    use tauri_plugin_store::StoreExt;

    let db = app.state::<db::Database>();
    if db.legacy_imported()? {
        return Ok(());
    }

    // Gather the legacy values up front (reads, outside the write transaction). A
    // missing store file just yields an empty store → nothing to import.
    let config_value = app
        .store("config.json")
        .ok()
        .and_then(|s| s.get("appConfig"));

    let mut tracked: Vec<(String, serde_json::Value)> = Vec::new();
    if let Ok(store) = app.store("prs.json") {
        for key in store.keys() {
            if let Some(pid) = key.strip_prefix("tracked:") {
                if let Some(v) = store.get(&key) {
                    tracked.push((pid.to_string(), v));
                }
            }
        }
    }

    let mut dispatched: Vec<(String, serde_json::Value)> = Vec::new();
    let mut events: Vec<(String, serde_json::Value)> = Vec::new();
    if let Ok(store) = app.store("ledger.json") {
        for key in store.keys() {
            if let Some(pid) = key.strip_prefix("dispatched:") {
                if let Some(v) = store.get(&key) {
                    dispatched.push((pid.to_string(), v));
                }
            } else if let Some(pid) = key.strip_prefix("events:") {
                if let Some(v) = store.get(&key) {
                    events.push((pid.to_string(), v));
                }
            }
        }
    }

    import_legacy_into_db(&db, config_value.as_ref(), &tracked, &dispatched, &events)
}

/// The cross-slice import ASSEMBLY (review F11) — split out from [`import_legacy_stores`]
/// (which is the tauri-store GATHER) so the part with NO per-slice owner is testable
/// without a tauri app: guard-check, then in ONE transaction hand each gathered legacy
/// value to its owning slice's `import_legacy_*` and stamp the done-guard, so a crash
/// mid-import rolls back and re-runs cleanly. The per-slice `import_legacy_*` have their
/// own round-trip tests; this layer's contract (all four categories in one tx + guard) is
/// covered by `import_legacy_into_db_imports_all_slices_then_guards`.
fn import_legacy_into_db(
    db: &db::Database,
    config_value: Option<&serde_json::Value>,
    tracked: &[(String, serde_json::Value)],
    dispatched: &[(String, serde_json::Value)],
    events: &[(String, serde_json::Value)],
) -> error::AppResult<()> {
    // Authoritative guard (also pre-checked in `import_legacy_stores` to skip the gather):
    // keeping it here makes this unit self-guarding, so a re-run is a proven no-op.
    if db.legacy_imported()? {
        return Ok(());
    }
    db.with_tx(|tx| {
        if let Some(v) = config_value {
            config::service::import_legacy_config(tx, v)?;
        }
        for (pid, v) in tracked {
            pr::registry::import_legacy_tracked(tx, pid, v)?;
        }
        for (pid, v) in dispatched {
            pr::ledger::import_legacy_dispatched(tx, pid, v)?;
        }
        for (pid, v) in events {
            pr::ledger::import_legacy_events(tx, pid, v)?;
        }
        db::mark_legacy_imported(tx)?;
        Ok(())
    })
}

/// Build the per-cycle [`pr::scheduler::ProjectDispatcher`] both auto-trigger sources
/// share — the poll scheduler and the webhook ingestor. Both drive a dispatchable
/// `(project_id, candidates)` through the SAME [`run_auto_dispatch`] (the composition
/// root's gate + concrete-engine assembly), so this single helper removes the duplicated
/// `Arc::new(move |..| Box::pin(run_auto_dispatch(..)))` closure that was built verbatim
/// at both wiring sites.
fn make_dispatcher<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> pr::scheduler::ProjectDispatcher {
    Arc::new(move |project_id, cands| {
        let app = app.clone();
        Box::pin(run_auto_dispatch(app, project_id, cands))
    })
}

/// Composition-root assembly for one auto-trigger cycle: this is the ONE place that
/// names the concrete review engine. It loads + validates config, builds the codex
/// [`review::engines::codex::CodexEngine`] (the [`review::engine::ReviewEngine`] the
/// root picks — a future Claude engine plugs in here), snapshots the registry's
/// active `(pr, kind)` pairs, and injects a ledger recorder + UI error reporter into
/// the engine-agnostic [`dispatch::auto_dispatch`]. Keeping the concrete names here
/// (not in `dispatch`) is what makes the slice boundary structural rather than
/// comment-only (PR #31 finding F1).
async fn run_auto_dispatch<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: String,
    candidates: Vec<Candidate>,
) {
    if candidates.is_empty() {
        return;
    }

    // Resolve + validate THIS project (#35): a bad / hand-edited project config (e.g. an
    // escaped skill path) must not take the poll loop down — skip the batch, logged +
    // surfaced to the UI scoped to the project (a desktop user never sees stderr).
    // `project_validated` re-runs the skill-path validation before codex attaches it.
    let project = match config::service::project_validated(&app, &project_id) {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("配置无效，自动 review 跳过本轮（{}）", e.message);
            eprintln!("auto-dispatch 跳过本轮（{project_id}）：{msg}");
            emit_dispatch_error(&app, &project_id, msg);
            return;
        }
    };
    let skill_abs = review::commands::skill_abs_path(&project.repo_root, &project.skill_rel_path);
    let state = app.state::<AppState>();
    // The review slice owns "what counts as active"; the pr slice owns the ledger.
    // Both are scoped to this project (#35) so a PR number active in one project does
    // not gate the same number in another, and dedup writes land in the right partition.
    let active = state.sessions.active_pairs(&project_id);
    let record = |cands: &[Candidate]| pr::ledger::record_dispatched(&app, &project_id, cands);
    let report = |msg: String| emit_dispatch_error(&app, &project_id, msg);
    // Snapshot the comment-URL source context from the project NOW (AB#1042), so each review
    // this batch starts resolves its pr-review comment URL at the terminal against the project
    // it ran against — never a config edited mid-review. Each engine `start` clones it per
    // candidate (the engine `start` takes `&self`), so one snapshot covers the whole batch.
    let url_ctx = review::session::CommentUrlContext {
        source_kind: project.source_kind,
        repo: project.repo.clone(),
        azure_org: project.azure_org.clone(),
        azure_project: project.azure_project.clone(),
    };
    // The ONE place that names a concrete engine for the auto-trigger path. The
    // exhaustive `match` over the sealed `EngineKind` (model.rs) is the Hard carrier:
    // adding a variant without an arm here is a compile error. Each arm monomorphizes
    // `dispatch::auto_dispatch` with its concrete engine (the trait uses bare `async fn`,
    // not dyn-safe, so we pick a concrete type per arm rather than box).
    match project.engine_kind {
        EngineKind::Codex => {
            // Respect an explicit user `stop_codex`: a stopped codex is NOT auto-revived
            // by a dispatchable PR. Skip this batch silently (same as the autoReview-off
            // skip — no emit, no spawn). Only MANUAL review (`start_review`) and manual
            // `start_codex` force a restart; auto-dispatch defers to the user's stop (PR
            // #47 F1). Codex-specific: claude has no resident server / stop flag, so this
            // gate lives in the codex arm — a `stop_codex` must not swallow claude reviews.
            if state.codex.is_stopped() {
                return;
            }
            let engine = review::engines::codex::CodexEngine {
                app: &app,
                codex: &state.codex,
                registry: &state.sessions,
                codex_bin: review::commands::CODEX_BIN,
                project_id: &project_id,
                repo: &project.repo,
                repo_root: &project.repo_root,
                skill_abs_path: &skill_abs,
                codex_model: &project.codex_model,
                url_ctx,
            };
            dispatch::auto_dispatch(candidates, &engine, &active, &record, &report).await;
        }
        EngineKind::Claude => {
            let engine = review::engines::claude::ClaudeEngine {
                app: &app,
                claude: &state.claude,
                registry: &state.sessions,
                claude_bin: review::engines::claude::process::CLAUDE_BIN,
                project_id: &project_id,
                repo: &project.repo,
                repo_root: &project.repo_root,
                claude_model: &project.claude_model,
                url_ctx,
            };
            dispatch::auto_dispatch(candidates, &engine, &active, &record, &report).await;
        }
    }
}

/// Emit a session-less [`events::ReviewEvent::DispatchError`] to the review area
/// (the availability banner), routed to `project_id` (#35) so the frontend shows it
/// on the right project. Best-effort — a gone window is not an error worth
/// propagating from the poll loop.
fn emit_dispatch_error<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    project_id: &str,
    message: String,
) {
    let _ = app.emit(
        events::REVIEW_EVENT,
        &events::ReviewEvent::DispatchError {
            project_id: project_id.to_string(),
            message,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    // Cross-slice legacy-import ASSEMBLY + guard (review F11). The per-slice
    // `import_legacy_*` have their own round-trip tests; this covers what ONLY the
    // composition root owns: all four categories imported in ONE transaction, the
    // done-guard stamped, and a second call short-circuiting with NO duplicate rows —
    // proven via `dispatch_event` (a plain INSERT, so a dropped guard would double it).
    #[test]
    fn import_legacy_into_db_imports_all_slices_then_guards() {
        let db = Database::open_in_memory().expect("open db");

        let config_value = serde_json::json!({ "activeProjectId": "imported", "projects": [] });
        let tracked = vec![(
            "default".to_string(),
            serde_json::json!([{
                "number": 7, "title": "PR 7", "labels": ["needs-review"],
                "url": "https://x/7", "kind": "review", "skipReason": null,
                "firstSeenEpoch": 1, "lastSeenEpoch": 2, "archived": false
            }]),
        )];
        let dispatched = vec![("default".to_string(), serde_json::json!(["7@sha:review"]))];
        let events = vec![(
            "default".to_string(),
            serde_json::json!([{
                "pr": 7, "kind": "review", "headSha": "sha",
                "key": "7@sha:review", "dispatchedAtEpoch": 100
            }]),
        )];

        import_legacy_into_db(&db, Some(&config_value), &tracked, &dispatched, &events)
            .expect("first import");

        // Guard stamped.
        assert!(db.legacy_imported().expect("guard read"));

        // Config blob imported (one row at the fixed id).
        let config_rows = db
            .with_conn(|c| {
                c.query_row("SELECT COUNT(*) FROM config_blob WHERE id = 1", [], |r| {
                    r.get::<_, i64>(0)
                })
            })
            .expect("count config");
        assert_eq!(config_rows, 1, "config blob imported");

        // pr-slice tracked + ledger round-trip through their db-loads.
        let prs = pr::registry::TrackedPrs::load_db(&db, "default").expect("tracked load");
        assert_eq!(prs.prs.len(), 1);
        assert_eq!(prs.prs[0].number, 7);

        let ledger = pr::ledger::Ledger::load_db(&db, "default").expect("ledger load");
        assert!(ledger.has_dispatched("7@sha:review"));
        assert_eq!(ledger.events.len(), 1);

        // Second call short-circuits on the guard → NO duplicate dispatch_event rows.
        import_legacy_into_db(&db, Some(&config_value), &tracked, &dispatched, &events)
            .expect("second import is a no-op");
        let ledger_again = pr::ledger::Ledger::load_db(&db, "default").expect("ledger reload");
        assert_eq!(
            ledger_again.events.len(),
            1,
            "guard prevents re-import duplicating events"
        );
    }
}
