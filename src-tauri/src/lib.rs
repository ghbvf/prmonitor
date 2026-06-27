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
//! notably [`dispatch`], which turns the `pr` slice's gating output into durable
//! outbox actions. Those composition modules may consume several slices' public
//! APIs (that is what makes them composition, not slices); the invariant that
//! stays structural is that *slices* never cross slice lines — only the
//! composition layer does.

// Modules are `pub` so forward-looking seams and shared types (e.g.
// `pr::source::EventSourceProvider`, `review::engine::ReviewEngine`) count as reachable API
// in this skeleton rather than tripping `dead_code` before their first use.
pub mod cli;
pub mod config;
pub mod db;
pub mod dispatch;
pub mod error;
pub mod events;
pub mod inbox;
pub mod model;
pub mod notification;
pub mod outbox;
pub mod pr;
pub mod remote;
pub mod review;
pub mod rule;
pub mod state;
pub mod stream;
pub mod terminal;

/// Rust slice-boundary enforcement test (AB#1066 F1, Medium carrier) — test-only module.
#[cfg(test)]
mod slice_boundary_test;
#[cfg(test)]
mod typegen;

use std::sync::Arc;

use model::{Candidate, Event};
use pr::source::DiscoveredEvent;
use state::AppState;
use tauri::{Manager, Runtime};

enum NotificationActionPayload {
    Delivery(model::NotificationDeliveryPayload),
    Legacy(model::Notification),
}

fn parse_notification_action_payload(payload: &str) -> error::AppResult<NotificationActionPayload> {
    match serde_json::from_str::<model::NotificationDeliveryPayload>(payload) {
        Ok(payload) => Ok(NotificationActionPayload::Delivery(payload)),
        Err(new_err) => match serde_json::from_str::<model::Notification>(payload) {
            Ok(note) => Ok(NotificationActionPayload::Legacy(note)),
            Err(old_err) => Err(error::AppError::new(format!(
                "outbox 通知 payload 既不是 NotificationDeliveryPayload，也不是 legacy Notification：new={new_err}; legacy={old_err}"
            ))),
        },
    }
}

