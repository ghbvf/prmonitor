//! Deeplink trigger transport (AB#1045): the third entry into the [`commands::trigger_review`]
//! funnel, beside the Tauri command (`start_review`/`trigger_review`), the local REST API
//! ([`super::local_api`]), and the CLI ([`crate::cli`]).
//!
//! A human / browser opens `prmonitor://review?pr=N&repo=R&kind=review` (Slack/Zoom-style
//! `app://<action>?key=value`). `tauri-plugin-deep-link` surfaces it through `on_open_url`
//! (wired in `lib.rs`); this module parses + validates the URL and hands `(reference, pr, kind)`
//! to the SAME funnel the other transports use — so engine selection + dedup stay single-source
//! ([`commands::dispatch_engine`]'s `Hard` carriers, never re-implemented here).
//!
//! Fire-and-forget: the OS returns no result/exit code to the opener, so completion is surfaced
//! out-of-band — a desktop notification carrying the pr-review comment URL + the window pulled to
//! the foreground (GitButler's `on_open_url` show/focus pattern).
//!
//! ## Governance
//! - **Deeplink param anti-forgery = Medium.** [`parse_review_deeplink`] is pure and
//!   table-unit-tested below; it reuses [`commands::validate_kind`] / [`commands::validate_pr_number`]
//!   so a deeplink hits the EXACT fail-closed boundary the CLI/local-API hit (no second whitelist
//!   to drift). The scheme is registered statically (Info.plist on macOS) in release; runtime
//!   registration (`register_all`) is gated to debug builds in `lib.rs`, so a production build
//!   only ever honors the OS-registered scheme — a forged argv can't smuggle a different one.
//! - **Scheme config consistency = Medium.** [`tests::scheme_matches_tauri_conf`] locks the
//!   `SCHEME` const the parser enforces to `tauri.conf.json`'s registered scheme (mirrors
//!   `cli.rs::app_identifier_matches_tauri_conf`), so a drift that would silently break every
//!   deeplink fails CI instead.

use tauri::{AppHandle, Manager, Runtime};
use url::Url;

use crate::error::{AppError, AppResult};
use crate::model::{Notification, NotificationKind, NotificationLevel, RedactedNotificationBody};
use crate::review::commands::{self, validate_kind, validate_pr_number};
use crate::review::notify;
use crate::review::session::CompletionOutcome;
use crate::state::AppState;

/// The deeplink URL scheme. Single source for both the parser's scheme check and the golden that
/// ties it to `tauri.conf.json`'s registered scheme ([`tests::scheme_matches_tauri_conf`]).
pub(crate) const SCHEME: &str = "prmonitor";

/// The only supported action (the URL's host slot: `prmonitor://review?…`). A second action would
/// be added here + routed in [`parse_review_deeplink`]; an unknown action is rejected.
const ACTION_REVIEW: &str = "review";

/// Default `kind` when the deeplink omits `?kind=` (parity with the CLI, where the absence of
/// `--check` means a review). Kept distinct from [`ACTION_REVIEW`]: they coincide as `"review"`
/// today, but the URL action and the review-turn kind are separate concepts.
const DEFAULT_KIND: &str = "review";

/// A validated deeplink trigger: the funnel inputs ([`commands::trigger_review`] takes the same
/// `(reference, pr_number, kind)`). `reference` is a project `id` OR a `repo` (resolved downstream
/// by `project_by_ref_validated`); exactly one of `repo` / `projectId` produced it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParsedTrigger {
    pub(crate) reference: String,
    pub(crate) pr_number: u64,
    pub(crate) kind: String,
}

