//! Deeplink external-review transport (AB#1045), alongside the local REST API and CLI.
//!
//! A human / browser opens `prmonitor://review?pr=N&repo=R&kind=review&requestId=<32-lower-hex>` (Slack/Zoom-style
//! `app://<action>?key=value`). `tauri-plugin-deep-link` surfaces it through `on_open_url`
//! (wired in `lib.rs`); this module parses + validates the URL and hands `(reference, pr, kind)`
//! to the SAME funnel the other transports use — so engine selection + dedup stay single-source
//! (the durable receipt path's `Hard` carriers, never re-implemented here).
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

use std::collections::VecDeque;
use std::sync::{Mutex as StdMutex, OnceLock};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager, Runtime};
use url::Url;

use crate::config::service as config_service;
use crate::error::{AppError, AppResult};
use crate::model::{
    ExternalRequestId, ExternalTriggerOrigin, Notification, NotificationKind, NotificationLevel,
    RedactedNotificationBody, ReviewKind, SendNotificationRequest,
};
use crate::review::notify;
use crate::state::AppState;

/// The deeplink URL scheme. Single source for both the parser's scheme check and the golden that
/// ties it to `tauri.conf.json`'s registered scheme ([`tests::scheme_matches_tauri_conf`]).
pub(crate) const SCHEME: &str = "prmonitor";

/// The only supported action (the URL's host slot: `prmonitor://review?…`). A second action would
/// be added here + routed in [`parse_review_deeplink`]; an unknown action is rejected.
const ACTION_REVIEW: &str = "review";
const ACTION_NOTIFY: &str = "notify";
const MAX_REVIEW_REFERENCE_CHARS: usize = 256;

/// Default `kind` when the deeplink omits `?kind=` (parity with the CLI, where the absence of
/// `--check` means a review). Kept distinct from [`ACTION_REVIEW`]: they coincide as `"review"`
/// today, but the URL action and the review-turn kind are separate concepts.
const DEFAULT_KIND: &str = "review";

/// A validated deeplink request. `reference` is a project `id` OR a `repo`; exactly one of
/// `repo` / `projectId` produced it, and `request_id` is the mandatory durable idempotency key.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParsedReviewRequest {
    pub(crate) reference: String,
    pub(crate) pr_number: u64,
    pub(crate) kind: ReviewKind,
    pub(crate) request_id: ExternalRequestId,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParsedNotify {
    pub(crate) request: SendNotificationRequest,
    pub(crate) signature: String,
}

type HmacSha256 = Hmac<Sha256>;
const NOTIFY_DEEPLINK_DEDUPE_CAP: usize = 256;
static SEEN_NOTIFY_DEEPLINK_KEYS: OnceLock<StdMutex<VecDeque<String>>> = OnceLock::new();