#[tauri::command]
async fn notification_test_send<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    channel: config::service::NotificationChannel,
) -> error::AppResult<String> {
    config::service::validate_notification_channel_for_test(&channel)?;
    let delivery_channel = config::service::notification_delivery_channel(&channel);
    let note = model::Notification::new(
        model::NotificationLevel::Info,
        "prmonitor 通知测试".to_string(),
        String::new(),
        model::RedactedNotificationBody::fixed("这是一条 prmonitor 测试通知"),
        String::new(),
    );
    match review::notify::deliver_channel_with_app(&app, &delivery_channel, &note).await? {
        model::ActionExecutionResult::Done => {
            Ok(format!("通知渠道「{}」测试发送成功", channel.name))
        }
        model::ActionExecutionResult::Retry { message, .. }
        | model::ActionExecutionResult::Dead { message } => Err(error::AppError::new(message)),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // AB#1044: branch on `argv` BEFORE building Tauri. `prmonitor review …` runs entirely as a thin
    // HTTP client over the AB#1043 local API (the `code --wait` role) — including launching the app
    // on a cold start and then connecting to it (see `cli::run_client_blocking`). It NEVER becomes
    // the GUI. Anything else (no/unknown subcommand) is a normal GUI launch.
    match cli::parse() {
        cli::Invocation::Review(args) => std::process::exit(cli::run_client_blocking(&args)),
        cli::Invocation::Notify(args) => std::process::exit(cli::run_notify_client_blocking(&args)),
        cli::Invocation::Gui => build_app(),
    }
}

/// Build + run the Tauri GUI.
fn build_app() {
    tauri::Builder::default()
        // Single-instance MUST be the FIRST plugin (AB#1044): it claims the OS lock before any
        // window work, so a second launch focuses the existing window instead of opening a
        // duplicate (incl. when two cold-start CLIs each spawn the GUI — only one survives, and
        // both CLIs then connect to its local API). The CLI never forwards a `review` request
        // through this callback, so it only needs to resurface the window.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        // Deeplink (AB#1045): registered AFTER single-instance (which must claim the OS lock /
        // argv first) so a `prmonitor://…` opened while the app runs reaches the resident instance
        // — `single-instance`'s `deep-link` feature forwards the argv on Windows/Linux, and the
        // `on_open_url` hook wired in `.setup` parses + triggers it. `notification` backs the
        // fire-and-forget completion toast (sent from Rust in `review::deeplink`).
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_notification::init())
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
            // The orphan-session reconcile (mark sessions left non-terminal by a dead previous
            // process as `failed`) now runs INSIDE `state.outbox.start(...)` via the `before_worker`
            // closure below (AB#1204 F3) — so its load-bearing ORDERING (reconcile BEFORE the outbox
            // worker can process any row) is enforced by control flow, not a comment. See the closure
            // passed to `outbox.start` and `OutboxManager::start`'s doc.

            let state = app.state::<AppState>();
            state.notification_sender.set_sink(Arc::new({
                let app = app.handle().clone();
                move |request, dedupe_prefix| {
                    notification::enqueue_notification_with_dedupe_prefix(
                        &app,
                        request,
                        dedupe_prefix.as_deref(),
                    )
                }
            }));
            // Install the auto-trigger dispatcher BEFORE starting the loop, so the
            // immediate first tick already persists dispatchable reviews/checks into
            // inbox + outbox. The closure is the `pr` slice's review-agnostic
            // `Dispatcher` seam; its body is [`run_auto_dispatch`], the
            // composition-root assembly that injects durable inbox/outbox writers into
            // the default producer.
            state
                .scheduler
                .set_dispatcher(make_dispatcher(app.handle().clone()));
            // Install the WEBHOOK trigger hooks (#9 / #61 / AB#822) — now FRONTED BY THE EVENT
            // INBOX (AB#1065). The webhook is a second auto-trigger source: its axum handler
            // parses + routes a push payload into a `WebhookEvent` and hands the verbatim body +
            // delivery GUID + event here through the WIDENED ingestor seam. The inbox
            // (`inbox::service::ingest_github`) PERSISTS + DEDUPS the delivery
            // (`UNIQUE(dedupe_key)` — the Hard ingress-idempotency carrier), then re-feeds the
            // SAME `WebhookEvent` through the UNCHANGED `pr::commands::ingest_webhook` (which
            // upserts the list + emits `prs:updated` + applies the per-project gates + the
            // downstream `dispatch_key` gate + dispatches). The inbox is ADDITIVE: it adds durable
            // persistence + replay in front of the vetted path, it does NOT replace the
            // authoritative dispatch dedup. Holding the concrete AppHandle + the shared
            // dispatcher here keeps `pr::webhook` runtime-agnostic.
            let webhook_dispatcher = make_dispatcher(app.handle().clone());
            // The GitHub RE-FEED hook (AB#1065 decoupling): the ONLY place that names
            // `pr::webhook::WebhookEvent` / the `ProjectDispatcher`. Given the AppHandle + the
            // stored parsed-`WebhookEvent` JSON, it deserializes the event and re-feeds it through
            // the UNCHANGED `pr::commands::ingest_webhook` (which upserts the list + emits
            // `prs:updated` + applies the per-project gates + the downstream `dispatch_key` gate +
            // dispatches). Built ONCE and shared by the live GitHub ingestor AND the inbox replay
            // hook, so a replay re-feeds identically. The inbox slice holds this only as the OPAQUE
            // `GithubRefeed` closure — it never names a `pr` type.
            let github_refeed: inbox::GithubRefeed = Arc::new({
                let dispatcher = webhook_dispatcher.clone();
                move |app: tauri::AppHandle, webhook_event_json: String| {
                    let dispatcher = dispatcher.clone();
                    Box::pin(async move {
                        // F2: a deser failure of the persisted replay JSON now PROPAGATES as Err so
                        // the inbox marks the row Failed (not falsely Processed) and surfaces it on
                        // replay — it is no longer just logged.
                        let ev: pr::webhook::WebhookEvent =
                            serde_json::from_str(&webhook_event_json).map_err(|e| {
                                error::AppError::new(format!(
                                    "重放/再投递时 WebhookEvent 反序列化失败：{e}"
                                ))
                            })?;
                        pr::commands::ingest_webhook(&app, &dispatcher, ev).await
                    })
                }
            });
            // The original `az` re-discovery hook the inbox's Azure path wraps (AB#822): re-run
            // `discover_once` (in-flight-coalesced) for the routed project. Built ONCE here and
            // shared by the webhook Azure refresher AND the inbox replay hook (so a replayed Azure
            // entry re-runs the SAME discovery).
            let azure_refresh: inbox::AzureRefresh = Arc::new({
                let app = app.handle().clone();
                move |project_id: String| {
                    let app = app.clone();
                    Box::pin(async move {
                        use tauri::Manager;
                        app.state::<AppState>()
                            .scheduler
                            .discover_once(&app, &project_id)
                            .await;
                        // `discover_once` is best-effort (emits its own PrEvent::Error on failure)
                        // and returns (); the refresh "succeeding" here means it ran, so Ok(()).
                        Ok(())
                    })
                }
            });
            let rule_processor: inbox::RuleProcessor = Arc::new(
                move |app: tauri::AppHandle,
                      inbox_event_id: i64,
                      event: Event,
                      candidate: Option<Candidate>| {
                    Box::pin(async move {
                        process_rule_event(&app, inbox_event_id, event, candidate).map(|_| ())
                    })
                },
            );
            // Install the inbox replay hooks (AB#1065/#1379) so `inbox_replay` re-uses the SAME
            // re-feed + Azure re-discovery + rule processor as live ingress.
            state.inbox.set_hooks(
                github_refeed.clone(),
                azure_refresh.clone(),
                rule_processor.clone(),
            );
            // The GitHub ingestor: normalize the `WebhookEvent` → neutral `model::Event` HERE (pr
            // owns `WebhookEvent`), then hand the inbox the neutral event + the verbatim body + the
            // parsed JSON. The inbox persists+dedups and re-feeds via the injected `github_refeed` —
            // it never sees a `pr` type.
            state.webhook.set_ingestor(Arc::new({
                let app = app.handle().clone();
                let github_refeed = github_refeed.clone();
                let rule_processor = rule_processor.clone();
                move |raw: String, guid: Option<String>, ev: pr::webhook::WebhookEvent| {
                    let app = app.clone();
                    let github_refeed = github_refeed.clone();
                    let rule_processor = rule_processor.clone();
                    Box::pin(async move {
                        use tauri::Manager;
                        // Normalize at the seam (the composition root owns the pr↔inbox boundary):
                        // `WebhookEvent` → neutral `model::Event`, and serialize the parsed event
                        // for replay BEFORE handing it off.
                        let candidate = match &ev.intent {
                            pr::webhook::IngestIntent::Track {
                                candidate: Some(candidate),
                                ..
                            } => Some(candidate.clone()),
                            _ => None,
                        };
                        let event = pr::webhook::event_from_webhook(&ev, guid.as_deref(), &raw);
                        let webhook_event_json = serde_json::to_string(&ev).unwrap_or_default();
                        let db = app.state::<db::Database>();
                        // F1: return the DURABLE-PERSIST Result so the handler gates the ACK on it.
                        inbox::service::ingest_github(
                            &app,
                            db.inner(),
                            inbox::service::GithubIngestHooks {
                                github_refeed: &github_refeed,
                                rule_processor: &rule_processor,
                            },
                            event,
                            raw,
                            webhook_event_json,
                            candidate,
                        )
                        .await
                    })
                }
            }));
            // The Azure refresher: persist an audit entry, then invoke the original re-discovery.
            state.webhook.set_refresher(Arc::new({
                let app = app.handle().clone();
                let azure_refresh = azure_refresh.clone();
                let rule_processor = rule_processor.clone();
                move |raw, project_id, repo| {
                    let app = app.clone();
                    let azure_refresh = azure_refresh.clone();
                    let rule_processor = rule_processor.clone();
                    Box::pin(async move {
                        use tauri::Manager;
                        let db = app.state::<db::Database>();
                        // F1: return the DURABLE-PERSIST Result so the handler gates the ACK on it.
                        inbox::service::ingest_azure_refresh(
                            &app,
                            db.inner(),
                            &azure_refresh,
                            &rule_processor,
                            raw,
                            project_id,
                            repo,
                        )
                        .await
                    })
                }
            }));
            // Install the ACTION OUTBOX executor (AB#1066/AB#1069) — the ONLY place that names
            // `review::notify` / `review::commands`. The outbox slice holds this only as the OPAQUE
            // `ActionExecutor`; the exhaustive `match ActionKind` HERE is the Hard carrier routing
            // each kind to its provider — `Notification` → the desktop notifier; `Review`/`Check` →
            // the review funnel (`run_review_action`); `StopReview` → the idempotent stop
            // (`run_stop_action`). A new `ActionKind` without an arm is a compile error — the missing
            // action cannot be expressed. Each arm deserializes the stored payload; a deser/execute
            // Err propagates so the worker retries / dead-letters rather than marking the row falsely
            // `done`. The review arms map `Deduped → Ok` (a review already in flight is success, not a
            // retry — see `review::commands::outbox_start_outcome`), the OPPOSITE of the manual
            // command path.
            let outbox_executor: outbox::ActionExecutor =
                Arc::new(|app: tauri::AppHandle, action: outbox::OutboxAction| {
                    Box::pin(async move {
                        match action.kind {
                            model::ActionKind::Notification => {
                                let payload =
                                    match parse_notification_action_payload(&action.payload) {
                                        Ok(payload) => payload,
                                        Err(e) => {
                                            return Ok(model::ActionExecutionResult::Dead {
                                                message: e.message,
                                            });
                                        }
                                    };
                                let payload = match payload {
                                    NotificationActionPayload::Delivery(payload) => payload,
                                    NotificationActionPayload::Legacy(note) => {
                                        return review::notify::deliver(
                                            &app,
                                            model::NotificationKind::Desktop,
                                            &note,
                                        )
                                        .await;
                                    }
                                };
                                let channel = match config::service::notification_channel(
                                    &app,
                                    &payload.channel_id,
                                ) {
                                    Ok(channel) => channel,
                                    Err(e) => {
                                        return Ok(model::ActionExecutionResult::Dead {
                                            message: e.message,
                                        });
                                    }
                                };
                                if !channel.enabled {
                                    return Ok(model::ActionExecutionResult::Dead {
                                        message: format!(
                                            "通知渠道「{}」已禁用，停止投递",
                                            channel.name
                                        ),
                                    });
                                }
                                if channel.kind != payload.kind {
                                    return Ok(model::ActionExecutionResult::Dead {
                                        message: format!(
                                            "通知渠道「{}」kind 已从 {:?} 改为 {:?}，停止投递",
                                            channel.name, payload.kind, channel.kind
                                        ),
                                    });
                                }
                                let delivery_channel =
                                    config::service::notification_delivery_channel(&channel);
                                review::notify::deliver_channel_with_app(
                                    &app,
                                    &delivery_channel,
                                    &payload.notification,
                                )
                                .await
                            }
                            model::ActionKind::Review => run_review_action(&app, &action, "review")
                                .await
                                .map(|_| model::ActionExecutionResult::Done),
                            model::ActionKind::Check => run_review_action(&app, &action, "check")
                                .await
                                .map(|_| model::ActionExecutionResult::Done),
                            model::ActionKind::StopReview => run_stop_action(&app, &action)
                                .await
                                .map(|_| model::ActionExecutionResult::Done),
                        }
                    })
                });
            // AB#1204 claim releaser — the ONLY place naming `review::claim_store`, injected like the
            // executor so the outbox slice stays review-blind. The worker calls it when a row goes
            // terminal (`done`/`dead`) to drop that row's review-execution claim; a no-op for rows
            // that never had one. Table hygiene only — correctness never depends on it.
            let claim_releaser: outbox::ClaimReleaser =
                Arc::new(|db: &db::Database, outbox_id: i64| {
                    if let Err(e) = review::claim_store::release_claim(db, outbox_id) {
                        eprintln!(
                            "outbox review claim：释放失败（outbox_id={outbox_id}）：{}",
                            e.message
                        );
                    }
                });
            // The AB#1204 F3 pre-worker reconcile closure: mark sessions left non-terminal by a dead
            // previous process (pr-review F1) as `failed` BEFORE the outbox worker can process any
            // row. `OutboxManager::start` invokes this synchronously before spawning the worker, so
            // the load-bearing ORDERING (a crash-replayed outbox review re-entering `start_for_outbox`
            // must read an ALREADY-reconciled `review_session` row, not a stale `running`) is enforced
            // by control flow rather than a "keep this above start" comment. Best-effort: a reconcile
            // failure at startup is logged (not fatal) — it only degrades a replay's suppress decision,
            // and the worker's first sweep is anyway gated behind this returning. The closure is the
            // ONLY place naming `review::history_store` here; the outbox slice sees only `FnOnce`
            // (review-blind, same injection shape as the executor / claim releaser).
            let reconcile_before_worker = {
                let app = app.handle().clone();
                move || {
                    let db = app.state::<db::Database>();
                    match review::history_store::fail_orphaned_sessions(db.inner()) {
                        Ok(stale) if stale > 0 => {
                            eprintln!("启动：{stale} 个遗留未完成 review 会话已标记 failed");
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!(
                            "启动：遗留 review 会话 reconcile 失败（继续启动）：{}",
                            e.message
                        ),
                    }
                }
            };
            // Start the outbox worker (AB#1066): drains the durable queue, retries failures with
            // backoff, dead-letters at the attempt cap. Its first sweep is immediate, so any rows
            // persisted before a previous exit resume now (restart-resume). Killed on app shutdown.
            // The reconcile closure runs first (AB#1204 F3 ordering guard).
            state.outbox.start(
                app.handle().clone(),
                outbox_executor,
                claim_releaser,
                reconcile_before_worker,
            );

            // Install the review→outbox notification sink (AB#1066) — the ONLY bridge from the review
            // slice's notification producer to the outbox slice. `review::deeplink` builds a
            // `model::Notification` and calls `state.notify_outbox.enqueue(note)`; THIS closure (the
            // sole place naming both the producer and `crate::outbox`) serializes it as the action
            // payload and enqueues it. Keeps `review` decoupled — it names neither `crate::outbox` nor
            // `ActionKind` (the Rust slice-boundary test locks this).
            state.notify_outbox.set_sink(Arc::new({
                let app = app.handle().clone();
                move |note: model::Notification| {
                    let channels = config::service::enabled_notification_channels(&app)?;
                    for channel in channels {
                        let delivery = model::NotificationDeliveryPayload {
                            notification: note.clone(),
                            channel_id: channel.id.clone(),
                            kind: channel.kind,
                        };
                        let summary = format!("{} via {}", note.title, channel.name);
                        let payload = serde_json::to_string(&delivery).map_err(|e| {
                            error::AppError::new(format!("outbox 通知序列化失败：{e}"))
                        })?;
                        outbox::service::enqueue(
                            &app,
                            &note.project_id,
                            model::ActionKind::Notification,
                            &summary,
                            &payload,
                        )?;
                    }
                    Ok(())
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
            // Start the Remote Access listener runtime (AB#1225): reconcile `config.listeners[]` →
            // bound loopback listeners. local-api is the sole real binder this PR — it mounts the
            // 127.0.0.1-only trigger control plane (`review::local_api::build_router`) on the port
            // from its `listeners[]` entry (NEVER tunneled; the TOKEN is still read live per request,
            // so setting/clearing it in Settings takes effect without a restart). Fail-closed: a
            // non-loopback bindHost is refused (needs AB#1073); remote-web/terminal/event-ingress are
            // reported `unsupported`. A per-listener bind failure is captured in status, never crashes
            // the app. Reconciled again after each `set_config` save.
            match config::service::load(app.handle()) {
                Ok(cfg) => state.remote.reconcile(
                    app.handle(),
                    &cfg.listeners,
                    &cfg.tunnels,
                    &cfg.cloudflared_bin,
                ),
                Err(e) => eprintln!("Remote 监听运行时：读取配置失败，跳过初次 reconcile：{e}"),
            }
            // Install the post-save reconcile hook (AB#1225 F4) — the ONLY place that bridges
            // config→remote. `config::commands::set_config` fires `state.config_saved` after a save;
            // THIS closure (capturing the concrete Wry `AppHandle` at install time, which sidesteps
            // the generic-`R` problem `set_config` would otherwise hit) reconciles the listener
            // runtime. The `config` slice never names `crate::remote` — the seam mirrors the
            // review→outbox notification sink + the webhook ingestor. (The startup reconcile above is
            // a direct call: lib.rs is the composition root, so naming remote here is by design.)
            state.config_saved.set_hook(Arc::new({
                let app = app.handle().clone();
                move |cfg: config::model::AppConfig| {
                    app.state::<AppState>().remote.reconcile(
                        &app,
                        &cfg.listeners,
                        &cfg.tunnels,
                        &cfg.cloudflared_bin,
                    );
                }
            }));

            // Deeplink trigger (AB#1045): route opened `prmonitor://review?…` URLs into the
            // `trigger_review` funnel. `register_all` is DEBUG-ONLY — it runtime-registers the
            // scheme for Windows/Linux `tauri dev`; a release build relies solely on the static
            // OS registration (Info.plist on macOS, installer on Windows), so production never
            // honors a runtime-registered (forge-able) scheme. macOS can only test deeplinks from
            // a packaged .app (scheme via Info.plist `CFBundleURLTypes`, not registrable at runtime).
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                #[cfg(debug_assertions)]
                let _ = app.deep_link().register_all();
                let handle = app.handle().clone();
                app.deep_link().on_open_url(move |event| {
                    review::deeplink::handle_review_deeplink(handle.clone(), event.urls());
                });
                // Cold-start deeplink (codex F1): `on_open_url` ONLY fires while the app is running,
                // so a link that LAUNCHED the app must be read here via `get_current` (the plugin's
                // doc-prescribed "on app load" path). On Windows/Linux it returns the launch argv;
                // on macOS the launch URL arrives later via `RunEvent::Opened` (which re-fires
                // `on_open_url`), so `get_current` is empty at setup there — net single handling on
                // every platform. A duplicate (if both paths ever fire) is dedup-safe via
                // `trigger_review`'s `try_reserve_pair`.
                if let Ok(Some(urls)) = app.deep_link().get_current() {
                    review::deeplink::handle_review_deeplink(app.handle().clone(), urls);
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            config::commands::app_version,
            config::commands::get_config,
            config::commands::set_config,
            notification_test_send,
            notification::send_notification,
            pr::commands::start_polling,
            pr::commands::stop_polling,
            pr::commands::poll_now,
            pr::commands::reschedule,
            pr::commands::gh_status,
            pr::commands::az_status,
            pr::commands::get_prs,
            pr::commands::set_pr_archived,
            pr::commands::start_webhook,
            pr::commands::stop_webhook,
            pr::commands::webhook_status,
            pr::commands::webhook_deliveries,
            pr::commands::poll_status,
            inbox::commands::inbox_list,
            inbox::commands::inbox_get_raw,
            inbox::commands::inbox_replay,
            outbox::commands::outbox_list,
            outbox::commands::outbox_get_raw,
            outbox::commands::outbox_retry,
            rule::commands::rule_matches_for_inbox,
            review::commands::get_codex_status,
            review::commands::get_claude_status,
            review::commands::start_codex,
            review::commands::stop_codex,
            review::commands::start_review,
            review::commands::trigger_review,
            review::commands::stop_review,
            review::commands::send_review_message,
            review::commands::list_review_sessions,
            review::commands::get_session_history,
            review::commands::get_pr_sessions,
            config::commands::set_active_project,
            remote::commands::get_listener_runtime_status,
            terminal::commands::list_terminal_sessions,
            terminal::commands::create_terminal_session,
            terminal::commands::attach_terminal,
            terminal::commands::detach_terminal,
            terminal::commands::close_terminal_session,
            terminal::commands::send_terminal_input,
            terminal::commands::resize_terminal,
            terminal::commands::get_terminal_status,
            terminal::commands::stop_terminal_daemon,
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
                // Stop the Remote Access listener runtime (AB#1225): fire each listener's graceful
                // shutdown + abort its serve task so none outlives the app (same contract).
                state.remote.shutdown();
                // Stop the action-outbox worker (AB#1066): signal + abort the task so it never
                // outlives the app (same "软件关闭时一起关闭" contract).
                state.outbox.shutdown();
                // Kill the resident iTerm daemon (#1383): same "软件关闭时一起关闭" contract as
                // codex — the python child never outlives the app.
                state.terminal.shutdown();
                // Kill every Web PTY shell child (#1372): same contract — SIGKILL + detached reap,
                // so no shell (and no reader thread) outlives the app.
                state.web_pty.shutdown();
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
/// `(project_id, candidates)` through the SAME [`run_auto_dispatch`] producer, so this
/// single helper removes the duplicated `Arc::new(move |..| Box::pin(run_auto_dispatch(..)))`
/// closure that was built verbatim at both wiring sites.
fn make_dispatcher<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> pr::scheduler::ProjectDispatcher {
    Arc::new(move |project_id, cands| {
        let app = app.clone();
        Box::pin(run_auto_dispatch(app, project_id, cands))
    })
}

/// Execute an AB#1069 `review` / `check` outbox action (the composition root's executor arm body):
/// deserialize the routing payload and run it through the review funnel
/// ([`review::commands::start_for_outbox`], which returns an executor outcome that separates a real
/// start / suppressed replay from active-session dedupe). `kind` is the funnel string the executor derived from the sealed
/// [`model::ActionKind`] variant (`Review` → `"review"`, `Check` → `"check"`), so it is valid by
/// construction. A deser `Err` propagates so the worker retries / dead-letters rather than marking the
/// row falsely `done`.
///
/// Routing key is `action.project_id` — the outbox ROW's single-source key (AB#1069 F3), NOT a
/// payload copy; the payload carries the backend-only [`model::Candidate`] so a successful executor
/// pass can land the dispatch ledger. Retry semantics (at-least-once): a `Deduped` (review already
/// in flight) resolves `Ok` so a crash-replay never dead-letters a running review. A prior attempt
/// that FAILED mid-start leaves a `Failed` session, which does NOT block a re-dispatch
/// (`try_reserve_pair` excludes `Failed`), so a retry genuinely re-runs the start — the intended
/// at-least-once behavior.
async fn run_review_action(
    app: &tauri::AppHandle,
    action: &outbox::OutboxAction,
    kind: &str,
) -> error::AppResult<()> {
    let payload: model::ReviewActionPayload = serde_json::from_str(&action.payload)
        .map_err(|e| error::AppError::new(format!("outbox review action 反序列化失败：{e}")))?;
    let mut candidate = payload.candidate;
    candidate.kind = kind.to_string();
    let state = app.state::<AppState>();
    let outcome = review::commands::start_for_outbox(
        app,
        state.inner(),
        &action.project_id,
        candidate.number,
        kind,
        // AB#1204: the outbox ROW id is the dedup key — a crash-replay of this row resolves its
        // prior review's claim instead of starting a duplicate.
        action.id,
    )
    .await?;
    if review::commands::should_record_dispatch_ledger(outcome) {
        pr::ledger::record_dispatched(app, &action.project_id, &[candidate])?;
    }
    Ok(())
}

/// Execute an AB#1069 `stop-review` outbox action (the composition root's executor arm body):
/// deserialize the `(pr, kind)` payload and interrupt the matching in-flight session
/// ([`review::commands::stop_for_outbox`], keyed by the ROW's `project_id` + payload `(pr, kind)`).
/// IDEMPOTENT — no live session is a benign `Ok(())`, so an at-least-once replay (or a stop fired
/// after the review already self-completed) never dead-letters; a bare reservation retries (F4).
/// Routing key is `action.project_id` (the row's single source, AB#1069 F3), not a payload copy.
/// A deser `Err` propagates (retry / dead-letter).
async fn run_stop_action(
    app: &tauri::AppHandle,
    action: &outbox::OutboxAction,
) -> error::AppResult<()> {
    let payload: model::StopReviewActionPayload =
        serde_json::from_str(&action.payload).map_err(|e| {
            error::AppError::new(format!("outbox stop-review action 反序列化失败：{e}"))
        })?;
    let state = app.state::<AppState>();
    review::commands::stop_for_outbox(
        app,
        state.inner(),
        &action.project_id,
        payload.pr_number,
        &payload.kind,
    )
    .await
}

/// Composition-root assembly for one auto-trigger cycle: validate config, snapshot active
/// `(pr, kind)` pairs, and inject durable inbox/outbox writers + UI error reporting into the
/// default [`dispatch::auto_dispatch`] producer. The executor is the only place that starts review
/// work; this path only materializes replayable actions.
async fn run_auto_dispatch<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: String,
    events: Vec<DiscoveredEvent>,
) {
    if events.is_empty() {
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
    let state = app.state::<AppState>();
    if should_skip_auto_dispatch_for_stopped_codex(project.engine_kind, state.codex.is_stopped()) {
        eprintln!("auto-dispatch 跳过本轮（{project_id}）：codex app-server 已停止");
        return;
    }
    let db = app.state::<db::Database>();
    for mut discovered in events {
        discovered.event.project_id = project_id.clone();
        discovered.event.received_at_epoch = inbox::store::now_epoch();
        if discovered.event.repo.is_empty() {
            discovered.event.repo = project.repo.clone();
        }
        let inserted = match inbox::store::insert_dedup(
            db.inner(),
            &discovered.event,
            "rule-engine-discovery",
            None,
            Some(&discovered.candidate),
        ) {
            Ok(id) => id,
            Err(e) => {
                emit_dispatch_error(
                    &app,
                    &project_id,
                    format!("规则引擎 inbox 持久化失败：{}", e.message),
                );
                continue;
            }
        };
        let Some(inbox_id) = inserted else {
            continue;
        };
        inbox::service::emit_for_id(&app, db.inner(), &project_id, inbox_id);
        let result = process_rule_event(
            &app,
            inbox_id,
            discovered.event.clone(),
            Some(discovered.candidate.clone()),
        );
        if let Err(e) = inbox::service::record_terminal(db.inner(), inbox_id, &result) {
            eprintln!(
                "rule-engine: 记录 inbox 终态失败（id={inbox_id}）：{}",
                e.message
            );
        }
        if let Err(e) = result {
            emit_dispatch_error(&app, &project_id, format!("规则执行失败：{}", e.message));
        }
        inbox::service::emit_for_id(&app, db.inner(), &project_id, inbox_id);
    }
}

fn process_rule_event<R: Runtime>(
    app: &tauri::AppHandle<R>,
    inbox_event_id: i64,
    event: Event,
    candidate: Option<Candidate>,
) -> error::AppResult<()> {
    let cfg = config::service::load(app)?;
    let plans = rule::service::plan_event(&cfg.rules, &event, candidate.as_ref());
    if plans.is_empty() {
        return Ok(());
    }

    let db = app.state::<db::Database>();
    let mut announced_action_ids = Vec::new();
    let mut blocking_errors = Vec::new();
    for plan in plans {
        let now = rule::store::now_epoch();
        let (action_ids, plan_blocking_errors) = db.inner().with_tx(|tx| {
            let mut action_ids = Vec::new();
            let mut errors = plan.errors.clone();
            let mut blocking = plan.errors.clone();
            for action in &plan.actions {
                match outbox::store::enqueue_deduped_in_tx(
                    tx,
                    &plan.project_id,
                    action.kind,
                    &action.summary,
                    &action.payload,
                    &action.dedupe_key,
                    now,
                ) {
                    Ok(id) => action_ids.push(id),
                    Err(e) => {
                        errors.push(e.message.clone());
                        blocking.push(e.message);
                    }
                }
            }
            let error = (!errors.is_empty()).then(|| errors.join("; "));
            rule::store::insert_match_in_tx(
                tx,
                rule::store::NewRuleMatch {
                    rule_id: &plan.rule_id,
                    rule_name: &plan.rule_name,
                    inbox_event_id,
                    project_id: &plan.project_id,
                    action_ids: &action_ids,
                    error: error.as_deref(),
                    now,
                },
            )?;
            Ok((action_ids, blocking))
        })?;
        announced_action_ids.extend(action_ids);
        blocking_errors.extend(plan_blocking_errors);
    }

    for id in &announced_action_ids {
        outbox::service::announce_updated(app, db.inner(), *id);
    }
    if !announced_action_ids.is_empty() {
        app.state::<AppState>().outbox.wake();
    }
    if blocking_errors.is_empty() {
        Ok(())
    } else {
        Err(error::AppError::new(format!(
            "规则动作入队失败：{}",
            blocking_errors.join("; ")
        )))
    }
}

fn should_skip_auto_dispatch_for_stopped_codex(
    engine_kind: model::EngineKind,
    codex_stopped: bool,
) -> bool {
    engine_kind == model::EngineKind::Codex && codex_stopped
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
    stream::emit(
        app,
        events::StreamEvent::Review(events::ReviewEvent::DispatchError {
            project_id: project_id.to_string(),
            message,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[test]
    fn auto_dispatch_skips_only_stopped_codex_projects() {
        assert!(
            should_skip_auto_dispatch_for_stopped_codex(model::EngineKind::Codex, true),
            "user-stopped codex should prevent automatic outbox production"
        );
        assert!(
            !should_skip_auto_dispatch_for_stopped_codex(model::EngineKind::Codex, false),
            "running codex projects can auto-dispatch"
        );
        assert!(
            !should_skip_auto_dispatch_for_stopped_codex(model::EngineKind::Claude, true),
            "Claude has no resident codex stop flag"
        );
    }

    #[test]
    fn process_rule_event_enqueues_actions_and_links_trace() {
        let app = tauri::test::mock_app();
        let db = Database::open_in_memory().expect("open db");
        let config = config::model::AppConfig {
            projects: vec![config::model::Project {
                id: "p1".to_string(),
                repo: "owner/repo".to_string(),
                enabled: false,
                ..config::model::Project::default()
            }],
            active_project_id: "p1".to_string(),
            rules: vec![config::model::RuleConfig {
                id: "r1".to_string(),
                name: "Ready".to_string(),
                enabled: true,
                event_type: Some(model::EventType::PullRequest),
                project_id: "p1".to_string(),
                labels_any: vec!["ready".to_string()],
                actions: vec![
                    config::model::RuleActionKind::Review,
                    config::model::RuleActionKind::Check,
                    config::model::RuleActionKind::Notify,
                ],
                ..config::model::RuleConfig::default()
            }],
            ..config::model::AppConfig::default()
        };
        let config_json = serde_json::to_string(&config).expect("config serializes");
        db.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO config_blob (id, json) VALUES (1, ?1)",
                [config_json],
            )?;
            Ok(())
        })
        .expect("seed config");
        let event = model::Event {
            dedupe_key: "delivery-1".to_string(),
            source: model::SourceKind::Github,
            event_type: model::EventType::PullRequest,
            project_id: "p1".to_string(),
            repo: "owner/repo".to_string(),
            number: Some(7),
            title: "Ready to ship".to_string(),
            body: String::new(),
            labels: vec!["ready".to_string()],
            url: "https://example.com/pull/7".to_string(),
            received_at_epoch: 10,
        };
        let inbox_id = inbox::store::insert_dedup(&db, &event, "raw", None, None)
            .expect("insert inbox")
            .expect("new inbox");
        app.manage(db);
        app.manage(AppState::default());
        let candidate = Candidate {
            number: 7,
            head_sha: "sha".to_string(),
            head_ref: "feature/rules".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            kind: "review".to_string(),
        };

        process_rule_event(app.handle(), inbox_id, event, Some(candidate)).expect("process rules");

        let db = app.state::<Database>();
        let kinds: Vec<String> = db
            .inner()
            .with_conn(|conn| {
                let mut stmt = conn.prepare("SELECT kind FROM action_outbox ORDER BY id")?;
                let rows = stmt
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .expect("read outbox");
        assert_eq!(kinds, vec!["review", "check", "notification"]);

        let trace = rule::store::list_by_inbox(db.inner(), inbox_id).expect("trace");
        assert_eq!(trace.len(), 1);
        assert_eq!(trace[0].rule_id, "r1");
        assert_eq!(trace[0].action_count, 3);
        assert_eq!(trace[0].action_outbox_ids.len(), 3);
        assert_eq!(trace[0].error, None);
    }

    #[test]
    fn process_rule_event_plan_errors_fail_inbox_lifecycle() {
        let app = tauri::test::mock_app();
        let db = Database::open_in_memory().expect("open db");
        let config = config::model::AppConfig {
            projects: vec![config::model::Project {
                id: "p1".to_string(),
                repo: "owner/repo".to_string(),
                enabled: false,
                ..config::model::Project::default()
            }],
            active_project_id: "p1".to_string(),
            rules: vec![config::model::RuleConfig {
                id: "r1".to_string(),
                name: "Needs candidate".to_string(),
                enabled: true,
                event_type: Some(model::EventType::PullRequest),
                project_id: "p1".to_string(),
                labels_any: vec!["ready".to_string()],
                actions: vec![config::model::RuleActionKind::Review],
                ..config::model::RuleConfig::default()
            }],
            ..config::model::AppConfig::default()
        };
        let config_json = serde_json::to_string(&config).expect("config serializes");
        db.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO config_blob (id, json) VALUES (1, ?1)",
                [config_json],
            )?;
            Ok(())
        })
        .expect("seed config");
        let event = model::Event {
            dedupe_key: "delivery-1".to_string(),
            source: model::SourceKind::Github,
            event_type: model::EventType::PullRequest,
            project_id: "p1".to_string(),
            repo: "owner/repo".to_string(),
            number: Some(7),
            title: "Ready to ship".to_string(),
            body: String::new(),
            labels: vec!["ready".to_string()],
            url: "https://example.com/pull/7".to_string(),
            received_at_epoch: 10,
        };
        let inbox_id = inbox::store::insert_dedup(&db, &event, "raw", None, None)
            .expect("insert inbox")
            .expect("new inbox");
        app.manage(db);
        app.manage(AppState::default());

        let err = process_rule_event(app.handle(), inbox_id, event, None).unwrap_err();

        assert!(err.message.contains("需要 PR candidate"), "{}", err.message);
        let db = app.state::<Database>();
        let trace = rule::store::list_by_inbox(db.inner(), inbox_id).expect("trace");
        assert_eq!(trace.len(), 1);
        assert_eq!(trace[0].action_count, 0);
        assert!(trace[0]
            .error
            .as_deref()
            .is_some_and(|e| e.contains("需要 PR candidate")));
    }

    #[test]
    fn notification_action_payload_accepts_legacy_naked_notification() {
        let note = model::Notification::new(
            model::NotificationLevel::Info,
            "done".to_string(),
            "https://example.com/pr/1".to_string(),
            model::RedactedNotificationBody::fixed("review done"),
            "p1".to_string(),
        );
        let raw = serde_json::to_string(&note).expect("legacy note serializes");

        match parse_notification_action_payload(&raw).expect("legacy note parses") {
            NotificationActionPayload::Legacy(parsed) => assert_eq!(parsed, note),
            NotificationActionPayload::Delivery(_) => panic!("legacy note parsed as new payload"),
        }
    }

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