/// Parse + validate a `prmonitor://review?pr=N&repo=R&kind=review` deeplink into the funnel inputs.
///
/// Rejects (never panics) anything that isn't a well-formed review trigger: wrong scheme, wrong
/// action, missing/non-numeric/zero `pr`, an unknown `kind`, or a reference that isn't EXACTLY one
/// of `repo` / `projectId` (the same "exactly one" rule `local_api::resolve_reference` enforces).
/// `kind` defaults to `"review"` when absent (parity with the CLI, where the absence of `--check`
/// means a review). Unknown query keys are ignored (forward-compat) — only the validated fields
/// are the contract.
pub(crate) fn parse_review_deeplink(url: &Url) -> AppResult<ParsedTrigger> {
    // `Url` ASCII-lowercases the scheme (WHATWG URL), so an OS that hands us `PRMONITOR://…`
    // still matches the lowercase `SCHEME` (locked by `tests::accepts_uppercase_scheme_*`).
    if url.scheme() != SCHEME {
        return Err(AppError::new(format!(
            "deeplink scheme 非法（期望 {SCHEME}://）: {:?}",
            url.scheme()
        )));
    }
    // The action lives in the authority/host slot: `prmonitor://review?…`.
    if url.host_str() != Some(ACTION_REVIEW) {
        return Err(AppError::new(format!(
            "deeplink action 非法（仅支持 {ACTION_REVIEW}）: {:?}",
            url.host_str()
        )));
    }

    let mut pr_raw: Option<String> = None;
    let mut repo: Option<String> = None;
    let mut project_id: Option<String> = None;
    let mut kind: Option<String> = None;
    for (key, value) in url.query_pairs() {
        let slot = match key.as_ref() {
            "pr" => &mut pr_raw,
            "repo" => &mut repo,
            "projectId" => &mut project_id,
            "kind" => &mut kind,
            // Ignore unknown params: the validated fields below are the contract, and tolerating
            // extras keeps a future `?foo=` from hard-failing existing links.
            _ => continue,
        };
        // Fail closed on a REPEATED key (codex F4): with `?pr=1&pr=2` a preview could show one
        // value while the parser triggers the other — never silently last-wins on external input.
        if slot.is_some() {
            return Err(AppError::new(format!("deeplink 重复参数: {key}")));
        }
        *slot = Some(value.into_owned());
    }

    // `pr`: required, must parse as u64, then the SAME `> 0` fail-closed check the funnel applies
    // (so `pr=0` is rejected here, not handed to the engine as `/pr-review 0`).
    let pr_str = pr_raw.ok_or_else(|| AppError::new("deeplink 缺少 pr 参数"))?;
    let pr_number: u64 = pr_str
        .parse()
        .map_err(|_| AppError::new(format!("deeplink pr 参数非法（需正整数）: {pr_str:?}")))?;
    validate_pr_number(pr_number)?;

    // `reference`: EXACTLY one of `projectId` / `repo` (mirrors `local_api::resolve_reference`).
    let reference = match (project_id, repo) {
        (Some(id), None) => id,
        (None, Some(r)) => r,
        (Some(_), Some(_)) => {
            return Err(AppError::new(
                "deeplink 同时给了 projectId 与 repo（只能其一）",
            ))
        }
        (None, None) => return Err(AppError::new("deeplink 缺少 repo 或 projectId")),
    };
    // Trim like `local_api::resolve_reference` so `?repo=%20octo/app` resolves identically across
    // transports, and a whitespace-only reference fails closed HERE rather than deep in project
    // lookup. Use the trimmed value downstream (not the raw one).
    let reference = reference.trim().to_string();
    if reference.is_empty() {
        return Err(AppError::new("deeplink repo/projectId 为空"));
    }

    // `kind`: default "review"; otherwise the SAME whitelist the funnel enforces.
    let kind = kind.unwrap_or_else(|| DEFAULT_KIND.to_string());
    validate_kind(&kind)?;

    Ok(ParsedTrigger {
        reference,
        pr_number,
        kind,
    })
}

/// Handle a batch of opened deeplink URLs (the `on_open_url` payload). Pulls the window forward
/// immediately (GitButler's show/focus on open), then triggers each URL on its OWN task so a
/// long-running review for `urls[0]` never blocks triggering `urls[1]` (multiple URLs in one
/// event is rare, but serial `.await` would stall behind a full review). Synchronous: it only
/// fans out spawns and returns, so the `on_open_url` callback calls it directly (no outer spawn).
pub(crate) fn handle_review_deeplink<R: Runtime>(app: AppHandle<R>, urls: Vec<Url>) {
    focus_main_window(&app);
    for url in urls {
        tauri::async_runtime::spawn(handle_one(app.clone(), url));
    }
}

