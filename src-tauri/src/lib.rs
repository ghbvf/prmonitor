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
mod composition;
pub mod config;
pub mod db;
pub mod error;
pub mod events;
pub mod inbox;
pub mod messaging;
pub mod messaging_outbox;
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
pub mod workflow;

#[cfg(test)]
mod managed_cli_guard_test;
/// Rust slice-boundary enforcement test (AB#1066 F1, Medium carrier) — test-only module.
#[cfg(test)]
mod slice_boundary_test;
#[cfg(test)]
mod typegen;

use std::sync::Arc;

use model::{Candidate, EventEnvelope};
use pr::source::DiscoveredEvent;
use state::AppState;
use tauri::{Manager, Runtime};

/// Composition-level CLI probe: config owns resolution, while only the root may aggregate active
/// resident fingerprints from review/webhook/remote without creating sibling-slice coupling.
#[tauri::command]
fn probe_cli_tools(
    state: tauri::State<'_, AppState>,
    cli_tools: config::service::CliToolsConfig,
    refresh_path: bool,
) -> error::AppResult<Vec<config::service::CliToolProbeStatus>> {
    Ok(composition::probe_cli_tools(
        &state.cli_resolver,
        &cli_tools,
        refresh_path,
        &composition::ActiveCliFingerprints {
            codex: state.codex.active_fingerprint(),
            agent: state.cursor.active_fingerprint(),
            webhook_cloudflared: state.webhook.active_cloudflared_fingerprint(),
            remote_cloudflared: state.remote.active_cloudflared_fingerprints(),
        },
    ))
}

/// Resolve cloudflared when Remote Access has an enabled tunnel that needs the managed
/// CLI: `Quick`, or `Command` whose program token is the bare `cloudflared` name.
/// Other modes/commands must not be degraded by an unrelated cloudflared config error.
fn resolve_remote_cloudflared<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    remote_access: &config::model::RemoteAccessConfig,
) -> error::AppResult<Option<config::service::ResolvedCli>> {
    if remote_tunnels_need_cloudflared(remote_access) {
        config::service::resolve_cli(app, model::CliTool::Cloudflared, false).map(Some)
    } else {
        Ok(None)
    }
}

fn remote_tunnels_need_cloudflared(remote_access: &config::model::RemoteAccessConfig) -> bool {
    remote_access.tunnels.iter().any(|tunnel| {
        tunnel.enabled
            && (tunnel.mode == config::model::RemoteTunnelMode::Quick
                || (tunnel.mode == config::model::RemoteTunnelMode::Command
                    && config::service::tunnel_command_uses_bare_cloudflared(&tunnel.command)))
    })
}

struct MessagingActionsImpl;

impl<R: tauri::Runtime> messaging::service::MessagingActions<R> for MessagingActionsImpl {
    fn enqueue_reply(
        &self,
        app: &tauri::AppHandle<R>,
        _integration_id: &str,
        kind: model::ActionKind,
        summary: &str,
        payload_json: &str,
        dedupe_key: &str,
    ) -> error::AppResult<i64> {
        outbox::service::enqueue_deduped(
            app,
            messaging_outbox::MESSAGING_OUTBOX_SCOPE,
            kind,
            summary,
            payload_json,
            dedupe_key,
        )
    }

    fn enqueue_send(
        &self,
        app: &tauri::AppHandle<R>,
        kind: model::ActionKind,
        summary: &str,
        payload_json: &str,
        dedupe_key: &str,
    ) -> error::AppResult<i64> {
        outbox::service::enqueue_deduped(
            app,
            messaging_outbox::MESSAGING_OUTBOX_SCOPE,
            kind,
            summary,
            payload_json,
            dedupe_key,
        )
    }

    fn enqueue_send_after(
        &self,
        app: &tauri::AppHandle<R>,
        kind: model::ActionKind,
        summary: &str,
        payload_json: &str,
        dedupe_key: &str,
        delay_secs: u64,
    ) -> error::AppResult<i64> {
        outbox::service::enqueue_deduped_after(
            app,
            messaging_outbox::MESSAGING_OUTBOX_SCOPE,
            kind,
            summary,
            payload_json,
            dedupe_key,
            delay_secs,
        )
    }

    fn enqueue_send_once_after(
        &self,
        app: &tauri::AppHandle<R>,
        kind: model::ActionKind,
        summary: &str,
        payload_json: &str,
        dedupe_key: &str,
        delay_secs: u64,
    ) -> error::AppResult<i64> {
        let db = app.state::<db::Database>();
        if let Some(existing_id) = outbox::store::id_by_dedupe_key_any_status(
            db.inner(),
            messaging_outbox::MESSAGING_OUTBOX_SCOPE,
            dedupe_key,
        )? {
            return Ok(existing_id);
        }
        outbox::service::enqueue_deduped_after(
            app,
            messaging_outbox::MESSAGING_OUTBOX_SCOPE,
            kind,
            summary,
            payload_json,
            dedupe_key,
            delay_secs,
        )
    }

    fn list_sends(
        &self,
        app: &tauri::AppHandle<R>,
        integration_id: Option<&str>,
    ) -> error::AppResult<Vec<model::OutboxEntry>> {
        let db = app.state::<db::Database>();
        let entries = outbox::store::list_by_project(
            db.inner(),
            Some(messaging_outbox::MESSAGING_OUTBOX_SCOPE),
        )?;
        let mut filtered = Vec::new();
        for entry in entries.into_iter().filter(|entry| {
            matches!(
                entry.kind,
                model::ActionKind::MessagingReply | model::ActionKind::MessagingSend
            )
        }) {
            if let Some(expected) = integration_id {
                let payload = outbox::store::get_raw(db.inner(), entry.id)?.unwrap_or_default();
                if !messaging_outbox_integration_matches(&payload, expected) {
                    continue;
                }
            }
            filtered.push(entry);
        }
        Ok(filtered)
    }

