//! `NotificationProvider` — the outbound notification seam (AB#1070), the output-side
//! mirror of `pr::source::EventSourceProvider`. The core hands a provider a normalized
//! [`crate::model::Notification`]; the provider absorbs all channel-specific differences,
//! so the core only ever deals in the normalized payload + an `AppResult` action result.
//!
//! Desktop (`tauri_plugin_notification`) is the MVP impl ([`DesktopNotifier`]); email /
//! Feishu / Telegram / WeChat Work plug in by adding a [`crate::model::NotificationKind`]
//! variant + an arm in [`deliver`] + their own impl (NOT implemented — design reservation).
//!
//! ## Governance
//! - **Channel dispatch = Hard.** [`deliver`] branches on an EXHAUSTIVE
//!   `match NotificationKind` (no wildcard, no `dyn`) — adding a channel variant without an
//!   arm is a compile error, so a future outbox (AB#1066) calls THIS one function and adding
//!   a channel never touches the outbox / rule-engine / inbox main flow. Mirrors how
//!   `comment_url::resolve_comment_url` dispatches on `SourceKind`.
//! - **Normalized payload = Medium.** The [`crate::model::Notification`] /
//!   `NotificationKind` / `NotificationLevel` wire shapes are locked by serde golden tests in
//!   `model.rs`; this slice consumes them but never redefines them.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use tauri::{AppHandle, Runtime};
use tauri_plugin_notification::NotificationExt;

use crate::error::{AppError, AppResult};
use crate::model::{Notification, NotificationKind};

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
    async fn deliver(&self, note: &Notification) -> AppResult<()>;
}

/// The reference [`NotificationProvider`] (AB#1070): a desktop notification via
/// `tauri_plugin_notification`. Holds the app handle the same way `CodexEngine` /
/// `ClaudeEngine` do (`&AppHandle<R>`), so it is constructed per-delivery at the call site.
pub struct DesktopNotifier<'a, R: Runtime> {
    pub app: &'a AppHandle<R>,
}

impl<R: Runtime> NotificationProvider for DesktopNotifier<'_, R> {
    async fn deliver(&self, note: &Notification) -> AppResult<()> {
        self.app
            .notification()
            .builder()
            .title(note.title.clone())
            .body(desktop_body(note))
            .show()
            .map_err(|e| AppError::new(format!("发送桌面通知失败: {e}")))
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

/// Dispatch a normalized notification to its channel (AB#1070). The EXHAUSTIVE
/// `match NotificationKind` is the **Hard** carrier (per `.claude/rules/prmonitor/ai-robust.md`):
/// a new channel variant fails to compile here until its arm + provider impl are added — so a
/// future outbox (AB#1066) calls THIS, and adding a channel never edits the outbox / rule-engine
/// / inbox main flow. No `Box<dyn>`: each arm monomorphizes its concrete provider.
pub async fn deliver<R: Runtime>(
    app: &AppHandle<R>,
    kind: NotificationKind,
    note: &Notification,
) -> AppResult<()> {
    match kind {
        NotificationKind::Desktop => DesktopNotifier { app }.deliver(note).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NotificationLevel, RedactedNotificationBody};
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
}