/// Trigger one deeplink URL through the funnel, then await its terminal completion and notify.
async fn handle_one<R: Runtime>(app: AppHandle<R>, url: Url) {
    let trigger = match parse_review_deeplink(&url) {
        Ok(t) => t,
        // Malformed / forged URL: reject — no trigger, no panic (acceptance ③). Fire-and-forget
        // has no return channel, so a stderr line is the only surface. Log structured fields only
        // (scheme + action), NOT the full URL: its `repo`/`projectId` query values are
        // percent-decoded and may name private projects — kept symmetric with the trigger-failure
        // log below (pr/kind only).
        Err(e) => {
            eprintln!(
                "deeplink 拒绝（scheme={} action={:?}）: {}",
                url.scheme(),
                url.host_str(),
                e.message
            );
            // Redacted FIXED text — never `e.message` (it echoes the raw `pr`/`kind` input). The
            // detail stays in the stderr log above; the notification center is an exposed/persisted
            // sink (codex --check 回归).
            notify_failure(
                &app,
                "prmonitor deeplink 无效",
                RedactedNotificationBody::fixed("链接格式或参数无效，未触发 review"),
            )
            .await;
            return;
        }
    };
    let ParsedTrigger {
        reference,
        pr_number,
        kind,
    } = trigger;

    // Reuse the transport-agnostic funnel EXACTLY as `local_api::handle_create` does: take the
    // managed `State` from the owned handle and pass a cloned handle alongside it (same lifetime
    // shape `trigger_review` itself uses; the State borrow lives across the await).
    let state = app.state::<AppState>();
    let session_id = match commands::trigger_review(
        app.clone(),
        state,
        reference,
        pr_number,
        kind.clone(),
    )
    .await
    {
        Ok(id) => id,
        // Dedup ("already in flight") / unknown project / validation: log the full reason, then a
        // REDACTED user toast. `e.message` from project resolution embeds the external `reference`
        // (`match_project_ref`: "找不到项目…: {reference}" / "repo 不唯一…: {reference}") — it must
        // NOT reach the notification center (codex --check 回归). `pr_number` is safe to show.
        Err(e) => {
            eprintln!(
                "deeplink 触发失败（pr={pr_number} kind={kind}）: {}",
                e.message
            );
            notify_failure(
                &app,
                "prmonitor review 未触发",
                RedactedNotificationBody::review_trigger_rejected(pr_number),
            )
            .await;
            return;
        }
    };

    // Subscribe AFTER trigger: `subscribe_completion` is get-or-create and RETAINS the last value,
    // so even if the turn finalized between trigger and here, the receiver reads the retained
    // `Some(outcome)` — there is no "subscribed too late" race (proven by
    // `session.rs::tests::signal_completion_carries_each_terminal_status` + the get-or-create doc).
    // This task owns NO subprocess (the engine's child is tracked by codex/claude `shutdown` on
    // `RunEvent::Exit`); on app exit the registry's watch senders drop, `rx.changed()` errors, and
    // the loop returns — so it needs no explicit abort registration.
    let mut rx = app
        .state::<AppState>()
        .sessions
        .subscribe_completion(&session_id);
    let outcome = loop {
        if let Some(o) = rx.borrow_and_update().clone() {
            break o;
        }
        // Sender dropped without ever signalling (registry gone / app shutting down): give up.
        if rx.changed().await.is_err() {
            return;
        }
    };

    notify_completion(&app, pr_number, &outcome).await;
    focus_main_window(&app);
}