/// Parse + validate a
/// `prmonitor://review?pr=N&repo=R&kind=review&requestId=<32-lower-hex>` deeplink into the
/// durable receipt-request inputs.
///
/// Rejects (never panics) anything that isn't a well-formed review request: wrong scheme, wrong
/// action, missing/non-numeric/zero `pr`, an unknown `kind`, or a reference that isn't EXACTLY one
/// of `repo` / `projectId` (the same "exactly one" rule `local_api::resolve_reference` enforces).
/// `kind` defaults to `"review"` when absent (parity with the CLI, where the absence of `--check`
/// means a review). Unknown query keys are ignored (forward-compat) — only the validated fields
/// are the contract.
pub(crate) fn parse_review_deeplink(url: &Url) -> AppResult<ParsedReviewRequest> {
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
    let mut request_id: Option<String> = None;
    for (key, value) in url.query_pairs() {
        let slot = match key.as_ref() {
            "pr" => &mut pr_raw,
            "repo" => &mut repo,
            "projectId" => &mut project_id,
            "kind" => &mut kind,
            "requestId" => &mut request_id,
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
    if pr_number == 0 {
        return Err(AppError::new("deeplink pr 参数非法（需正整数）"));
    }

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
    if reference.chars().count() > MAX_REVIEW_REFERENCE_CHARS {
        return Err(AppError::new(format!(
            "deeplink repo/projectId 过长（最多 {MAX_REVIEW_REFERENCE_CHARS} 字符）"
        )));
    }

    // `kind`: default "review"; otherwise the SAME whitelist the funnel enforces.
    let kind = kind
        .unwrap_or_else(|| DEFAULT_KIND.to_string())
        .parse::<ReviewKind>()
        .map_err(AppError::new)?;
    let request_id = request_id
        .ok_or_else(|| AppError::new("deeplink 缺少 requestId 参数"))
        .and_then(|value| ExternalRequestId::parse(value).map_err(AppError::new))?;

    Ok(ParsedReviewRequest {
        reference,
        pr_number,
        kind,
        request_id,
    })
}

/// Parse + validate a `prmonitor://notify?title=...&body=...&sig=...` deeplink.
///
/// Reuses the same external-input discipline as review deeplinks: wrong scheme/action rejects,
/// duplicate recognized scalar keys reject, and unknown keys are ignored. Notification deeplinks
/// require a scoped HMAC signature rather than carrying the global local API bearer token in the
/// URL. Channel selection is expressed as either `channelId=<id>` or comma-separated
/// `channelIds=a,b`.
pub(crate) fn parse_notify_deeplink(url: &Url) -> AppResult<ParsedNotify> {
    if url.scheme() != SCHEME {
        return Err(AppError::new(format!(
            "deeplink scheme 非法（期望 {SCHEME}://）: {:?}",
            url.scheme()
        )));
    }
    if url.host_str() != Some(ACTION_NOTIFY) {
        return Err(AppError::new(format!(
            "deeplink action 非法（仅支持 {ACTION_NOTIFY}）: {:?}",
            url.host_str()
        )));
    }

    let mut title: Option<String> = None;
    let mut body: Option<String> = None;
    let mut link_url: Option<String> = None;
    let mut project_id: Option<String> = None;
    let mut level: Option<String> = None;
    let mut channel_id: Option<String> = None;
    let mut channel_ids: Option<String> = None;
    let mut signature: Option<String> = None;
    for (key, value) in url.query_pairs() {
        if key == "token" {
            return Err(AppError::new(
                "deeplink notify 不支持 token 参数，请使用 sig 签名",
            ));
        }
        let slot = match key.as_ref() {
            "title" => &mut title,
            "body" => &mut body,
            "url" => &mut link_url,
            "projectId" => &mut project_id,
            "level" => &mut level,
            "channelId" => &mut channel_id,
            "channelIds" => &mut channel_ids,
            "sig" => &mut signature,
            _ => continue,
        };
        if slot.is_some() {
            return Err(AppError::new(format!("deeplink 重复参数: {key}")));
        }
        *slot = Some(value.into_owned());
    }

    let title = title
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| AppError::new("deeplink notify 缺少 title 参数"))?;
    let body = body.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let link_url = link_url
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    if body.is_none() && link_url.is_none() {
        return Err(AppError::new("deeplink notify 缺少 body 或 url"));
    }
    let level = match level
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        None => None,
        Some(value) => Some(value.parse().map_err(AppError::new)?),
    };
    let project_id = project_id
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let channel_ids = match (channel_id, channel_ids) {
        (Some(_), Some(_)) => {
            return Err(AppError::new(
                "deeplink notify 同时给了 channelId 与 channelIds（只能其一）",
            ))
        }
        (Some(id), None) => vec![id.trim().to_string()],
        (None, Some(ids)) => ids
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect(),
        (None, None) => Vec::new(),
    };
    let signature = signature
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| AppError::new("deeplink notify 缺少 sig 参数"))?;

    Ok(ParsedNotify {
        request: SendNotificationRequest {
            level,
            title,
            body,
            url: link_url,
            project_id,
            channel_ids,
        },
        signature,
    })
}

/// Handle a batch of opened deeplink URLs (the `on_open_url` payload). Pulls the window forward
/// immediately (GitButler's show/focus on open), then processes each URL on its OWN task so a
/// long-running review for `urls[0]` never blocks submitting `urls[1]` (multiple URLs in one
/// event is rare, but serial `.await` would stall behind a full review). Synchronous: it only
/// fans out spawns and returns, so the `on_open_url` callback calls it directly (no outer spawn).
pub(crate) fn handle_review_deeplink(app: AppHandle, urls: Vec<Url>) {
    focus_main_window(&app);
    for url in urls {
        tauri::async_runtime::spawn(handle_one(app.clone(), url));
    }
}

/// Submit one deeplink URL through the receipt funnel, then await terminal completion and notify.
async fn handle_one(app: AppHandle, url: Url) {
    match url.host_str() {
        Some(ACTION_REVIEW) => handle_review_one(app, url).await,
        Some(ACTION_NOTIFY) => handle_notify_one(app, url).await,
        _ => {
            eprintln!(
                "deeplink 拒绝（scheme={} action={:?}）: unsupported action",
                url.scheme(),
                url.host_str()
            );
            notify_failure(
                &app,
                "prmonitor deeplink 无效",
                RedactedNotificationBody::fixed("链接格式或参数无效，未触发操作"),
            )
            .await;
        }
    }
}