    fn submit_review(
        &self,
        app: &tauri::AppHandle<R>,
        reference: String,
        pr_number: u64,
        extra_args: String,
        request_id: model::ExternalRequestId,
    ) -> error::AppResult<model::ReviewReceiptId> {
        app.state::<AppState>().external_review.submit(
            reference,
            pr_number,
            extra_args,
            request_id,
            model::ExternalTriggerOrigin::MessagingBot,
            false,
        )
    }
}

fn messaging_outbox_integration_matches(payload_json: &str, integration_id: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(payload_json)
        .ok()
        .and_then(|value| {
            value
                .get("integrationId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .is_some_and(|id| id == integration_id)
}

pub(crate) fn make_local_api_ctx<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    port: u16,
    base_path: String,
    remote_entrypoint_id: Option<String>,
) -> review::local_api::Ctx<R> {
    review::local_api::Ctx {
        app,
        port,
        base_path,
        remote_entrypoint_id,
    }
}

pub(crate) fn make_messaging_local_api_ctx<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    port: u16,
    remote_entrypoint_id: Option<String>,
) -> messaging::local_api::Ctx<R> {
    messaging::local_api::Ctx {
        app,
        port,
        remote_entrypoint_id,
    }
}

fn reconcile_blocked_reviews_after_restart(db: &db::Database) -> error::AppResult<u64> {
    outbox::store::unblock_reviews(db, outbox::store::now_epoch())
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
        model::ActionExecutionResult::Done { .. } => {
            Ok(format!("通知渠道「{}」测试发送成功", channel.name))
        }
        model::ActionExecutionResult::Blocked { message, .. }
        | model::ActionExecutionResult::Retry { message, .. }
        | model::ActionExecutionResult::Dead { message } => Err(error::AppError::new(message)),
    }
}

