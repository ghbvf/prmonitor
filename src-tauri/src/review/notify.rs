//! `NotificationProvider` — the outbound notification seam (AB#1070), the output-side
//! mirror of `pr::source::EventSourceProvider`. The core hands a provider a normalized
//! [`crate::model::Notification`]; the provider absorbs all channel-specific differences,
//! so the core only ever deals in the normalized payload + an `AppResult` action result.
//!
//! Desktop (`tauri_plugin_notification`) uses the app handle directly. Configured external
//! channels use [`crate::model::NotificationDeliveryChannel`], a model-level DTO assembled by
//! `lib.rs`, so this slice never imports config's persisted model.
//!
//! ## Governance
//! - **Channel dispatch = Hard.** [`deliver_channel`] branches on an EXHAUSTIVE
//!   `match NotificationKind` (no wildcard, no `dyn`) — adding a channel variant without an
//!   arm is a compile error. The outbox execution route stays in `lib.rs`; adding a channel
//!   extends the adapter match here, not the inbox/rule-engine main flow.
//! - **Normalized payload = Medium.** The [`crate::model::Notification`] /
//!   `NotificationKind` / `NotificationLevel` wire shapes are locked by serde golden tests in
//!   `model.rs`; this slice consumes them but never redefines them.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose, Engine as _};
use hmac::{Hmac, Mac};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use reqwest::StatusCode;
use serde_json::json;
use sha2::Sha256;
use tauri::{AppHandle, Runtime};
use tauri_plugin_notification::NotificationExt;
use url::Url;

use crate::error::{AppError, AppResult};
use crate::model::{
    ActionExecutionResult, Notification, NotificationDeliveryChannel, NotificationKind,
};

/// The durable-enqueue sink the composition root injects (AB#1066). Given a normalized
/// [`Notification`], it persists it into the action outbox for reliable (retried, restart-resumed)
/// delivery. OPAQUE on purpose: the `review` slice holds only this `dyn Fn` (the closure, installed
/// in `lib.rs`, is the sole place that names `crate::outbox`), so the review slice never references a
/// sibling slice — the producer mirror of how the inbox holds OPAQUE re-feed closures. The closure
/// captures its own `AppHandle`, so the signature stays `Fn(Notification) -> AppResult<()>`.
pub type NotificationSink = Arc<dyn Fn(Notification) -> AppResult<()> + Send + Sync>;

/// Holds the composition-root-injected [`NotificationSink`] (AB#1066). A `tauri::State` field on
/// [`crate::state::AppState`] (which stays `Default`), `&self` + interior mutability — mirroring the
/// inbox's `InboxManager`. The review slice enqueues durable notifications through [`enqueue`] without
/// ever naming `crate::outbox`.
#[derive(Default)]
pub struct NotificationOutbox {
    sink: StdMutex<Option<NotificationSink>>,
}

impl NotificationOutbox {
    /// Install the durable-enqueue sink (composition root, before any deeplink fires). Replaces any
    /// prior sink (last writer wins), mirroring `InboxManager::set_hooks`.
    pub fn set_sink(&self, sink: NotificationSink) {
        *self.sink.lock().unwrap() = Some(sink);
    }

    /// Enqueue a notification for durable delivery via the injected sink. Clones the `Arc` out of the
    /// lock before calling so the lock isn't held across the enqueue. Fails closed if the root hasn't
    /// installed the sink yet (never in practice — a notification before `setup` completes).
    pub fn enqueue(&self, note: Notification) -> AppResult<()> {
        let sink = self.sink.lock().unwrap().clone();
        match sink {
            Some(sink) => sink(note),
            None => Err(AppError::new(
                "通知出口未初始化（notify sink 未安装）".to_string(),
            )),
        }
    }
}

/// A channel that delivers a normalized [`Notification`]. Best-effort: a channel failure is
/// an `Err` the caller logs (notifications are fire-and-forget today), never a panic.
///
/// This is each channel's per-channel IMPLEMENTATION contract; channel SELECTION is
/// [`deliver`]'s exhaustive `match NotificationKind` (the Hard carrier), not `dyn`-dispatch on
/// this trait — so a new channel is an `impl` + one `match` arm, never a call-site edit.
#[allow(async_fn_in_trait)]
pub trait NotificationProvider {
    /// Deliver one normalized notification through this channel.
    async fn deliver(&self, note: &Notification) -> AppResult<ActionExecutionResult>;
}

