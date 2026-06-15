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
pub mod dispatch;
pub mod error;
pub mod events;
pub mod model;
pub mod pr;
pub mod review;
pub mod state;

use std::sync::Arc;

use model::Candidate;
use state::AppState;
use tauri::{Emitter, Manager};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .manage(AppState::default())
        .setup(|app| {
            let state = app.state::<AppState>();
            // Install the auto-trigger dispatcher BEFORE starting the loop, so the
            // immediate first tick already auto-starts dispatchable reviews. The
            // closure is the `pr` slice's review-agnostic `Dispatcher` seam; its body
            // is [`run_auto_dispatch`], the composition-root assembly that picks the
            // concrete engine and injects it into the engine-agnostic
            // [`dispatch::auto_dispatch`] (so adding an engine never edits `dispatch`).
            state.scheduler.set_dispatcher(Arc::new({
                let app = app.handle().clone();
                move |cands| {
                    let app = app.clone();
                    Box::pin(run_auto_dispatch(app, cands))
                }
            }));
            // Auto-start the poll loop on launch ("启动即跑"): the first interval
            // tick fires immediately, so this yields an initial PR list too.
            state.scheduler.start(app.handle().clone());
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
            review::commands::get_codex_status,
            review::commands::start_review,
            review::commands::stop_review,
            review::commands::list_review_sessions,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // Kill the resident codex app-server when the app exits, so the child
            // process never outlives the app ("软件关闭时一起关闭"). The manager's
            // shutdown is idempotent and `kill_on_drop(true)` is the backstop.
            if matches!(event, tauri::RunEvent::Exit) {
                app_handle.state::<AppState>().codex.shutdown();
            }
        });
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
    candidates: Vec<Candidate>,
) {
    if candidates.is_empty() {
        return;
    }

    // A bad / hand-edited config must not take the poll loop down: skip the batch,
    // logged + surfaced to the UI (a desktop user never sees stderr).
    let cfg = match config::service::load_validated(&app) {
        Ok(cfg) => cfg,
        Err(e) => {
            let msg = format!("配置无效，自动 review 跳过本轮（{}）", e.message);
            eprintln!("auto-dispatch 跳过本轮：{msg}");
            emit_dispatch_error(&app, msg);
            return;
        }
    };
    let skill_abs = skill_abs_path(&cfg.repo_root, &cfg.skill_rel_path);
    let state = app.state::<AppState>();
    let engine = review::engines::codex::CodexEngine {
        app: &app,
        codex: &state.codex,
        registry: &state.sessions,
        codex_bin: review::commands::CODEX_BIN,
        repo: &cfg.repo,
        repo_root: &cfg.repo_root,
        skill_abs_path: &skill_abs,
    };
    // The review slice owns "what counts as active"; the pr slice owns the ledger.
    let active = state.sessions.active_pairs();
    let record = |cands: &[Candidate]| pr::ledger::record_dispatched(&app, cands);
    let report = |msg: String| emit_dispatch_error(&app, msg);
    dispatch::auto_dispatch(candidates, &engine, &active, &record, &report).await;
}

/// Emit a session-less [`events::ReviewEvent::DispatchError`] to the review area
/// (the availability banner). Best-effort — a gone window is not an error worth
/// propagating from the poll loop.
fn emit_dispatch_error<R: tauri::Runtime>(app: &tauri::AppHandle<R>, message: String) {
    let _ = app.emit(
        events::REVIEW_EVENT,
        &events::ReviewEvent::DispatchError { message },
    );
}

/// Absolute path to the pr-review skill file codex attaches to a turn. `repo_root`
/// is an absolute dir and `skill_rel_path` a relative path under it (both
/// config-validated), so the join is absolute and infallible.
fn skill_abs_path(repo_root: &str, skill_rel_path: &str) -> String {
    std::path::Path::new(repo_root)
        .join(skill_rel_path)
        .to_string_lossy()
        .into_owned()
}