/// Send the fire-and-forget completion notification. `wire_status` (not the `Done`/`Failed`
/// collapse) distinguishes completed vs interrupted vs failed; the body carries the pr-review
/// comment URL (the actionable artifact) when one was resolved.
async fn notify_completion<R: Runtime>(
    app: &AppHandle<R>,
    pr_number: u64,
    outcome: &CompletionOutcome,
) {
    // Whitelist the known terminal statuses; never reflect codex's raw `wire_status` (it comes from
    // the codex subprocess) into the notification title. An unexpected value gets a fixed label +
    // a diagnostic log rather than surfacing arbitrary content.
    let (status_label, no_link_body) = match outcome.wire_status.as_str() {
        "completed" => ("完成", "本次 review 完成（无评论链接）"),
        "interrupted" => ("已中断", "本次 review 已中断（无评论链接）"),
        "failed" => ("失败", "本次 review 失败（无评论链接）"),
        other => {
            eprintln!("deeplink 通知：未知 wire_status {other:?}");
            ("结束", "本次 review 结束（无评论链接）")
        }
    };
    // Normalized AB#1070 payload through the `NotificationProvider` seam. `body` keeps the prior
    // displayed text (comment URL when present, else the no-link fallback); `url` carries the same
    // comment URL so the seam knows the actionable artifact (the desktop notifier folds them so the
    // shown text is unchanged).
    let body = match outcome.comment_url.clone() {
        Some(url) => RedactedNotificationBody::action_url(url),
        None => RedactedNotificationBody::fixed(no_link_body),
    };
    let note = Notification::new(
        NotificationLevel::Info,
        format!("PR #{pr_number} review {status_label}"),
        outcome.comment_url.clone().unwrap_or_default(),
        body,
        String::new(),
    );

    if let Err(e) = notify::deliver(app, NotificationKind::Desktop, &note).await {
        eprintln!("deeplink 通知发送失败: {e}");
    }
}

/// Surface a deeplink FAILURE to the user (codex F3). A deeplink is fire-and-forget with no return
/// channel, so a clicked link that can't run would otherwise be silent (only stderr). Best-effort,
/// like [`notify_completion`].
///
/// `body` is a [`RedactedNotificationBody`], so callers cannot pass an arbitrary
/// `AppError::message` directly. Project-resolution errors embed the external `reference`
/// (repo/projectId), so the full reason stays in stderr and only a typed safe body reaches
/// the notification center.
async fn notify_failure<R: Runtime>(
    app: &AppHandle<R>,
    title: &str,
    body: RedactedNotificationBody,
) {
    let note = Notification::new(
        NotificationLevel::Warning,
        title.to_string(),
        String::new(),
        body,
        String::new(),
    );
    if let Err(e) = notify::deliver(app, NotificationKind::Desktop, &note).await {
        eprintln!("deeplink 失败通知发送失败: {e}");
    }
}