/// The reference [`NotificationProvider`] (AB#1070): a desktop notification via
/// `tauri_plugin_notification`. Holds the app handle the same way `CodexEngine` /
/// `ClaudeEngine` do (`&AppHandle<R>`), so it is constructed per-delivery at the call site.
pub struct DesktopNotifier<'a, R: Runtime> {
    pub app: &'a AppHandle<R>,
}

impl<R: Runtime> NotificationProvider for DesktopNotifier<'_, R> {
    async fn deliver(&self, note: &Notification) -> AppResult<ActionExecutionResult> {
        self.app
            .notification()
            .builder()
            .title(note.title.clone())
            .body(desktop_body(note))
            .show()
            .map_err(|e| AppError::new(format!("发送桌面通知失败: {e}")))?;
        Ok(ActionExecutionResult::done())
    }
}

pub struct WebhookNotifier<'a> {
    pub channel: &'a NotificationDeliveryChannel,
}

impl NotificationProvider for WebhookNotifier<'_> {
    async fn deliver(&self, note: &Notification) -> AppResult<ActionExecutionResult> {
        let mut body = webhook_body(self.channel.kind, note);
        apply_feishu_signature(self.channel, &mut body)?;
        let url = signed_webhook_url(self.channel)?;
        let client = http_client(self.channel.timeout_secs)?;
        let resp = client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::new(safe_http_transport_error(self.channel, &e)))?;
        let status = resp.status();
        let retry_after = retry_after_secs(resp.headers());
        if !status.is_success() {
            return Ok(classify_http_status(self.channel, status, retry_after));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| AppError::new(safe_http_transport_error(self.channel, &e)))?;
        Ok(classify_provider_success_body(self.channel, &body))
    }
}

pub struct TelegramNotifier<'a> {
    pub channel: &'a NotificationDeliveryChannel,
}

impl NotificationProvider for TelegramNotifier<'_> {
    async fn deliver(&self, note: &Notification) -> AppResult<ActionExecutionResult> {
        let token = self.channel.telegram_bot_token.trim();
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        let body = json!({
            "chat_id": self.channel.telegram_chat_id.trim(),
            "text": text_message(note),
            "disable_web_page_preview": false,
        });
        let client = http_client(self.channel.timeout_secs)?;
        let resp = client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::new(safe_http_transport_error(self.channel, &e)))?;
        let status = resp.status();
        let retry_after = retry_after_secs(resp.headers());
        if !status.is_success() {
            return Ok(classify_http_status(self.channel, status, retry_after));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| AppError::new(safe_http_transport_error(self.channel, &e)))?;
        Ok(classify_provider_success_body(self.channel, &body))
    }
}

pub struct EmailNotifier<'a> {
    pub channel: &'a NotificationDeliveryChannel,
}

impl NotificationProvider for EmailNotifier<'_> {
    async fn deliver(&self, note: &Notification) -> AppResult<ActionExecutionResult> {
        let mut builder = Message::builder()
            .from(match self.channel.smtp_from.trim().parse() {
                Ok(addr) => addr,
                Err(_) => {
                    return Ok(ActionExecutionResult::Dead {
                        message: format!("通知渠道「{}」SMTP 发件人地址无效", self.channel.name),
                    });
                }
            })
            .subject(note.title.clone());
        for addr in self
            .channel
            .smtp_to
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            builder = builder.to(match addr.parse() {
                Ok(addr) => addr,
                Err(_) => {
                    return Ok(ActionExecutionResult::Dead {
                        message: format!("通知渠道「{}」SMTP 收件人地址无效", self.channel.name),
                    });
                }
            });
        }
        let email = match builder.body(text_message(note)) {
            Ok(email) => email,
            Err(_) => {
                return Ok(ActionExecutionResult::Dead {
                    message: format!("通知渠道「{}」邮件构造失败", self.channel.name),
                });
            }
        };
        let mut relay = match AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(
            self.channel.smtp_host.trim(),
        ) {
            Ok(relay) => relay
                .port(self.channel.smtp_port)
                .timeout(Some(Duration::from_secs(self.channel.timeout_secs))),
            Err(_) => {
                return Ok(ActionExecutionResult::Dead {
                    message: format!("通知渠道「{}」SMTP relay 配置无效", self.channel.name),
                });
            }
        };
        if !self.channel.smtp_username.trim().is_empty() {
            relay = relay.credentials(Credentials::new(
                self.channel.smtp_username.trim().to_string(),
                self.channel.smtp_password.clone(),
            ));
        }
        match relay.build().send(email).await {
            Ok(_) => Ok(ActionExecutionResult::done()),
            Err(e) => Ok(classify_smtp_error(self.channel, &e)),
        }
    }
}