async fn handle_notify_one<R: Runtime>(app: AppHandle<R>, url: Url) {
    let parsed = match parse_notify_deeplink(&url) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!(
                "deeplink notify 拒绝（scheme={} action={:?}）: {}",
                url.scheme(),
                url.host_str(),
                e.message
            );
            notify_failure(
                &app,
                "prmonitor notification 未入队",
                RedactedNotificationBody::fixed("通知链接格式或参数无效，未入队通知"),
            )
            .await;
            return;
        }
    };
    if let Err(e) = verify_notify_deeplink_signature(&app, &parsed.request, &parsed.signature) {
        eprintln!("deeplink notify 鉴权失败: {}", e.message);
        notify_failure(
            &app,
            "prmonitor notification 未入队",
            RedactedNotificationBody::fixed("通知链接未授权，未入队通知"),
        )
        .await;
        return;
    }
    let dedupe_prefix = match notify_deeplink_dedupe_prefix(&parsed.request) {
        Ok(prefix) => prefix,
        Err(e) => {
            eprintln!("deeplink notify 去重键生成失败: {}", e.message);
            notify_failure(
                &app,
                "prmonitor notification 未入队",
                RedactedNotificationBody::fixed("通知内容无效，未入队通知"),
            )
            .await;
            return;
        }
    };
    if notify_deeplink_key_seen(&dedupe_prefix) {
        eprintln!("deeplink notify 重复打开，已忽略");
        return;
    }
    let state = app.state::<AppState>();
    match state
        .notification_sender
        .send(parsed.request, Some(dedupe_prefix.clone()))
    {
        Ok(_) => {
            remember_notify_deeplink_key(&dedupe_prefix);
        }
        Err(e) => {
            eprintln!("deeplink notify 入队失败: {}", e.message);
            notify_failure(
                &app,
                "prmonitor notification 未入队",
                RedactedNotificationBody::fixed("通知参数无效或渠道不可用，未入队通知"),
            )
            .await;
        }
    }
}

fn notify_deeplink_mac(request: &SendNotificationRequest, secret: &str) -> AppResult<HmacSha256> {
    let secret = secret.trim();
    if secret.is_empty() {
        return Err(AppError::new("deeplink notify 签名密钥为空".to_string()));
    }
    let bytes = serde_json::to_vec(request)
        .map_err(|e| AppError::new(format!("deeplink notify 签名载荷序列化失败：{e}")))?;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| AppError::new("deeplink notify 签名密钥无效".to_string()))?;
    mac.update(&bytes);
    Ok(mac)
}

#[cfg(test)]
fn notify_deeplink_signature(request: &SendNotificationRequest, secret: &str) -> AppResult<String> {
    Ok(hex::encode(
        notify_deeplink_mac(request, secret)?
            .finalize()
            .into_bytes(),
    ))
}

fn verify_notify_deeplink_signature<R: Runtime>(
    app: &AppHandle<R>,
    request: &SendNotificationRequest,
    supplied: &str,
) -> AppResult<()> {
    let configured = config_service::load(app)?.local_api_token;
    let configured = configured.trim();
    let supplied =
        hex::decode(supplied).map_err(|_| AppError::new("deeplink notify sig 非法".to_string()))?;
    let mac = notify_deeplink_mac(request, configured)?;
    mac.verify_slice(&supplied)
        .map_err(|_| AppError::new("deeplink notify sig 无效".to_string()))
}

fn notify_deeplink_dedupe_prefix(request: &SendNotificationRequest) -> AppResult<String> {
    let bytes = serde_json::to_vec(request)
        .map_err(|e| AppError::new(format!("deeplink notify 序列化失败: {e}")))?;
    let digest = Sha256::digest(bytes);
    Ok(format!("deeplink-notify:{}", hex::encode(digest)))
}

fn remember_notify_deeplink_key(key: &str) -> bool {
    let seen = SEEN_NOTIFY_DEEPLINK_KEYS.get_or_init(|| StdMutex::new(VecDeque::new()));
    let mut seen = seen.lock().unwrap_or_else(|p| p.into_inner());
    if seen.iter().any(|existing| existing == key) {
        return false;
    }
    if seen.len() >= NOTIFY_DEEPLINK_DEDUPE_CAP {
        seen.pop_front();
    }
    seen.push_back(key.to_string());
    true
}