/// Resurface the main window (unminimize → show → focus). Mirrors the single-instance handler's
/// inline window resurface in `lib.rs`; best-effort (each step is ignored on error).
fn focus_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> AppResult<ParsedTrigger> {
        let url = Url::parse(s).unwrap_or_else(|e| panic!("test url {s:?} is malformed: {e}"));
        parse_review_deeplink(&url)
    }

    #[test]
    fn parses_repo_pr_and_explicit_kind() {
        let p = parse("prmonitor://review?pr=42&repo=octo/app&kind=check").expect("ok");
        assert_eq!(
            p,
            ParsedTrigger {
                reference: "octo/app".to_string(),
                pr_number: 42,
                kind: "check".to_string(),
            }
        );
    }

    #[test]
    fn kind_defaults_to_review_when_absent() {
        let p = parse("prmonitor://review?pr=7&repo=octo/app").expect("ok");
        assert_eq!(p.kind, "review");
    }

    #[test]
    fn rejects_duplicate_pr_key() {
        // External input must fail closed on a repeated key (codex F4): a link with two `pr`
        // values could show one in a preview and trigger the other. Reject, don't last-wins.
        assert!(parse("prmonitor://review?pr=1&repo=octo/app&pr=2").is_err());
    }

    #[test]
    fn rejects_duplicate_repo_key() {
        assert!(parse("prmonitor://review?pr=7&repo=octo/app&repo=evil/app").is_err());
    }

    #[test]
    fn trims_surrounding_whitespace_in_reference() {
        // `%20…%20` decodes to " octo/app " → trimmed to "octo/app" (parity with local_api).
        let p = parse("prmonitor://review?pr=7&repo=%20octo/app%20").expect("ok");
        assert_eq!(p.reference, "octo/app");
    }

    #[test]
    fn rejects_whitespace_only_reference() {
        // `?repo=%20%20` decodes to "  " → trimmed empty → rejected (not handed to project lookup).
        assert!(parse("prmonitor://review?pr=7&repo=%20%20").is_err());
    }

    #[test]
    fn accepts_uppercase_scheme_normalized_by_url_crate() {
        // The `url` crate ASCII-lowercases the scheme (WHATWG URL), so an OS that hands us
        // `PRMONITOR://…` still matches `SCHEME`. Locks that assumption (see parser comment).
        let p = parse("PRMONITOR://review?pr=7&repo=octo/app").expect("ok");
        assert_eq!(p.pr_number, 7);
    }

    #[test]
    fn parses_project_id_reference() {
        let p = parse("prmonitor://review?pr=7&projectId=proj-123").expect("ok");
        assert_eq!(p.reference, "proj-123");
        assert_eq!(p.pr_number, 7);
    }

    #[test]
    fn ignores_unknown_query_params() {
        let p = parse("prmonitor://review?pr=7&repo=octo/app&foo=bar").expect("ok");
        assert_eq!(p.reference, "octo/app");
    }

    #[test]
    fn rejects_missing_pr() {
        assert!(parse("prmonitor://review?repo=octo/app").is_err());
    }

    #[test]
    fn rejects_non_numeric_pr() {
        assert!(parse("prmonitor://review?pr=abc&repo=octo/app").is_err());
    }

    #[test]
    fn rejects_pr_zero() {
        // Same fail-closed boundary as `validate_pr_number` (PR/MR numbers are 1-based).
        assert!(parse("prmonitor://review?pr=0&repo=octo/app").is_err());
    }

    #[test]
    fn rejects_unknown_kind() {
        assert!(parse("prmonitor://review?pr=7&repo=octo/app&kind=delete").is_err());
    }

    #[test]
    fn rejects_both_repo_and_project_id() {
        assert!(parse("prmonitor://review?pr=7&repo=octo/app&projectId=proj-1").is_err());
    }

    #[test]
    fn rejects_neither_repo_nor_project_id() {
        assert!(parse("prmonitor://review?pr=7").is_err());
    }

    #[test]
    fn rejects_empty_reference() {
        assert!(parse("prmonitor://review?pr=7&repo=").is_err());
    }

    #[test]
    fn rejects_wrong_scheme() {
        assert!(parse("https://review?pr=7&repo=octo/app").is_err());
    }

    #[test]
    fn rejects_wrong_action() {
        assert!(parse("prmonitor://deploy?pr=7&repo=octo/app").is_err());
    }

    /// **Medium** carrier: the scheme the parser enforces (`SCHEME`) MUST equal the scheme
    /// registered in `tauri.conf.json` `plugins.deep-link.desktop.schemes`, or a `prmonitor://`
    /// link the OS routes here never matches the parser (silent breakage). Mirrors
    /// `cli.rs::app_identifier_matches_tauri_conf`, which locks the bundle identifier the same way.
    #[test]
    fn scheme_matches_tauri_conf() {
        let conf = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tauri.conf.json"))
            .expect("read tauri.conf.json");
        let v: serde_json::Value = serde_json::from_str(&conf).expect("parse tauri.conf.json");
        let schemes = v["plugins"]["deep-link"]["desktop"]["schemes"]
            .as_array()
            .expect("deep-link.desktop.schemes is an array");
        assert_eq!(
            schemes,
            &vec![serde_json::Value::String(SCHEME.to_string())],
            "tauri.conf.json deep-link schemes must equal the parser's SCHEME const"
        );
    }
}