/// The text shown in a desktop notification body for `note`: its `body`, with the actionable
/// `url` appended on its own line when present and not already the body (a desktop notification
/// has no clickable action, so the link must be visible in the text). Pure (no `AppHandle`) so
/// the body composition is unit-tested; [`DesktopNotifier::deliver`] itself shells into the
/// Tauri plugin and is exercised end-to-end, not in a unit test (parity with `comment_url.rs`'s
/// subprocess arm, which is also only integration-covered).
fn desktop_body(note: &Notification) -> String {
    if note.body.is_empty() {
        // No body: show the url alone (avoid a leading-newline blank first line).
        note.url.clone()
    } else if note.url.is_empty() || note.url == note.body.as_str() {
        note.body.as_str().to_string()
    } else {
        format!("{}\n{}", note.body.as_str(), note.url)
    }
}

fn text_message(note: &Notification) -> String {
    desktop_body(note)
}

fn http_client(timeout_secs: u64) -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(timeout_secs.max(1)))
        .build()
        .map_err(|e| AppError::new(format!("通知 HTTP client 初始化失败: {e}")))
}

fn webhook_body(kind: NotificationKind, note: &Notification) -> serde_json::Value {
    let text = text_message(note);
    match kind {
        NotificationKind::Slack => json!({ "text": text }),
        NotificationKind::WeChatWork => json!({ "msgtype": "text", "text": { "content": text } }),
        NotificationKind::Feishu => json!({ "msg_type": "text", "content": { "text": text } }),
        NotificationKind::DingTalk => json!({ "msgtype": "text", "text": { "content": text } }),
        NotificationKind::Desktop | NotificationKind::Email | NotificationKind::Telegram => {
            json!({ "text": text })
        }
    }
}

fn signed_webhook_url(channel: &NotificationDeliveryChannel) -> AppResult<Url> {
    let mut url = Url::parse(channel.webhook_url.trim()).map_err(|e| {
        AppError::new(format!(
            "通知渠道「{}」webhook URL 无效（已脱敏）: {e}",
            channel.name
        ))
    })?;
    if channel.kind == NotificationKind::DingTalk && !channel.webhook_secret.trim().is_empty() {
        let timestamp = now_millis();
        let sign = hmac_sha256_base64(
            channel.webhook_secret.trim().as_bytes(),
            format!("{timestamp}\n{}", channel.webhook_secret.trim()).as_bytes(),
        )?;
        url.query_pairs_mut()
            .append_pair("timestamp", &timestamp.to_string())
            .append_pair("sign", &sign);
    }
    Ok(url)
}

fn apply_feishu_signature(
    channel: &NotificationDeliveryChannel,
    body: &mut serde_json::Value,
) -> AppResult<()> {
    if channel.kind != NotificationKind::Feishu || channel.webhook_secret.trim().is_empty() {
        return Ok(());
    }
    let timestamp = now_secs();
    let sign = hmac_sha256_base64(
        format!("{timestamp}\n{}", channel.webhook_secret.trim()).as_bytes(),
        b"",
    )?;
    body["timestamp"] = json!(timestamp.to_string());
    body["sign"] = json!(sign);
    Ok(())
}

fn hmac_sha256_base64(key: &[u8], data: &[u8]) -> AppResult<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|_| AppError::new("通知 webhook secret 无效"))?;
    mac.update(data);
    Ok(general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    retry_after_secs_at(headers, SystemTime::now())
}

fn retry_after_secs_at(headers: &reqwest::header::HeaderMap, now: SystemTime) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.parse::<u64>().ok().or_else(|| {
                httpdate::parse_http_date(v).ok().map(|at| {
                    at.duration_since(now)
                        .map(|d| d.as_secs().max(1))
                        .unwrap_or(1)
                })
            })
        })
}