#[tauri::command]
async fn start_review<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    state: tauri::State<'_, AppState>,
    project_id: String,
    pr_number: u64,
    extra_args: Option<String>,
) -> error::AppResult<String> {
    let project = config::service::project_validated(&app, &project_id)?;
    let config = config::service::load(&app)?;
    let invocation = rule::service::resolve_skill_invocation(
        &config.rules,
        &project,
        pr_number,
        extra_args.as_deref().unwrap_or(""),
    )
    .map_err(error::AppError::new)?;
    // SAFETY: this composition-root command is an authorized durable review ingress.
    let capability = unsafe { review::engine::ReviewStartCapability::new_composition_root() };
    review::commands::start_review_authorized(
        &capability,
        app,
        state.inner(),
        project_id,
        pr_number,
        invocation,
    )
    .await
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
        cli::Invocation::Message(args) => {
            std::process::exit(cli::run_message_client_blocking(&args))
        }
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
        .manage(messaging::service::MessagingRuntime::<tauri::Wry> {
            actions: Arc::new(MessagingActionsImpl),
        })
        .manage(messaging::human_input::HumanInputBroker::default())
        .manage(messaging::service::MessagingEventWorker::default())
        .manage(messaging::feishu_long_connection::FeishuConnectionManager::default())
        .manage(messaging::dingtalk_stream::DingTalkConnectionManager::default())
        .setup(|app| {
            // Open + migrate the unified SQLite store and manage it as a `tauri::State`
            // BEFORE anything that reads persistence (config load / poll start). It is a
            // `State` rather than an `AppState` field because `app_data_dir()` only
            // resolves here in `setup`, while `AppState` is `.manage()`d at builder time.
            // Then run the one-time legacy JSON → SQLite import (#70) so existing users'
            // config / tracked PRs / ledger carry over before the first read.
            app.manage(db::Database::open(app.handle())?);
            // MCP waiters are process-local. A request left pending in SQLite belongs to the
            // previous process and can never resume after this launch, so terminalize it before
            // the local MCP endpoint starts accepting sessions.
            let orphaned_human_inputs = {
                let db = app.state::<db::Database>();
                let broker = app.state::<messaging::human_input::HumanInputBroker>();
                let requests = messaging::human_input::reconcile_startup_orphans(
                    db.inner(),
                    broker.inner(),
                    messaging::store::now_epoch(),
                )?;
                messaging::human_input::prune_terminal(
                    db.inner(),
                    messaging::store::now_epoch(),
                )?;
                requests
            };
            for request in orphaned_human_inputs {
                let app = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    messaging::mcp::update_terminal_card(&app, &request).await;
                });
            }
            let maintenance_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut interval =
                    tokio::time::interval(std::time::Duration::from_secs(30));
                // Startup reconciliation above already established the initial state.
                interval.tick().await;
                loop {
                    interval.tick().await;
                    let now = messaging::store::now_epoch();
                    let expired = {
                        let db = maintenance_app.state::<db::Database>();
                        let broker = maintenance_app
                            .state::<messaging::human_input::HumanInputBroker>();
                        match messaging::human_input::expire_due(db.inner(), broker.inner(), now) {
                            Ok(expired) => expired,
                            Err(error) => {
                                eprintln!("清理超时人工输入请求失败：{}", error.message);
                                Vec::new()
                            }
                        }
                    };
                    for request in expired {
                        messaging::mcp::update_terminal_card(&maintenance_app, &request).await;
                    }
                    let db = maintenance_app.state::<db::Database>();
                    if let Err(error) = messaging::human_input::prune_terminal(db.inner(), now) {
                        eprintln!("清理人工输入终态记录失败：{}", error.message);
                    }
                }
            });
            app.state::<messaging::service::MessagingEventWorker>()
                .start(app.handle().clone());
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
            composition::install_review_lifecycle_sink(&state, app.handle().clone());
            state.review_resume.set_sink(Arc::new({
                let app = app.handle().clone();
                move || {
                    let db = app.state::<db::Database>();
                    outbox::store::unblock_reviews(db.inner(), outbox::store::now_epoch())?;
                    app.state::<AppState>().outbox.wake();
                    Ok(())
                }
            }));
            state.external_review.set_sinks(
                Arc::new({
                    let app = app.handle().clone();
                    move |reference,
                          pr_number,
                          extra_args,
                          request_id,
                          origin,
                          notify_on_completion| {
                        let project = config::service::project_by_ref_validated(&app, &reference)?;
                        let config = config::service::load(&app)?;
                        let invocation = rule::service::resolve_skill_invocation(
                            &config.rules,
                            &project,
                            pr_number,
                            &extra_args,
                        )
                        .map_err(error::AppError::new)?;
                        let dedupe = model::InboxDedupeKey::new(format!("external:{request_id}"))
                            .map_err(error::AppError::new)?;
                        // Skill identity is resolved from rules at plan time; payload fields are
                        // placeholders for wire/DB shape only (ingress no longer accepts free skill).
                        let event = model::EventEnvelope::review_request(
                            dedupe,
                            project.source_kind,
                            project.id.clone(),
                            project.repo.clone(),
                            pr_number,
                            model::DEFAULT_SKILL_NAME,
                            extra_args.clone(),
                            model::DEFAULT_SKILL_PATH,
                            model::DEFAULT_COMMAND_TEMPLATE,
                            request_id,
                            origin,
                            notify_on_completion,
                            inbox::store::now_epoch(),
                        )
                        .map_err(error::AppError::new)?;
                        let db = app.state::<db::Database>();
                        let key = event.dedupe_key().as_str().to_string();
                        let id = match inbox::store::insert_dedup(
                            db.inner(),
                            &event,
                            "external-review-request",
                            None,
                            None,
                        )? {
                            Some(id) => id,
                            None => {
                                let id = inbox::store::id_by_dedupe_key(db.inner(), &key)?
                                    .ok_or_else(|| error::AppError::new("重复 review request 未找到 receipt"))?;
                                let existing = inbox::store::get_entry(db.inner(), id)?
                                    .ok_or_else(|| error::AppError::new("重复 review request 的 receipt 已丢失"))?;
                                ensure_same_external_review_request(&existing.event, &event)?;
                                id
                            }
                        };
                        let receipt = model::ReviewReceiptId::new(id).map_err(error::AppError::new)?;
                        let app_state = app.state::<AppState>();
                        app_state.inbox.wake();
                        if notify_on_completion {
                            app_state.workflow.start_receipt_notify(
                                app.clone(),
                                receipt,
                                reference,
                                pr_number,
                                invocation.skill_key,
                            )?;
                        }
                        Ok(receipt)
                    }
                }),
                Arc::new(inbox::store::get_review_receipt),
            );
            let workflow_app = app.handle().clone();
            state.workflow.set_actions(workflow::manager::WorkflowActions {
                wait_receipt: Arc::new({
                    let app = workflow_app.clone();
                    move |receipt_id| {
                    let app = app.clone();
                    Box::pin(async move {
                        let db = app.state::<db::Database>();
                        let state = app.state::<AppState>();
                        let project_id = inbox::store::get_entry(db.inner(), receipt_id.get())?
                            .map(|entry| entry.event.project_id().to_string())
                            .ok_or_else(|| error::AppError::new("review receipt 不存在"))?;
                        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
                        loop {
                            ticker.tick().await;
                            let receipt = state.external_review.get(db.inner(), receipt_id)?;
                            if matches!(receipt.status, model::ReviewReceiptStatus::Done | model::ReviewReceiptStatus::Failed) {
                                return Ok(workflow::manager::ReviewCompletion {
                                    thread_id: receipt.thread_id,
                                    project_id,
                                    status: receipt.status,
                                    comment_url: receipt.comment_url,
                                });
                            }
                        }
                    })
                    }
                }),
                send_notification: Arc::new({
                    let app = workflow_app.clone();
                    move |request, dedupe_prefix| {
                    let app = app.clone();
                    Box::pin(async move {
                        let state = app.state::<AppState>();
                        state
                            .notification_sender
                            .send(request, Some(dedupe_prefix))
                    })
                    }
                }),
            });
            // Install the auto-trigger event_sink BEFORE starting the loop, so the
            // immediate first tick already persists dispatchable reviews/checks into
            // inbox + outbox. The closure is the `pr` slice's review-agnostic
            // event sink; its body is the durable discovery ingestor, the
            // composition-root assembly that injects durable inbox/outbox writers into
            // the default producer.
            state
                .scheduler
                .set_event_sink(make_event_sink(app.handle().clone()));
            // Install the GitHub re-feed used exclusively by the inbox worker. It restores the
            // provider event for list/gate reconciliation; the worker then plans rules and commits
            // traces, actions, and Processed atomically. Live ingress only inserts and wakes.
            let github_refeed: inbox::GithubRefeed = Arc::new({
                move |app: tauri::AppHandle, webhook_event_json: String| {
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
                        pr::commands::ingest_webhook(&app, ev).await
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
                            .await
                    })
                }
            });
            let rule_processor: inbox::RuleProcessor = Arc::new(
                move |app: tauri::AppHandle,
                      inbox_event_id: i64,
                      event: EventEnvelope,
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
            state.inbox.start(app.handle().clone());
            // The GitHub ingestor: normalize the `WebhookEvent` → neutral `model::Event` HERE (pr
            // owns `WebhookEvent`), then hand the inbox the neutral event + the verbatim body + the
            // parsed JSON. The inbox persists+dedups and re-feeds via the injected `github_refeed` —
            // it never sees a `pr` type.
            state.webhook.set_ingestor(Arc::new({
                let app = app.handle().clone();
                move |raw: String, guid: Option<String>, ev: pr::webhook::WebhookEvent| {
                    let app = app.clone();
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
                        let webhook_event_json = serde_json::to_string(&ev).map_err(|error| {
                            error::AppError::new(format!(
                                "WebhookEvent 序列化失败，拒绝空 payload fallback：{error}"
                            ))
                        })?;
                        let db = app.state::<db::Database>();
                        // F1: return the DURABLE-PERSIST Result so the handler gates the ACK on it.
                        inbox::service::ingest_github(
                            &app,
                            db.inner(),
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
                move |raw, project_id, repo| {
                    let app = app.clone();
                    Box::pin(async move {
                        use tauri::Manager;
                        let db = app.state::<db::Database>();
                        // F1: return the DURABLE-PERSIST Result so the handler gates the ACK on it.
                        inbox::service::ingest_azure_refresh(
                            &app,
                            db.inner(),
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
            // each ActionKind to its provider — `Notification` → the desktop notifier; `RunSkill` →
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
                        match action.kind() {
                            model::ActionKind::Notification => {
                                let payload = match serde_json::from_str::<
                                    model::NotificationDeliveryPayload,
                                >(action.payload())
                                {
                                    Ok(payload) => payload,
                                    Err(e) => {
                                        return Ok(model::ActionExecutionResult::Dead {
                                            message: format!(
                                                "outbox 通知 payload 无效（仅接受 NotificationDeliveryPayload）：{e}"
                                            ),
                                        });
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
                                            "通知渠道「{}」类型不匹配：payload={:?} channel={:?}",
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
                            model::ActionKind::RunSkill => run_review_action(&app, &action).await,
                            model::ActionKind::StopReview => run_stop_action(&app, &action)
                                .await
                                .map(|_| model::ActionExecutionResult::done()),
                            model::ActionKind::MessagingReply => {
                                let payload =
                                    match serde_json::from_str::<model::MessagingReplyPayload>(
                                        action.payload(),
                                    ) {
                                        Ok(payload) => payload,
                                        Err(e) => {
                                            return Ok(model::ActionExecutionResult::Dead {
                                                message: format!(
                                                    "messaging reply payload 反序列化失败: {e}"
                                                ),
                                            });
                                        }
                                    };
                                messaging::service::execute_reply(&app, payload).await
                            }
                            model::ActionKind::MessagingSend => {
                                let payload =
                                    match serde_json::from_str::<model::MessagingSendPayload>(
                                        action.payload(),
                                    ) {
                                        Ok(payload) => payload,
                                        Err(e) => {
                                            return Ok(model::ActionExecutionResult::Dead {
                                                message: format!(
                                                    "messaging send payload 反序列化失败: {e}"
                                                ),
                                            });
                                        }
                                    };
                                messaging::service::execute_send(&app, payload).await
                            }
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
                    match reconcile_blocked_reviews_after_restart(db.inner()) {
                        Ok(resumed) if resumed > 0 => {
                            eprintln!("启动：{resumed} 个 blocked review 动作已恢复为 pending");
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!("启动：blocked review reconcile 失败：{}", e.message),
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
            state.workflow.start(app.handle().clone());

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
            // valid config lands, running the same gate. The event_sink hook above stays
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
                Ok(cfg) => {
                    let cloudflared = resolve_remote_cloudflared(app.handle(), &cfg.remote_access);
                    state
                        .remote
                        .reconcile(app.handle(), &cfg.remote_access, cloudflared);
                    app.state::<messaging::feishu_long_connection::FeishuConnectionManager>()
                        .reconcile(app.handle(), &cfg.messaging.integrations);
                    app.state::<messaging::dingtalk_stream::DingTalkConnectionManager>()
                        .reconcile(app.handle(), &cfg.messaging.integrations);
                }
                Err(e) => eprintln!("Remote 监听运行时：读取配置失败，跳过初次 reconcile：{e}"),
            }
            app.state::<messaging::service::MessagingEventWorker>().wake();
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
                    let cloudflared = resolve_remote_cloudflared(&app, &cfg.remote_access);
                    app.state::<AppState>().remote.reconcile(
                        &app,
                        &cfg.remote_access,
                        cloudflared,
                    );
                    app.state::<messaging::feishu_long_connection::FeishuConnectionManager>()
                        .reconcile(&app, &cfg.messaging.integrations);
                    app.state::<messaging::dingtalk_stream::DingTalkConnectionManager>()
                        .reconcile(&app, &cfg.messaging.integrations);
                    app.state::<messaging::service::MessagingEventWorker>()
                        .wake();
                }
            }));

            // Deeplink trigger (AB#1045): route opened `prmonitor://review?…` URLs into the
            // manual review funnel. `register_all` is DEBUG-ONLY — it runtime-registers the
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
                // manual start's `try_reserve_pair`.
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
            probe_cli_tools,
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
            messaging::commands::messaging_events_list,
            messaging::commands::messaging_event_raw,
            messaging::commands::messaging_event_replay,
            messaging::commands::messaging_send,
            messaging::commands::messaging_sends_list,
            messaging::commands::messaging_integrations_list,
            messaging::commands::messaging_connection_statuses_list,
            outbox::commands::outbox_list,
            outbox::commands::outbox_get_raw,
            outbox::commands::outbox_retry,
            rule::commands::rule_matches_for_inbox,
            review::commands::get_codex_status,
            review::commands::get_claude_status,
            review::commands::get_cursor_status,
            review::commands::start_codex,
            review::commands::stop_codex,
            review::commands::start_cursor,
            review::commands::stop_cursor,
            start_review,
            review::commands::stop_review,
            review::commands::send_review_message,
            review::commands::list_review_sessions,
            review::commands::get_session_history,
            review::commands::get_pr_sessions,
            workflow::commands::workflow_list,
            workflow::commands::workflow_get,
            workflow::commands::workflow_get_raw,
            workflow::commands::workflow_retry,
            config::commands::set_active_project,
            remote::commands::get_remote_access_runtime_status,
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
                // Kill the resident Cursor ACP (`agent acp`) child so it never outlives
                // the app — same "软件关闭时一起关闭" contract as codex.
                state.cursor.shutdown();
                // Kill the cloudflared tunnel + abort the receiver so neither outlives
                // the app (same "软件关闭时一起关闭" contract as codex).
                state.webhook.shutdown();
                // Stop the Remote Access listener runtime (AB#1225): fire each listener's graceful
                // shutdown + abort its serve task so none outlives the app (same contract).
                state.remote.shutdown();
                state.inbox.shutdown();
                // Stop the action-outbox worker (AB#1066): signal + abort the task so it never
                // outlives the app (same "软件关闭时一起关闭" contract).
                state.outbox.shutdown();
                // Stop the workflow worker (#1370) so no background saga task outlives the app.
                state.workflow.shutdown();
                // Kill the resident iTerm daemon (#1383): same "软件关闭时一起关闭" contract as
                // codex — the python child never outlives the app.
                state.terminal.shutdown();
                // Kill every Web PTY shell child (#1372): same contract — SIGKILL + detached reap,
                // so no shell (and no reader thread) outlives the app.
                state.web_pty.shutdown();
                app_handle
                    .state::<messaging::feishu_long_connection::FeishuConnectionManager>()
                    .stop();
                app_handle
                    .state::<messaging::dingtalk_stream::DingTalkConnectionManager>()
                    .stop();
                app_handle.state::<messaging::service::MessagingEventWorker>().shutdown();
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

/// Build the per-cycle discovery event sink the poll source uses.
/// share — the poll scheduler and the webhook ingestor. Both drive a dispatchable
/// `(project_id, candidates)` through the durable inbox producer.
/// closure that was built verbatim at both wiring sites.
fn make_event_sink<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
) -> pr::scheduler::DiscoveredEventSink {
    Arc::new(move |project_id, cands| {
        let app = app.clone();
        Box::pin(ingest_discovered_events(app, project_id, cands))
    })
}

fn ensure_same_external_review_request(
    existing: &model::EventEnvelope,
    requested: &model::EventEnvelope,
) -> error::AppResult<()> {
    let same_envelope = existing.source() == requested.source()
        && existing.project_id() == requested.project_id()
        && existing.repo() == requested.repo();
    let same_request = match (existing.as_review_request(), requested.as_review_request()) {
        (Some(existing), Some(requested)) => {
            existing.pr_number == requested.pr_number
                && existing.extra_args == requested.extra_args
                && existing.request_id == requested.request_id
                && existing.origin == requested.origin
                && existing.notify_on_completion == requested.notify_on_completion
        }
        _ => false,
    };
    if same_envelope && same_request {
        Ok(())
    } else {
        Err(error::AppError::new(format!(
            "requestId {} 已绑定到不同的 review request",
            requested
                .as_review_request()
                .map(|request| request.request_id.as_str())
                .unwrap_or("<invalid>")
        )))
    }
}

/// Execute an AB#1069 `review` / `check` outbox action (the composition root's executor arm body):
/// deserialize the routing payload and run it through the review funnel
/// ([`review::commands::start_for_outbox`], which returns an executor outcome that separates a real
/// start / suppressed replay from active-session dedupe). `skill_key` is the funnel string the executor derived from the sealed
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
) -> error::AppResult<model::ActionExecutionResult> {
    let payload: model::ReviewActionPayload = serde_json::from_str(action.payload())
        .map_err(|e| error::AppError::new(format!("outbox review action 反序列化失败：{e}")))?;
    let invocation = payload.invocation().clone();
    let (pr_number, candidate, explicit) = match payload {
        model::ReviewActionPayload::Automatic {
            mut candidate,
            invocation: inv,
        } => {
            candidate.skill_key = inv.skill_key.clone();
            model::ReviewActionKey::for_candidate(&candidate).map_err(error::AppError::new)?;
            (candidate.number, Some(candidate), false)
        }
        model::ReviewActionPayload::Explicit { pr_number, .. } => (pr_number, None, true),
    };
    let state = app.state::<AppState>();
    let project = config::service::project_validated(app, action.project_id())?;
    let observed_resume_generation = state.review_resume.generation();
    match project.engine_kind {
        model::EngineKind::Codex => {
            if state.codex.is_stopped() && !explicit {
                return Ok(model::ActionExecutionResult::Blocked {
                    message: "Codex 已由用户停止，等待显式恢复".to_string(),
                    observed_resume_generation,
                });
            }
            if explicit {
                state.codex.resume();
                state.review_resume.fire()?;
            }
        }
        model::EngineKind::Cursor => {
            if state.cursor.is_stopped() && !explicit {
                return Ok(model::ActionExecutionResult::Blocked {
                    message: "Cursor ACP 已由用户停止，等待显式恢复".to_string(),
                    observed_resume_generation,
                });
            }
            if explicit {
                state.cursor.resume();
                state.review_resume.fire()?;
            }
        }
        model::EngineKind::Claude => {}
    }
    // SAFETY: the durable outbox executor is the composition root's authorized replay ingress.
    let capability = unsafe { review::engine::ReviewStartCapability::new_composition_root() };
    let outcome = review::commands::start_for_outbox(
        &capability,
        app,
        state.inner(),
        action.project_id(),
        pr_number,
        &invocation,
        // AB#1204: the outbox ROW id is the dedup key — a crash-replay of this row resolves its
        // prior review's claim instead of starting a duplicate.
        action.id(),
    )
    .await?;
    if !explicit && review::commands::should_record_dispatch_ledger(outcome.clone()) {
        pr::ledger::record_dispatched(
            app,
            action.project_id(),
            &[candidate.expect("automatic candidate")],
        )?;
    }
    Ok(model::ActionExecutionResult::review(outcome.thread_id()))
}

/// Execute an AB#1069 `stop-review` outbox action (the composition root's executor arm body):
/// deserialize the `(pr, skill_key)` payload and interrupt the matching in-flight session
/// ([`review::commands::stop_for_outbox`], keyed by the ROW's `project_id` + payload `(pr, skill_key)`).
/// IDEMPOTENT — no live session is a benign `Ok(())`, so an at-least-once replay (or a stop fired
/// after the review already self-completed) never dead-letters; a bare reservation retries (F4).
/// Routing key is `action.project_id` (the row's single source, AB#1069 F3), not a payload copy.
/// A deser `Err` propagates (retry / dead-letter).
async fn run_stop_action(
    app: &tauri::AppHandle,
    action: &outbox::OutboxAction,
) -> error::AppResult<()> {
    let payload: model::StopReviewActionPayload =
        serde_json::from_str(action.payload()).map_err(|e| {
            error::AppError::new(format!("outbox stop-review action 反序列化失败：{e}"))
        })?;
    let state = app.state::<AppState>();
    review::commands::stop_for_outbox(
        app,
        state.inner(),
        action.project_id(),
        payload.pr_number,
        payload.skill_key.as_str(),
    )
    .await
}

/// Composition-root assembly for one auto-trigger cycle: validate config, snapshot active
/// `(pr, skill_key)` pairs, and inject durable inbox/outbox writers + UI error reporting into the
/// durable discovery producer. The executor is the only place that starts review
/// work; this path only materializes replayable actions.
async fn ingest_discovered_events<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project_id: String,
    events: Vec<DiscoveredEvent>,
) -> error::AppResult<()> {
    if events.is_empty() {
        return Ok(());
    }

    let state = app.state::<AppState>();
    let db = app.state::<db::Database>();
    let mut first_error = None;
    for discovered in events {
        let event =
            match normalize_discovered_event(&project_id, &discovered, inbox::store::now_epoch()) {
                Ok(event) => event,
                Err(error) => {
                    emit_dispatch_error(&app, &project_id, error.message.clone());
                    first_error.get_or_insert(error);
                    continue;
                }
            };
        let inserted = match inbox::store::insert_dedup(
            db.inner(),
            &event,
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
                first_error.get_or_insert(e);
                continue;
            }
        };
        let Some(inbox_id) = inserted else {
            continue;
        };
        inbox::service::emit_for_id(&app, db.inner(), &project_id, inbox_id);
        state.inbox.wake();
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn normalize_discovered_event(
    project_id: &str,
    discovered: &DiscoveredEvent,
    received_at_epoch: u64,
) -> error::AppResult<EventEnvelope> {
    let observation = discovered
        .event
        .as_observation()
        .ok_or_else(|| error::AppError::new("poll source produced a non-observation event"))?;
    let mut subject = observation.subject.clone();
    subject.labels.sort();
    subject.labels.dedup();
    let snapshot = serde_json::json!({
        "projectId": project_id,
        "source": discovered.event.source(),
        "repo": discovered.event.repo(),
        "eventType": observation.event_type,
        "subject": &subject,
        "candidate": &discovered.candidate,
    });
    use sha2::Digest;
    let digest = sha2::Sha256::digest(
        serde_json::to_vec(&snapshot)
            .map_err(|error| error::AppError::new(format!("poll snapshot 序列化失败：{error}")))?,
    );
    EventEnvelope::observation(
        model::InboxDedupeKey::new(format!("poll:sha256:{}", hex::encode(digest)))
            .map_err(error::AppError::new)?,
        discovered.event.source(),
        project_id,
        discovered.event.repo(),
        observation.event_type,
        subject,
        received_at_epoch,
    )
    .map_err(error::AppError::new)
}

fn process_rule_event<R: Runtime>(
    app: &tauri::AppHandle<R>,
    inbox_event_id: i64,
    event: EventEnvelope,
    candidate: Option<Candidate>,
) -> error::AppResult<()> {
    let cfg = config::service::load(app)?;
    let plans = rule::service::plan_event(&cfg.rules, &cfg.projects, &event, candidate.as_ref());
    let db = app.state::<db::Database>();
    let now = rule::store::now_epoch();
    let announced_action_ids = db.inner().with_tx(|tx| {
        let mut all_action_ids = Vec::new();
        for plan in &plans {
            if !plan.errors.is_empty() {
                return Err(error::AppError::new(format!(
                    "规则动作规划失败：{}",
                    plan.errors.join("; ")
                )));
            }
            let mut action_ids = Vec::new();
            for action in &plan.actions {
                match &action.dispatch {
                    rule::service::RuleActionDispatch::Outbox { payload } => {
                        let producer_key = model::OutboxProducerKey::for_rule_action(
                            model::InboxEventId::new(inbox_event_id)
                                .map_err(error::AppError::new)?,
                            &plan.rule_id,
                            &action.action_id,
                        )
                        .map_err(error::AppError::new)?;
                        let row = outbox::store::EnqueueInput {
                            project_id: &plan.project_id,
                            kind: action.kind,
                            summary: &action.summary,
                            payload,
                            dedupe_key: Some(&action.dedupe_key),
                            next_attempt_at: Some(now.saturating_add(action.delay_secs)),
                        };
                        action_ids.push(outbox::store::enqueue_in_tx_with_producer(
                            tx,
                            &row,
                            &producer_key,
                            now,
                        )?);
                    }
                    rule::service::RuleActionDispatch::Notification { request } => {
                        action_ids.extend(notification::enqueue_notification_in_tx(
                            tx,
                            &cfg,
                            request.clone(),
                            Some(&action.dedupe_key),
                            action.delay_secs,
                            now,
                        )?);
                    }
                    rule::service::RuleActionDispatch::Messaging { request } => {
                        action_ids.push(messaging_outbox::enqueue_send_once_after_in_tx(
                            tx,
                            &cfg,
                            request.clone(),
                            action.delay_secs,
                            now,
                        )?);
                    }
                }
            }
            rule::store::insert_match_in_tx(
                tx,
                rule::store::NewRuleMatch {
                    rule_id: &plan.rule_id,
                    rule_name: &plan.rule_name,
                    inbox_event_id,
                    project_id: &plan.project_id,
                    action_ids: &action_ids,
                    error: None,
                    now,
                },
            )?;
            all_action_ids.extend(action_ids);
        }
        inbox::store::mark_processed_in_tx(tx, inbox_event_id, now)?;
        Ok(all_action_ids)
    })?;

    for id in &announced_action_ids {
        outbox::service::announce_updated(app, db.inner(), *id);
    }
    if !announced_action_ids.is_empty() {
        app.state::<AppState>().outbox.wake();
    }
    Ok(())
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
    fn remote_tunnels_need_cloudflared_for_quick_or_bare_command() {
        use config::model::{RemoteAccessConfig, RemoteTunnel, RemoteTunnelMode};

        let mut remote = RemoteAccessConfig {
            entrypoints: Vec::new(),
            tunnels: vec![RemoteTunnel {
                enabled: true,
                mode: RemoteTunnelMode::Command,
                command: "cloudflared tunnel run --token x".to_string(),
                ..RemoteTunnel::default()
            }],
        };
        assert!(remote_tunnels_need_cloudflared(&remote));

        remote.tunnels[0].command = "/opt/homebrew/bin/cloudflared tunnel run".to_string();
        assert!(!remote_tunnels_need_cloudflared(&remote));

        remote.tunnels[0].mode = RemoteTunnelMode::Quick;
        remote.tunnels[0].command.clear();
        assert!(remote_tunnels_need_cloudflared(&remote));

        remote.tunnels[0].enabled = false;
        assert!(!remote_tunnels_need_cloudflared(&remote));
    }

    #[test]
    fn startup_reconcile_returns_blocked_review_to_claimable_pending_state() {
        let db = Database::open_in_memory().expect("db");
        let payload = serde_json::to_string(&model::ReviewActionPayload::Explicit {
            pr_number: 7,
            request_id: model::ExternalRequestId::parse("0123456789abcdef0123456789abcdef")
                .unwrap(),
            origin: model::ExternalTriggerOrigin::Http,
            invocation: model::SkillInvocation::build(
                "pr-review",
                Some("/tmp/skill.md".into()),
                "/pr-review 7",
                "",
            ),
        })
        .expect("payload");
        let id = outbox::store::enqueue(
            &db,
            "p1",
            model::ActionKind::RunSkill,
            "review",
            &payload,
            1,
        )
        .expect("enqueue");
        outbox::store::mark_blocked(&db, id, "stopped", 2).expect("block");
        assert!(outbox::store::claim_due(&db, 3)
            .expect("claim")
            .0
            .is_empty());

        assert_eq!(
            reconcile_blocked_reviews_after_restart(&db).expect("reconcile"),
            1
        );
        let claimed = outbox::store::claim_due(&db, i64::MAX as u64)
            .expect("claim")
            .0;
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id(), id);
    }

    fn discovered_snapshot() -> DiscoveredEvent {
        let candidate = Candidate {
            number: 7,
            head_sha: "same-head".to_string(),
            head_ref: "feature/x".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            skill_key: crate::model::SkillInvocation::skill_key("pr-review", ""),
        };
        DiscoveredEvent {
            event: EventEnvelope::observation(
                model::InboxDedupeKey::new("source-seed").unwrap(),
                model::SourceKind::Github,
                "discovery",
                "owner/repo",
                model::EventType::PullRequest,
                model::EventSubject {
                    number: Some(7),
                    title: "Title".to_string(),
                    body: "Body".to_string(),
                    labels: vec!["b".to_string(), "a".to_string()],
                    url: "https://example.com/7".to_string(),
                },
                0,
            )
            .unwrap(),
            candidate,
            conflict: false,
        }
    }

    #[test]
    fn poll_snapshot_hash_is_canonical_and_semantic() {
        let base = discovered_snapshot();
        let first = normalize_discovered_event("p1", &base, 1).unwrap();
        let later = normalize_discovered_event("p1", &base, 999).unwrap();
        assert_eq!(
            first.dedupe_key(),
            later.dedupe_key(),
            "receipt time is excluded"
        );

        let mut reordered = base.clone();
        if let Some(observation) = reordered.event.as_observation() {
            let mut subject = observation.subject.clone();
            let event_type = observation.event_type;
            let source = reordered.event.source();
            let repo = reordered.event.repo().to_string();
            subject.labels.reverse();
            reordered.event = EventEnvelope::observation(
                model::InboxDedupeKey::new("other-seed").unwrap(),
                source,
                "discovery",
                repo,
                event_type,
                subject,
                42,
            )
            .unwrap();
        }
        assert_eq!(
            first.dedupe_key(),
            normalize_discovered_event("p1", &reordered, 2)
                .unwrap()
                .dedupe_key()
        );

        for variant in 0..4 {
            let mut changed = base.clone();
            let observation = changed.event.as_observation().unwrap();
            let mut subject = observation.subject.clone();
            let event_type = observation.event_type;
            let source = changed.event.source();
            let repo = changed.event.repo().to_string();
            match variant {
                0 => subject.title.push('!'),
                1 => subject.body.push('!'),
                2 => subject.labels.push("new".to_string()),
                _ => {
                    changed.candidate.skill_key =
                        model::SkillInvocation::skill_key("pr-review", "--check")
                }
            }
            changed.event = EventEnvelope::observation(
                model::InboxDedupeKey::new("changed-seed").unwrap(),
                source,
                "discovery",
                repo,
                event_type,
                subject,
                0,
            )
            .unwrap();
            assert_ne!(
                first.dedupe_key(),
                normalize_discovered_event("p1", &changed, 3)
                    .unwrap()
                    .dedupe_key()
            );
        }
    }

    #[test]
    fn external_request_id_is_bound_to_request_semantics() {
        let make = |pr_number, extra_args: &str, received_at| {
            model::EventEnvelope::review_request(
                model::InboxDedupeKey::new("external:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap(),
                model::SourceKind::Github,
                "p1",
                "owner/repo",
                pr_number,
                model::DEFAULT_SKILL_NAME,
                extra_args,
                model::DEFAULT_SKILL_PATH,
                model::DEFAULT_COMMAND_TEMPLATE,
                model::ExternalRequestId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap(),
                model::ExternalTriggerOrigin::Http,
                true,
                received_at,
            )
            .unwrap()
        };
        let first = make(7, "", 1);
        let retry = make(7, "", 2);
        assert!(ensure_same_external_review_request(&first, &retry).is_ok());

        let different_pr = make(8, "", 3);
        assert!(ensure_same_external_review_request(&first, &different_pr).is_err());
        let different_kind = make(7, "--check", 4);
        assert!(ensure_same_external_review_request(&first, &different_kind).is_err());
    }

    fn rule_event() -> model::EventEnvelope {
        model::EventEnvelope::observation(
            model::InboxDedupeKey::new("delivery-1").unwrap(),
            model::SourceKind::Github,
            "p1",
            "owner/repo",
            model::EventType::PullRequest,
            model::EventSubject {
                number: Some(7),
                title: "Ready to ship".to_string(),
                body: String::new(),
                labels: vec!["ready".to_string()],
                url: "https://example.com/pull/7".to_string(),
            },
            10,
        )
        .unwrap()
    }

    #[test]
    fn process_rule_event_enqueues_actions_and_links_trace() {
        let app = tauri::test::mock_app();
        let db = Database::open_in_memory().expect("open db");
        let config = config::model::AppConfig {
            projects: vec![config::model::Project {
                id: "p1".to_string(),
                repo: "owner/repo".to_string(),
                repo_root: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .expect("src-tauri parent")
                    .display()
                    .to_string(),
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
                    config::model::RuleActionConfig::run_skill("review"),
                    config::model::RuleActionConfig::run_skill_check("check"),
                    config::model::RuleActionConfig::notify("notify"),
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
        let event = rule_event();
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
            skill_key: crate::model::SkillInvocation::skill_key("pr-review", ""),
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
        assert_eq!(kinds, vec!["runSkill", "runSkill", "notification"]);

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
                repo_root: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .expect("src-tauri parent")
                    .display()
                    .to_string(),
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
                actions: vec![config::model::RuleActionConfig::run_skill("review")],
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
        let event = rule_event();
        let inbox_id = inbox::store::insert_dedup(&db, &event, "raw", None, None)
            .expect("insert inbox")
            .expect("new inbox");
        app.manage(db);
        app.manage(AppState::default());

        let err = process_rule_event(app.handle(), inbox_id, event, None).unwrap_err();

        assert!(err.message.contains("需要 PR candidate"), "{}", err.message);
        let db = app.state::<Database>();
        let trace = rule::store::list_by_inbox(db.inner(), inbox_id).expect("trace");
        assert!(
            trace.is_empty(),
            "failed planning rolls back trace and actions"
        );
    }

    #[test]
    fn process_rule_event_final_status_failure_rolls_back_every_prior_write() {
        let app = tauri::test::mock_app();
        let db = Database::open_in_memory().expect("open db");
        let config = config::model::AppConfig {
            projects: vec![config::model::Project {
                id: "p1".to_string(),
                repo: "owner/repo".to_string(),
                repo_root: std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .expect("src-tauri parent")
                    .display()
                    .to_string(),
                enabled: false,
                ..config::model::Project::default()
            }],
            active_project_id: "p1".to_string(),
            rules: vec![config::model::RuleConfig {
                id: "r1".to_string(),
                name: "Atomic".to_string(),
                enabled: true,
                event_type: Some(model::EventType::PullRequest),
                project_id: "p1".to_string(),
                labels_any: vec!["ready".to_string()],
                actions: vec![config::model::RuleActionConfig::run_skill("review")],
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
        let event = rule_event();
        let inbox_id = inbox::store::insert_dedup(&db, &event, "raw", None, None)
            .expect("insert inbox")
            .expect("new inbox");
        db.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER fail_processed_cutpoint \
                 BEFORE UPDATE OF status ON inbox_event \
                 WHEN NEW.status='processed' \
                 BEGIN SELECT RAISE(ABORT, 'forced final cutpoint'); END;",
            )
        })
        .expect("install cutpoint");
        app.manage(db);
        app.manage(AppState::default());
        let candidate = Candidate {
            number: 7,
            head_sha: "sha".to_string(),
            head_ref: "feature/atomic".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            skill_key: crate::model::SkillInvocation::skill_key("pr-review", ""),
        };

        let error = process_rule_event(app.handle(), inbox_id, event, Some(candidate))
            .expect_err("final status cutpoint aborts transaction");
        assert!(error.message.contains("forced final cutpoint"));

        let db = app.state::<Database>();
        let counts: (i64, i64, i64) = db
            .inner()
            .with_conn(|conn| {
                Ok((
                    conn.query_row("SELECT COUNT(*) FROM rule_match", [], |row| row.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM rule_match_action", [], |row| {
                        row.get(0)
                    })?,
                    conn.query_row("SELECT COUNT(*) FROM action_outbox", [], |row| row.get(0))?,
                ))
            })
            .expect("read rollback counts");
        assert_eq!(counts, (0, 0, 0), "trace and actions roll back together");
        let inbox = inbox::store::get_entry(db.inner(), inbox_id)
            .expect("get inbox")
            .expect("inbox exists");
        assert_eq!(inbox.status, model::InboxStatus::Received);
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
                "url": "https://x/7", "skillKey": "review", "skipReason": null,
                "firstSeenEpoch": 1, "lastSeenEpoch": 2, "archived": false
            }]),
        )];
        let review_key = model::SkillInvocation::skill_key("pr-review", "");
        let action_key = format!("7@sha:{review_key}");
        let dispatched = vec![(
            "default".to_string(),
            serde_json::json!([action_key.clone()]),
        )];
        let events = vec![(
            "default".to_string(),
            serde_json::json!([{
                "pr": 7, "skillKey": "review", "headSha": "sha",
                "key": action_key, "dispatchedAtEpoch": 100
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
        assert!(ledger.has_dispatched(&format!(
            "7@sha:{}",
            model::SkillInvocation::skill_key("pr-review", "")
        )));
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