fn notify_deeplink_key_seen(key: &str) -> bool {
    let seen = SEEN_NOTIFY_DEEPLINK_KEYS.get_or_init(|| StdMutex::new(VecDeque::new()));
    let seen = seen.lock().unwrap_or_else(|p| p.into_inner());
    seen.iter().any(|existing| existing == key)
}

async fn handle_review_one(app: AppHandle, url: Url) {
    let request = match parse_review_deeplink(&url) {
        Ok(t) => t,
        // Malformed / forged URL: reject — no request, no panic (acceptance ③). Fire-and-forget
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
    let ParsedReviewRequest {
        reference,
        pr_number,
        kind,
        request_id,
    } = request;

    let state = app.state::<AppState>();
    if let Err(e) = state.external_review.submit(
        reference,
        pr_number,
        kind,
        request_id,
        ExternalTriggerOrigin::DeepLink,
        true,
    ) {
        eprintln!(
            "deeplink receipt 入队失败（pr={pr_number} kind={kind}）: {}",
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
    focus_main_window(&app);
}

/// Surface a deeplink FAILURE to the user (codex F3). A deeplink is fire-and-forget with no return
/// channel, so a clicked link that can't run would otherwise be silent (only stderr).
///
/// Delivers INLINE via `notify::deliver` (NOT through the outbox, unlike `notify_completion` which
/// enqueues for durable + retried delivery). This is deliberate: a failure notice is transient
/// immediate feedback on a bad click BEFORE any session exists — there is nothing durable to resume,
/// and a retried "link invalid" toast minutes later would be noise. If the inline delivery fails it
/// is logged, not retried.
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const REQUEST_ID: &str = "0123456789abcdef0123456789abcdef";

    fn parse(s: &str) -> AppResult<ParsedReviewRequest> {
        let mut url = Url::parse(s).unwrap_or_else(|e| panic!("test url {s:?} is malformed: {e}"));
        if url.host_str() == Some(ACTION_REVIEW)
            && !url.query_pairs().any(|(key, _)| key == "requestId")
        {
            url.query_pairs_mut().append_pair("requestId", REQUEST_ID);
        }
        parse_review_deeplink(&url)
    }

    fn parse_notify(s: &str) -> AppResult<ParsedNotify> {
        let url = Url::parse(s).unwrap_or_else(|e| panic!("test url {s:?} is malformed: {e}"));
        parse_notify_deeplink(&url)
    }

    fn notify_request(title: &str) -> SendNotificationRequest {
        SendNotificationRequest {
            level: Some(NotificationLevel::Warning),
            title: title.to_string(),
            body: Some("Build 42".to_string()),
            url: Some("https://example.com/build/42".to_string()),
            project_id: Some("p1".to_string()),
            channel_ids: vec!["desktop".to_string(), "slack-main".to_string()],
        }
    }

    fn signed_notify_url(request: &SendNotificationRequest, secret: &str) -> Url {
        let sig = notify_deeplink_signature(request, secret).expect("sign notify deeplink");
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("title", &request.title)
            .append_pair("body", request.body.as_deref().unwrap_or_default())
            .append_pair("url", request.url.as_deref().unwrap_or_default())
            .append_pair(
                "projectId",
                request.project_id.as_deref().unwrap_or_default(),
            )
            .append_pair("level", "warning")
            .append_pair("channelIds", &request.channel_ids.join(","))
            .append_pair("sig", &sig);
        Url::parse(&format!("prmonitor://notify?{}", query.finish())).expect("signed notify url")
    }

    #[test]
    fn parses_repo_pr_and_explicit_kind() {
        let p = parse("prmonitor://review?pr=42&repo=octo/app&kind=check").expect("ok");
        assert_eq!(
            p,
            ParsedReviewRequest {
                reference: "octo/app".to_string(),
                pr_number: 42,
                kind: ReviewKind::Check,
                request_id: ExternalRequestId::parse(REQUEST_ID).expect("request id"),
            }
        );
    }

    #[test]
    fn complete_documented_review_url_parses_without_test_helper() {
        let url = Url::parse(&format!(
            "prmonitor://review?pr=42&repo=octo/app&kind=review&requestId={REQUEST_ID}"
        ))
        .expect("url");
        let parsed = parse_review_deeplink(&url).expect("documented url");
        assert_eq!(parsed.pr_number, 42);
        assert_eq!(parsed.request_id.as_str(), REQUEST_ID);
    }

    #[test]
    fn kind_defaults_to_review_when_absent() {
        let p = parse("prmonitor://review?pr=7&repo=octo/app").expect("ok");
        assert_eq!(p.kind, ReviewKind::Review);
    }

    #[test]
    fn request_id_is_required_and_fail_closed() {
        let missing = Url::parse("prmonitor://review?pr=7&repo=octo/app").expect("url");
        assert!(parse_review_deeplink(&missing).is_err());
        assert!(parse("prmonitor://review?pr=7&repo=octo/app&requestId=ABC").is_err());
        assert!(parse(&format!(
            "prmonitor://review?pr=7&repo=octo/app&requestId={REQUEST_ID}&requestId={REQUEST_ID}"
        ))
        .is_err());
    }

    #[test]
    fn review_reference_has_length_limit_before_persistence() {
        let repo = "a".repeat(MAX_REVIEW_REFERENCE_CHARS + 1);
        let err = parse(&format!("prmonitor://review?pr=7&repo={repo}"))
            .expect_err("overlong repo rejected");
        assert!(err.message.contains("repo/projectId 过长"));
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
    fn parses_notify_deeplink() {
        let request = notify_request("Deploy done");
        let sig = notify_deeplink_signature(&request, "secret-token").expect("sign");
        let p = parse_notify(&format!(
            "prmonitor://notify?title=%20Deploy%20done%20&body=Build%2042&url=https://example.com/build/42&projectId=p1&level=warning&channelIds=desktop,slack-main&sig={sig}",
        ))
        .expect("ok");
        assert_eq!(p.request.title, "Deploy done");
        assert_eq!(p.request.body.as_deref(), Some("Build 42"));
        assert_eq!(
            p.request.url.as_deref(),
            Some("https://example.com/build/42")
        );
        assert_eq!(p.request.project_id.as_deref(), Some("p1"));
        assert_eq!(p.request.level, Some(NotificationLevel::Warning));
        assert_eq!(p.request.channel_ids, vec!["desktop", "slack-main"]);
        assert_eq!(p.signature, sig);
    }

    #[test]
    fn notify_deeplink_rejects_duplicate_keys_invalid_level_and_empty_title() {
        assert!(parse_notify("prmonitor://notify?title=a&title=b&body=x&sig=t").is_err());
        assert!(parse_notify("prmonitor://notify?title=a&body=x&sig=t&sig=u").is_err());
        assert!(parse_notify("prmonitor://notify?title=a&body=x&level=critical&sig=t").is_err());
        assert!(parse_notify("prmonitor://notify?title=%20%20&body=x&sig=t").is_err());
        assert!(parse_notify("prmonitor://notify?title=a&sig=t").is_err());
        assert!(parse_notify("prmonitor://notify?title=a&body=x").is_err());
        assert!(parse_notify("prmonitor://notify?title=a&body=x&token=t").is_err());
        assert!(parse_notify("prmonitor://review?title=a&body=x").is_err());
    }

    #[test]
    fn notify_deeplink_dedupe_key_is_one_shot() {
        let key = format!(
            "test-notify-dedupe:{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        assert!(remember_notify_deeplink_key(&key));
        assert!(!remember_notify_deeplink_key(&key));
    }

    #[tokio::test]
    async fn failed_notify_deeplink_enqueue_does_not_mark_seen() {
        let app = tauri::test::mock_builder()
            .plugin(tauri_plugin_notification::init())
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("build mock app");
        let db = crate::db::Database::open_in_memory().expect("open db");
        let secret = "secret-token-0123456789";
        let config_json = serde_json::json!({
            "projects": [],
            "activeProjectId": "",
            "localApiToken": secret,
        })
        .to_string();
        db.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO config_blob (id, json) VALUES (1, ?1)",
                [config_json],
            )?;
            Ok(())
        })
        .expect("seed config");
        app.manage(db);
        let state = AppState::default();
        let attempts = Arc::new(AtomicUsize::new(0));
        state.notification_sender.set_sink(Arc::new({
            let attempts = attempts.clone();
            move |_, _| {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(AppError::new("db down"))
            }
        }));
        app.manage(state);
        let request = notify_request(&format!(
            "Deploy done {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let url = signed_notify_url(&request, secret);

        handle_notify_one(app.handle().clone(), url.clone()).await;
        handle_notify_one(app.handle().clone(), url).await;

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
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