fn classify_http_status(
    channel: &NotificationDeliveryChannel,
    status: StatusCode,
    retry_after_secs: Option<u64>,
) -> ActionExecutionResult {
    if status.is_success() {
        return ActionExecutionResult::done();
    }
    let message = format!(
        "通知渠道「{}」HTTP 投递失败（status={}，已脱敏）",
        channel.name, status
    );
    if status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
        || status.is_server_error()
    {
        ActionExecutionResult::Retry {
            message,
            retry_after_secs,
        }
    } else {
        ActionExecutionResult::Dead { message }
    }
}

fn classify_provider_success_body(
    channel: &NotificationDeliveryChannel,
    body: &str,
) -> ActionExecutionResult {
    match channel.kind {
        NotificationKind::Slack => {
            if body.trim().is_empty() || body.trim().eq_ignore_ascii_case("ok") {
                ActionExecutionResult::done()
            } else {
                provider_business_dead(channel, "slack")
            }
        }
        NotificationKind::Telegram => {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
                return provider_business_dead(channel, "telegram-invalid-json");
            };
            if v.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                ActionExecutionResult::done()
            } else {
                provider_business_dead(channel, "telegram")
            }
        }
        NotificationKind::WeChatWork | NotificationKind::DingTalk => {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
                return provider_business_dead(channel, "webhook-invalid-json");
            };
            if v.get("errcode").and_then(|v| v.as_i64()) == Some(0) {
                ActionExecutionResult::done()
            } else {
                provider_business_dead(channel, "webhook")
            }
        }
        NotificationKind::Feishu => {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
                return provider_business_dead(channel, "feishu-invalid-json");
            };
            let ok_status = v.get("StatusCode").and_then(|v| v.as_i64()) == Some(0);
            let ok_code = v.get("code").and_then(|v| v.as_i64()) == Some(0);
            if ok_status || ok_code {
                ActionExecutionResult::done()
            } else {
                provider_business_dead(channel, "feishu")
            }
        }
        NotificationKind::Desktop | NotificationKind::Email => ActionExecutionResult::done(),
    }
}

fn provider_business_dead(
    channel: &NotificationDeliveryChannel,
    provider: &str,
) -> ActionExecutionResult {
    ActionExecutionResult::Dead {
        message: format!(
            "通知渠道「{}」{} 业务响应失败（已脱敏）",
            channel.name, provider
        ),
    }
}

fn classify_smtp_error(
    channel: &NotificationDeliveryChannel,
    err: &lettre::transport::smtp::Error,
) -> ActionExecutionResult {
    let class = if err.is_timeout() {
        "timeout"
    } else if err.is_transient() {
        "transient"
    } else if err.is_permanent() {
        "permanent"
    } else if err.is_client() {
        "client"
    } else if err.is_response() {
        "response"
    } else if err.is_tls() {
        "tls"
    } else if err.is_transport_shutdown() {
        "transport-shutdown"
    } else {
        "transport"
    };
    let retryable = err.is_timeout()
        || err.is_transient()
        || err.is_transport_shutdown()
        || !(err.is_permanent() || err.is_client() || err.is_response() || err.is_tls());
    classified_smtp_failure(&channel.name, class, retryable)
}

fn classified_smtp_failure(
    channel_name: &str,
    class: &str,
    retryable: bool,
) -> ActionExecutionResult {
    let message = format!("通知渠道「{channel_name}」SMTP {class} 失败（已脱敏）");
    if retryable {
        ActionExecutionResult::Retry {
            message,
            retry_after_secs: None,
        }
    } else {
        ActionExecutionResult::Dead { message }
    }
}

fn safe_http_transport_error(
    channel: &NotificationDeliveryChannel,
    err: &reqwest::Error,
) -> String {
    let class = if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
    } else if err.is_builder() {
        "config"
    } else {
        "transport"
    };
    format!(
        "通知渠道「{}」HTTP {class} 失败（URL/token 已脱敏）",
        channel.name
    )
}

/// Dispatch a normalized notification to its channel (AB#1070). The EXHAUSTIVE
/// `match NotificationKind` is the **Hard** carrier (per `.claude/rules/prmonitor/ai-robust.md`):
/// a new channel variant fails to compile here until its arm + provider impl are added — so a
/// future outbox (AB#1066) calls THIS, and adding a channel never edits the outbox / rule-engine
/// / inbox main flow. No `Box<dyn>`: each arm monomorphizes its concrete provider.
pub async fn deliver<R: Runtime>(
    app: &AppHandle<R>,
    kind: NotificationKind,
    note: &Notification,
) -> AppResult<ActionExecutionResult> {
    match kind {
        NotificationKind::Desktop => DesktopNotifier { app }.deliver(note).await,
        NotificationKind::Email
        | NotificationKind::Slack
        | NotificationKind::Telegram
        | NotificationKind::WeChatWork
        | NotificationKind::Feishu
        | NotificationKind::DingTalk => Ok(ActionExecutionResult::Dead {
            message: "通知渠道配置缺失：外部渠道必须通过 outbox delivery payload 指定 channelId"
                .to_string(),
        }),
    }
}

pub async fn deliver_channel(
    channel: &NotificationDeliveryChannel,
    note: &Notification,
) -> AppResult<ActionExecutionResult> {
    match channel.kind {
        NotificationKind::Desktop => Ok(ActionExecutionResult::Dead {
            message: "desktop channel requires AppHandle delivery path".to_string(),
        }),
        NotificationKind::Email => EmailNotifier { channel }.deliver(note).await,
        NotificationKind::Slack
        | NotificationKind::WeChatWork
        | NotificationKind::Feishu
        | NotificationKind::DingTalk => WebhookNotifier { channel }.deliver(note).await,
        NotificationKind::Telegram => TelegramNotifier { channel }.deliver(note).await,
    }
}

pub async fn deliver_channel_with_app<R: Runtime>(
    app: &AppHandle<R>,
    channel: &NotificationDeliveryChannel,
    note: &Notification,
) -> AppResult<ActionExecutionResult> {
    match channel.kind {
        NotificationKind::Desktop => DesktopNotifier { app }.deliver(note).await,
        _ => deliver_channel(channel, note).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NotificationDeliveryChannel, NotificationLevel, RedactedNotificationBody};
    fn note(url: &str, body: &str) -> Notification {
        Notification::new(
            NotificationLevel::Info,
            "PR #7 review 完成".to_string(),
            url.to_string(),
            RedactedNotificationBody::test_only(body),
            String::new(),
        )
    }

    #[test]
    fn desktop_body_returns_body_when_url_absent() {
        assert_eq!(
            desktop_body(&note("", "本次 review 完成（无评论链接）")),
            "本次 review 完成（无评论链接）"
        );
    }

    #[test]
    fn desktop_body_returns_body_when_url_equals_body() {
        // The completion path sets body == comment_url, so the link is not duplicated.
        let n = note("https://x/pr/7", "https://x/pr/7");
        assert_eq!(desktop_body(&n), "https://x/pr/7");
    }

    #[test]
    fn desktop_body_appends_url_when_distinct_from_body() {
        let n = note("https://x/pr/7", "评审完成");
        assert_eq!(desktop_body(&n), "评审完成\nhttps://x/pr/7");
    }

    #[test]
    fn desktop_body_returns_url_when_body_empty() {
        // No leading-newline blank line when body is empty but a url is present.
        assert_eq!(desktop_body(&note("https://x/pr/7", "")), "https://x/pr/7");
    }

    #[test]
    fn feishu_signature_is_added_to_webhook_body() {
        let channel = NotificationDeliveryChannel {
            id: "feishu-main".to_string(),
            name: "Feishu".to_string(),
            kind: NotificationKind::Feishu,
            webhook_secret: "secret-123".to_string(),
            ..delivery_channel(NotificationKind::Feishu)
        };
        let mut body = webhook_body(
            NotificationKind::Feishu,
            &note("https://x/pr/7", "评审完成"),
        );

        apply_feishu_signature(&channel, &mut body).expect("sign");

        let timestamp = body
            .get("timestamp")
            .and_then(|v| v.as_str())
            .expect("timestamp");
        assert!(timestamp.parse::<u64>().expect("timestamp is seconds") > 0);
        let expected_sign = hmac_sha256_base64(format!("{timestamp}\nsecret-123").as_bytes(), b"")
            .expect("expected sign");
        assert_eq!(
            body.get("sign").and_then(|v| v.as_str()),
            Some(expected_sign.as_str())
        );
    }

    #[test]
    fn http_status_classification_maps_retryable_and_permanent_failures() {
        let channel = delivery_channel(NotificationKind::Slack);
        assert_eq!(
            classify_http_status(&channel, StatusCode::OK, None),
            ActionExecutionResult::done()
        );
        assert_eq!(
            classify_http_status(&channel, StatusCode::TOO_MANY_REQUESTS, Some(42)),
            ActionExecutionResult::Retry {
                message: "通知渠道「test」HTTP 投递失败（status=429 Too Many Requests，已脱敏）"
                    .to_string(),
                retry_after_secs: Some(42),
            }
        );
        assert_eq!(
            classify_http_status(&channel, StatusCode::BAD_REQUEST, None),
            ActionExecutionResult::Dead {
                message: "通知渠道「test」HTTP 投递失败（status=400 Bad Request，已脱敏）"
                    .to_string(),
            }
        );
    }

    #[test]
    fn retry_after_accepts_delta_seconds_and_http_date() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "90".parse().expect("header"));
        assert_eq!(retry_after_secs_at(&headers, now), Some(90));

        headers.insert(
            reqwest::header::RETRY_AFTER,
            httpdate::fmt_http_date(now + Duration::from_secs(120))
                .parse()
                .expect("header"),
        );
        assert_eq!(retry_after_secs_at(&headers, now), Some(120));

        headers.insert(
            reqwest::header::RETRY_AFTER,
            httpdate::fmt_http_date(now - Duration::from_secs(5))
                .parse()
                .expect("header"),
        );
        assert_eq!(retry_after_secs_at(&headers, now), Some(1));

        headers.insert(
            reqwest::header::RETRY_AFTER,
            "later".parse().expect("header"),
        );
        assert_eq!(retry_after_secs_at(&headers, now), None);
    }

    #[test]
    fn smtp_failure_message_is_redacted_and_classified() {
        assert_eq!(
            classified_smtp_failure("mail", "transient", true),
            ActionExecutionResult::Retry {
                message: "通知渠道「mail」SMTP transient 失败（已脱敏）".to_string(),
                retry_after_secs: None,
            }
        );
        assert_eq!(
            classified_smtp_failure("mail", "permanent", false),
            ActionExecutionResult::Dead {
                message: "通知渠道「mail」SMTP permanent 失败（已脱敏）".to_string(),
            }
        );
    }

    #[test]
    fn provider_success_body_must_pass_business_protocol() {
        assert_eq!(
            classify_provider_success_body(
                &delivery_channel(NotificationKind::Telegram),
                r#"{"ok":true}"#
            ),
            ActionExecutionResult::done()
        );
        assert_eq!(
            classify_provider_success_body(
                &delivery_channel(NotificationKind::Telegram),
                r#"{"ok":false,"description":"bad token"}"#
            ),
            ActionExecutionResult::Dead {
                message: "通知渠道「test」telegram 业务响应失败（已脱敏）".to_string(),
            }
        );
        assert_eq!(
            classify_provider_success_body(
                &delivery_channel(NotificationKind::WeChatWork),
                r#"{"errcode":0,"errmsg":"ok"}"#
            ),
            ActionExecutionResult::done()
        );
        assert_eq!(
            classify_provider_success_body(
                &delivery_channel(NotificationKind::DingTalk),
                r#"{"errcode":310000,"errmsg":"keywords not in content"}"#
            ),
            ActionExecutionResult::Dead {
                message: "通知渠道「test」webhook 业务响应失败（已脱敏）".to_string(),
            }
        );
        assert_eq!(
            classify_provider_success_body(
                &delivery_channel(NotificationKind::Feishu),
                r#"{"StatusCode":0,"StatusMessage":"success"}"#
            ),
            ActionExecutionResult::done()
        );
        assert_eq!(
            classify_provider_success_body(
                &delivery_channel(NotificationKind::Feishu),
                r#"{"code":0,"msg":"success"}"#
            ),
            ActionExecutionResult::done()
        );
        assert_eq!(
            classify_provider_success_body(
                &delivery_channel(NotificationKind::Slack),
                "invalid_payload"
            ),
            ActionExecutionResult::Dead {
                message: "通知渠道「test」slack 业务响应失败（已脱敏）".to_string(),
            }
        );
    }

    fn delivery_channel(kind: NotificationKind) -> NotificationDeliveryChannel {
        NotificationDeliveryChannel {
            id: "test".to_string(),
            name: "test".to_string(),
            kind,
            webhook_url: "https://example.com/hook".to_string(),
            webhook_secret: String::new(),
            telegram_bot_token: "token".to_string(),
            telegram_chat_id: "chat".to_string(),
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            smtp_username: String::new(),
            smtp_password: String::new(),
            smtp_from: "from@example.com".to_string(),
            smtp_to: "to@example.com".to_string(),
            timeout_secs: 15,
        }
    }
}
