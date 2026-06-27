//! Cross-slice shared types — the contract boundary between slices.
//!
//! Slices must not import each other's internals; any type that crosses a slice
//! boundary lives here. Serialized fields use camelCase for the frontend.

use serde::{Deserialize, Serialize};

/// A PR discovered by a [`crate::pr::source::EventSourceProvider`] that may need review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub number: u64,
    pub head_sha: String,
    pub head_ref: String,
    pub author: String,
    pub is_cross_repository: bool,
    pub is_draft: bool,
    /// `"review"` or `"check"` — which pr-review mode the trigger label maps to.
    pub kind: String,
}

/// Which backend owns a [`TerminalSession`] (#1372).
///
/// **Hard carrier** (sealed enum): the command layer routes per-session ops through an
/// exhaustive `match TerminalBackendKind { ... }` (`terminal::commands::RoutedBackend`), so
/// adding a third backend without handling it everywhere is a compile error — the missing arm
/// cannot be expressed. The second backend (`WebPty`) realizes the seam the `#1383` slice
/// reserved.
///
/// Wire strings are pinned camelCase (`"iterm"` / `"webPty"`) — a cross-agent contract the
/// frontend's TS union mirrors exactly; the serde golden below
/// (`terminal_backend_kind_wire_strings`) locks them against a `rename_all` / variant drift.
/// `Default` is `Iterm`: the iTerm Python daemon returns rows with NO `backend` key, so a
/// `#[serde(default)]` parse fills in `Iterm` (the daemon contract stays unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum TerminalBackendKind {
    /// The iTerm2 Python-API daemon backend (#1383).
    #[default]
    Iterm,
    /// A local pseudo-terminal shell backend (#1372): `portable-pty`-spawned, exposed to the
    /// Remote Web Console; cross-platform (unix openpty + Windows ConPTY).
    WebPty,
}

/// One addressable terminal session in the user's iTerm (#1383). The leaf the
/// frontend xterm panel attaches to: `session_id` is the iTerm session GUID (the
/// attach key); `window_id` / `tab_id` group it in the picker (the frontend
/// flattens this list then re-groups window → tab → session client-side);
/// `rows` / `cols` are iTerm's current grid (seed the xterm size + validate a
/// fit-driven resize).
///
/// Front/back contract (UNLIKE backend-internal [`Candidate`]): the `terminal`
/// slice returns `Vec<TerminalSession>` over the `list_terminal_sessions` Tauri
/// command, so it IS mirrored in `src/types.ts` and the golden below locks the
/// camelCase wire shape both sides depend on. The [`backend`](Self::backend)
/// discriminator (#1372) tells the frontend which backend owns the row (label
/// "iTerm" vs "Web PTY", route close); it is ALWAYS serialized but
/// `#[serde(default)]` on parse, so the iTerm daemon's rows (which carry NO
/// `backend` key) deserialize to the Default `Iterm`. `Deserialize` too: the
/// Python daemon returns this camelCase shape, Rust parses then re-serializes to
/// the frontend (same dual-derive rationale as [`Candidate`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSession {
    pub session_id: String,
    pub window_id: String,
    pub tab_id: String,
    pub title: String,
    pub is_active: bool,
    pub rows: u16,
    pub cols: u16,
    /// Which backend owns this session (#1372). ALWAYS serialized (no `skip`) so the frontend
    /// can label + route; `#[serde(default)]` so the iTerm daemon's `backend`-less rows parse to
    /// `Iterm`.
    #[serde(default)]
    pub backend: TerminalBackendKind,
}

/// Options for `create_terminal_session` (#1383). Both optional: `None` lets the
/// daemon pick (a fresh window with the default profile). `skip_serializing_if`
/// OMITS an absent key so the daemon-side JSON matches the optional `windowId?` /
/// `profile?` the frontend `CreateSessionOpts` mirror declares (an absent key,
/// not a JSON `null`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionOpts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Which backend to create the session on (#1372). `None` routes to the default `Iterm`
    /// backend (the pre-#1372 behavior the iTerm daemon expects — `skip_serializing_if` OMITS the
    /// key so its createSession JSON is byte-unchanged); `Some(WebPty)` spawns a local PTY shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<TerminalBackendKind>,
}

/// Which PR source backs the monitor.
///
/// **Hard carrier** (sealed enum): once PR3+ wires source selection through an
/// exhaustive `match SourceKind { ... }`, adding a variant without handling it
/// is a compile error — the missing arm cannot be expressed. Today it has one
/// variant, so the seam is reserved but not yet load-bearing.
///
/// #11 design reservation: future variant `GitLab`. Wire strings for `Github` /
/// `Azure` / `Bitbucket` are pinned to `"github"` / `"azure"` / `"bitbucket"`
/// (cross-agent contract; the frontend mirrors them and a serde golden test locks them).
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum SourceKind {
    #[default]
    Github,
    /// Azure DevOps Repos: pulled via the `az repos pr list` CLI (#818) and pushed via
    /// inbound Azure DevOps Service Hooks on `/webhook` (`git.pullrequest.created/updated`,
    /// AB#822) — both feed the same PR list / dispatch path.
    Azure,
    /// Bitbucket Server / Data Center: pulled via the REST API v1.0 over HTTP
    /// (`{host}/rest/api/1.0/projects/{project}/repos/{repo}/pull-requests`, `reqwest`,
    /// AB#717). Bitbucket Server PRs carry NO native labels, so a Bitbucket project
    /// must use [`LabelSource::Title`] (status labels written into the PR title, e.g.
    /// `[pr-status/need-fix]`). No inbound webhook yet (poll/API discovery only).
    Bitbucket,
    // future #11: GitLab
}

/// Where a project's review/check trigger labels come from (AB#717).
///
/// **Hard carrier** (sealed enum): label resolution branches on an exhaustive
/// `match LabelSource { ... }` (`crate::pr::labels::effective_labels`), so adding a
/// variant without handling it is a compile error.
///
/// Wire strings are pinned camelCase (`"native" | "title"`) — a cross-agent contract
/// the frontend's TS union mirrors exactly; a serde golden test below locks it.
///
/// - [`Native`](Self::Native) (default, status quo): labels are the source provider's
///   own PR labels (GitHub labels / Azure DevOps tags).
/// - [`Title`](Self::Title): labels are parsed from bracketed segments in the PR title
///   (`[pr-status/need-fix][wip]` → `["pr-status/need-fix", "wip"]`). The ONLY viable
///   mode for Bitbucket Server, which has no native PR labels.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum LabelSource {
    #[default]
    Native,
    Title,
}

/// Per-project data-update mode (#818): how a project's PR list is kept fresh.
///
/// **Hard carrier** (sealed enum): source/poll selection branches on an exhaustive
/// `match UpdateMode { ... }` (the scheduler's `periodic_polling` and `poll_now`),
/// so adding a variant without handling it is a compile error.
///
/// Wire strings are pinned kebab-case (`"webhook-only" | "pull-only" | "hybrid" |
/// "manual"`) — a cross-agent contract the frontend's TS union must mirror exactly;
/// a serde golden test below locks it.
///
/// Modes:
/// - [`WebhookOnly`](Self::WebhookOnly) (default, the safe boot behavior): the list
///   is updated ONLY by inbound webhook deliveries — NO automatic CLI polling. A
///   manual pull is rejected (there is no source to pull from in this mode).
/// - [`PullOnly`](Self::PullOnly): a periodic CLI poll loop is the sole update source.
/// - [`Hybrid`](Self::Hybrid): both a periodic CLI poll loop AND inbound webhooks.
/// - [`Manual`](Self::Manual): no periodic loop; the list updates only on an explicit
///   "立即拉取" (one-shot CLI discovery) or inbound webhook.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateMode {
    #[default]
    WebhookOnly,
    PullOnly,
    Hybrid,
    Manual,
}

/// Which review engine runs against a PR.
///
/// **Hard carrier** (sealed enum): engine selection is wired through exhaustive
/// `match EngineKind { ... }` in the review start funnel (`commands.rs::start_via_engine`), so
/// adding a variant without handling it is a compile error — the missing arm cannot be expressed.
/// Now load-bearing (#718): `Claude` is available alongside `Codex`.
///
/// Wire strings are a cross-agent contract the frontend mirrors (`ENGINE_KINDS`
/// in `src/types.ts`): `Codex → "codex"`, `Claude → "claude"`. The serde golden
/// test below (`discriminator_enums_serialize_to_pinned_wire_strings`) is the
/// **Medium** carrier locking those strings against a `rename_all` / variant drift.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum EngineKind {
    #[default]
    Codex,
    /// `claude -p` headless (Claude Code) review engine (#718).
    Claude,
}

/// How the webhook receiver's local port is exposed to the public internet (#9).
///
/// Lives here (not in `pr/webhook.rs`) because, like [`SourceKind`] / [`EngineKind`],
/// it is a config→pr cross-slice kind enum: the `config` slice persists it on
/// `AppConfig` and the `pr` slice's `webhook::start` consumes it to branch the tunnel
/// strategy. Keeping it in `crate::model` is the SINGLE source both slices import,
/// rather than each defining its own — mirroring the `SourceKind` precedent.
///
/// Wire strings are pinned lowercase (`"quick" | "command" | "listener"`) — a
/// cross-agent contract the frontend's TS union must mirror exactly; a serde golden
/// test below locks it.
///
/// Modes:
/// - [`Quick`](Self::Quick) (default, unchanged status quo): spawn a Cloudflare Quick
///   Tunnel via `cloudflared` and scrape the `*.trycloudflare.com` URL.
/// - [`Command`](Self::Command): spawn a user-supplied tunnel command (e.g. a named
///   cloudflared tunnel, `ngrok`, …); `publicUrl` comes from config, not scraped.
/// - [`Listener`](Self::Listener): only bind the local port; the tunnel is fully
///   external (no child process); `publicUrl` comes from config.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebhookTunnelMode {
    #[default]
    Quick,
    Command,
    Listener,
}

/// A PR row shown in the UI (display superset of [`Candidate`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestView {
    pub number: u64,
    pub title: String,
    pub labels: Vec<String>,
    pub url: String,
    /// `"review"` or `"check"` — the trigger-label mode this PR maps to.
    pub kind: String,
    /// Why this PR would be skipped (not dispatched), or `None` when it would
    /// dispatch. Serializes to `null` / a string for the frontend.
    pub skip_reason: Option<String>,
}

/// Whether a tracked PR was seen in the latest discovery window or has aged out.
///
/// `Current` = last seen within the presence grace window (the live working set);
/// `Stale` = not seen recently (a transient `gh` miss or a genuinely closed PR),
/// retained so a one-round miss flips presence rather than dropping the row.
///
/// `Serialize`-only by design: this is a frontend projection derived from a
/// [`crate::pr::registry::TrackedPr`]'s `last_seen_epoch` vs the grace window, never
/// persisted or read back, so it intentionally does NOT derive `Deserialize` (the
/// persisted shape is `TrackedPr`).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PrPresence {
    Current,
    Stale,
}

/// Tracking-aware PR row emitted to the frontend (persisted-retention view).
///
/// Flattens [`PullRequestView`] so the wire shape stays a flat row plus the two
/// retention fields (`presence` / `archived`) the persisted-list UI renders.
///
/// `Serialize`-only by design: this is the frontend projection of a
/// [`crate::pr::registry::TrackedPr`] (computed per emit), never persisted or read
/// back, so it intentionally does NOT derive `Deserialize` — `TrackedPr` is the
/// persisted type.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackedPrView {
    #[serde(flatten)]
    pub pr: PullRequestView,
    pub presence: PrPresence,
    pub archived: bool,
}

/// The class of a normalized inbound [`Event`] (AB#1079, epic AB#1078): the event
/// pipeline's first cross-slice discriminator. The inbox (AB#1065) persists it; the rule
/// engine (AB#1068) matches on it.
///
/// **Hard carrier** (sealed enum): once a consumer (the inbox normalizer / rule matcher)
/// branches on an exhaustive `match EventType { ... }`, adding a variant without an arm is
/// a compile error — the missing arm cannot be expressed. Today the seam is RESERVED (no
/// consumer yet — the webhook path only emits `PullRequest`), exactly like [`SourceKind`]'s
/// reserved-but-not-yet-load-bearing note; the Hard carrier closes when the inbox lands.
///
/// Wire strings are pinned camelCase (`"pullRequest" | "issue" | "comment" | "label" |
/// "generic"`) — a cross-agent contract the frontend's `EVENT_TYPES` (`src/types.ts`)
/// mirrors; the serde golden test below (`event_type_serializes_to_pinned_wire_strings`)
/// is the **Medium** carrier locking them against a `rename_all` / variant drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum EventType {
    /// A pull-request event (the only class the current webhook path emits).
    #[default]
    PullRequest,
    /// An issue event (reserved for the inbox's issue ingestion, AB#1065).
    Issue,
    /// An issue/PR comment event.
    Comment,
    /// A label add/remove event.
    Label,
    /// A generic webhook event that does not map to the classes above.
    Generic,
}

/// A normalized inbound event — the event pipeline's cross-slice envelope (AB#1079,
/// epic AB#1078). External deliveries (the webhook today; future connectors, AB#1070) are
/// normalized into this shape; the inbox (AB#1065) persists it (dedup by
/// [`dedupe_key`](Self::dedupe_key)), the rule engine (AB#1068) matches on its fields, and
/// the outbox (AB#1066) acts on the result. It GENERALIZES the existing
/// `crate::pr::webhook::WebhookEvent` (`project_id` / `repo` / `number` / `title` /
/// `labels` / `url`), adding the cross-source identity (`source` / `event_type`) and the
/// idempotency key the inbox dedups on.
///
/// `Serialize` + `Deserialize`: the inbox stores it (as JSON) and reads it back, and it is
/// a front/back contract mirrored in `src/types.ts` (`Event`). The serde golden below
/// (`event_wire_shape_is_camel_case`) is the **Medium** carrier locking the camelCase wire
/// shape (upstream = Rust `rename_all`; the downstream TS mirror is hand-kept — the open
/// end of this funnel, future Hard path = codegen `types.ts` from `model.rs` +
/// `git diff --exit-code`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    /// Idempotency key the inbox dedups on (AB#1065): the SAME logical delivery (a webhook
    /// retry, a tunnel re-delivery) yields the SAME key, so it is processed exactly once.
    /// Composed by the inbox normalizer from a delivery's stable identity; the exact key
    /// format is defined by the inbox (AB#1065), not pinned here.
    pub dedupe_key: String,
    /// Which source produced the event (reuses the existing source discriminator).
    pub source: SourceKind,
    /// The event class (PR / issue / comment / label / generic).
    pub event_type: EventType,
    /// The matched project id (routing key), mirroring `WebhookEvent::project_id`.
    pub project_id: String,
    /// The repo `owner/name` (GitHub) or bare repo name (Azure), for matching / diagnostics.
    pub repo: String,
    /// The PR/issue number, or `None` for an event class that has none (a generic webhook).
    /// Serializes to JSON `null` (not omitted) so the TS mirror's `number | null` stays a
    /// closed contract.
    pub number: Option<u64>,
    /// The PR/issue title (`""` when absent), for title matchers / display.
    pub title: String,
    /// The PR/issue body (`""` when absent), for body matchers.
    pub body: String,
    /// The effective labels (post-[`LabelSource`] resolution), for label matchers.
    pub labels: Vec<String>,
    /// The event's HTML URL (`""` when absent).
    pub url: String,
    /// When the event was received (epoch seconds), stamped by the ingress.
    pub received_at_epoch: u64,
}

/// The processing state of one persisted inbox delivery (AB#1065, epic AB#1078): the
/// inbox's per-entry status, surfaced to the frontend's event-inbox panel.
///
/// **Hard carrier** (sealed enum): the inbox store branches on an exhaustive
/// `match InboxStatus { ... }` ([`crate::inbox::store::status_as_wire`]), so adding a
/// variant without an arm is a compile error — the missing case cannot be expressed.
/// Already load-bearing: the store's `as_wire` is the DB column source and the service's
/// `mark_processed` / `mark_failed` transitions read it back.
///
/// Wire strings are pinned camelCase (`"received" | "processed" | "failed"`) — a
/// cross-agent contract the frontend's `INBOX_STATUSES` (`src/types.ts`) mirrors; the
/// serde golden below (`inbox_status_serializes_to_pinned_wire_strings`) is the
/// **Medium** carrier locking them against a `rename_all` / variant drift. `Default` is
/// [`Received`](Self::Received) — the state every delivery starts in before processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum InboxStatus {
    /// The delivery was persisted (deduped) but not yet processed.
    #[default]
    Received,
    /// The delivery was re-fed through the dispatch path successfully.
    Processed,
    /// Processing raised an error (carried in [`InboxEntry::error`]); replayable.
    Failed,
}

/// One persisted inbox delivery row (AB#1065, epic AB#1078): a normalized [`Event`] plus
/// its processing state, surfaced to the frontend's event-inbox panel and the source of a
/// replay.
///
/// `Serialize`: a front/back contract mirrored in `src/types.ts` (`InboxEntry`); a field
/// change must be synced there in lockstep (the open end of this funnel — future Hard path
/// = codegen `types.ts` from `model.rs` + `git diff --exit-code`). The serde golden below
/// (`inbox_entry_wire_shape_is_camel_case`) is the **Medium** carrier locking the camelCase
/// wire shape.
///
/// The [`Event`] is NESTED (a real `event` object), NOT flattened — the panel renders the
/// envelope as a unit and the wire stays `{ id, event: { … }, status, processedAtEpoch,
/// error }`, distinct from [`TrackedPrView`]'s flatten.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxEntry {
    /// The inbox row id (the `inbox_event` table PRIMARY KEY) — the replay / get-raw key.
    pub id: i64,
    /// The normalized delivery envelope (nested, not flattened).
    pub event: Event,
    /// The processing state of this delivery.
    pub status: InboxStatus,
    /// When processing finished (epoch seconds), or `None` while still `Received`.
    /// Serializes to JSON `null` (not omitted) so the TS mirror's `processedAtEpoch:
    /// number | null` stays a closed contract.
    pub processed_at_epoch: Option<u64>,
    /// The failure message when `status` is `Failed`, else `None` (→ JSON `null`).
    pub error: Option<String>,
}

/// User-visible body text that is safe to send to a notification center or external
/// notification channel.
///
/// **Hard carrier** for the deeplink redaction rule: callers cannot place a raw
/// `String` into [`Notification::body`]. They must choose one of the typed constructors
/// below, making the safety decision explicit at the call site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RedactedNotificationBody(String);

impl RedactedNotificationBody {
    /// Fixed text authored by prmonitor, never derived from an external error message.
    pub fn fixed(text: &'static str) -> Self {
        Self(text.to_string())
    }

    /// Action URL already intended to be visible to the user.
    pub fn action_url(url: String) -> Self {
        Self(url)
    }

    /// Fixed deeplink failure text whose only dynamic component is the validated PR number.
    pub fn review_trigger_rejected(pr_number: u64) -> Self {
        Self(format!("PR #{pr_number}：项目无效或该 review 已在进行中"))
    }

    #[cfg(test)]
    pub(crate) fn test_only(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The normalized OUTBOUND payload (AB#1070): what the core hands a
/// [`crate::review::notify::NotificationProvider`], the output-side mirror of the
/// inbound [`Event`]. **Backend-internal** cross-Rust-slice contract (like
/// [`Candidate`], NOT [`PullRequestView`]): consumed only by Rust providers — today
/// the review slice's desktop notifier; a future outbox (AB#1066) drives it — so per
/// the charter it is intentionally NOT mirrored in `src/types.ts` (the funnel has no
/// open TS end). The serde golden (`notification_wire_shape_is_camel_case`) is the
/// **Medium** carrier locking the camelCase wire shape, so a future channel that
/// (de)serializes it (an email/webhook outbox queue) sees a stable shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    /// Severity, for channels that render it (email subject prefix, log level, icon).
    pub level: NotificationLevel,
    /// Short headline.
    pub title: String,
    /// The actionable URL (the pr-review comment URL today), or `""` when absent.
    pub url: String,
    /// Body text that is safe for persisted/exposed notification sinks. The private
    /// inner field on [`RedactedNotificationBody`] prevents raw `AppError::message`
    /// strings from being placed here by accident.
    pub body: RedactedNotificationBody,
    /// Routing key for a future multi-project / multi-channel outbox (AB#1066); `""` today.
    pub project_id: String,
}

impl Notification {
    pub fn new(
        level: NotificationLevel,
        title: String,
        url: String,
        body: RedactedNotificationBody,
        project_id: String,
    ) -> Self {
        Self {
            level,
            title,
            url,
            body,
            project_id,
        }
    }
}

/// Severity of a [`Notification`] (AB#1070). Sealed enum; the serde golden
/// (`notification_enums_serialize_to_pinned_wire_strings`) is the **Medium** carrier
/// pinning the camelCase wire strings. Default `Info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum NotificationLevel {
    #[default]
    Info,
    Warning,
    Error,
}

/// Which channel a [`Notification`] is delivered through (AB#1070).
///
/// **Hard carrier** (sealed enum): the outbound dispatch ([`crate::review::notify::deliver`]
/// today; a future outbox, AB#1066) branches on an exhaustive `match NotificationKind { ... }`,
/// so adding a variant without an arm is a compile error — the missing channel cannot be
/// expressed. Today ONE variant (`Desktop`), already load-bearing at the review-completion
/// call site (the only outbound today), mirroring how [`SourceKind`] / [`EngineKind`] dispatch.
///
/// AB#1459 adds external channels as concrete variants: `Email` → `"email"`, `Feishu` →
/// `"feishu"`, `Telegram` → `"telegram"`, `WeChatWork` → `"weChatWork"`. Wire string pinned
/// camelCase; the serde golden locks it (Medium carrier).
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum NotificationKind {
    #[default]
    Desktop,
    Email,
    Slack,
    Telegram,
    WeChatWork,
    Feishu,
    DingTalk,
}

/// Persisted payload for one notification delivery row (AB#1459).
///
/// **Hard carrier for secret exclusion:** this is the ONLY shape an outbox
/// `ActionKind::Notification` row executes. It carries the normalized notification plus a channel
/// reference (`channel_id` + `kind`), but it has no field capable of holding a webhook URL, bot
/// token, SMTP password, authorization header, or provider response. Adapters live-load channel
/// config by id at execution time, so the panel-visible raw payload cannot contain channel secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationDeliveryPayload {
    pub notification: Notification,
    pub channel_id: String,
    pub kind: NotificationKind,
}

/// Runtime delivery config for one notification channel (AB#1459).
///
/// Horizontal DTO: config owns persisted settings, review owns delivery adapters, and `lib.rs`
/// composes them. Adapters consume this model-level shape so the `review` slice never imports the
/// `config` slice's persisted model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationDeliveryChannel {
    pub id: String,
    pub name: String,
    pub kind: NotificationKind,
    pub webhook_url: String,
    pub webhook_secret: String,
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_username: String,
    pub smtp_password: String,
    pub smtp_from: String,
    pub smtp_to: String,
    pub timeout_secs: u64,
}

/// Classified result of one outbox action execution (AB#1459).
///
/// Horizontal because the composition root, outbox worker, and notification adapters all need the
/// same sealed result without any slice importing a sibling. Adapters map provider-specific
/// HTTP/SMTP failures into this type before the generic outbox worker records state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionExecutionResult {
    Done,
    Retry {
        message: String,
        retry_after_secs: Option<u64>,
    },
    Dead {
        message: String,
    },
}

/// The kind of side effect a persisted outbox row executes (AB#1066/AB#1069, epic AB#1078).
///
/// **Hard carrier** (sealed enum): the outbox's executor router branches on an exhaustive
/// `match ActionKind { ... }` (installed by the composition root in `lib.rs`, the only place that
/// names `review::notify` / `review::commands`), so adding a variant without an arm is a compile
/// error — the missing action cannot be expressed. The store's wire mapping
/// ([`crate::outbox::store::kind_as_wire`] / `kind_from_wire`) round-trips through serde, so a new
/// variant is carried automatically (no exhaustive store edit). Variants: `Notification` (the
/// review-completion desktop notification, AB#1066) + `Review` / `Check` / `StopReview` (the
/// AB#1069 action executor, each reusing the existing review funnel).
///
/// **Email / IM are NOT ActionKinds — they are [`NotificationKind`] channels** under the single
/// `Notification` action: "send via email/Feishu/…" is one notification delivered over a different
/// channel, sharing the unified status/retry/dead-letter lifecycle, not a distinct action class.
/// AB#1069/1070 design reservation for genuinely-distinct future kinds: `WebhookForward`,
/// `WorkItemComment` — each forcing a new executor arm. Wire string pinned camelCase; the serde
/// golden locks it (Medium carrier).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ActionKind {
    /// The review-completion desktop notification (AB#1066) → `"notification"`.
    #[default]
    Notification,
    /// Start a full review for a PR via the review funnel (AB#1069) → `"review"`.
    Review,
    /// Start a lightweight check for a PR via the review funnel (AB#1069) → `"check"`.
    Check,
    /// Interrupt an in-flight review/check session for a `(project, pr, kind)` (AB#1069) →
    /// `"stopReview"`. Idempotent: no live session is a benign no-op success, not a failure.
    StopReview,
    // future genuinely-distinct kinds (AB#1069/1070): WebhookForward, WorkItemComment.
    // Email/IM are NotificationKind channels, NOT kinds here — see the doc comment.
}

/// The outbox payload for a [`ActionKind::Review`] / [`ActionKind::Check`] action (AB#1069): the PR
/// the executor reviews via the review funnel (`review::commands::start_for_outbox`).
///
/// **Routing key is NOT here (AB#1069 F3).** The owning project is the OUTBOX ROW's single-source
/// routing key ([`crate::outbox::OutboxAction::project_id`] / [`OutboxEntry::project_id`] — what the
/// panel + `outbox:updated` events route by); the executor reads `action.project_id`, never a payload
/// copy. Carrying `project_id` here too would be a dual source of truth: a drifted/forged payload
/// could route a row shown under project A to project B's review. The action MODE (review vs check) is
/// likewise the sealed [`ActionKind`] variant, not a field — so neither the project nor the kind can be
/// forged in the persisted payload.
///
/// Backend-internal (read only by the `lib.rs` executor; a future Rule Engine, AB#1068, produces it),
/// so NOT mirrored in `src/types.ts`, like [`Notification`] / [`Candidate`]. It IS persisted in the
/// outbox `payload` column and replayed, so its camelCase shape must stay stable. serde camelCase;
/// the golden locks it (Medium carrier).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewActionPayload {
    /// The candidate to review. Carries the dispatch key inputs (`pr_number`, `head_sha`, `kind`)
    /// so the executor can land the ledger after the outbox action succeeds.
    pub candidate: Candidate,
}

/// The outbox payload for a [`ActionKind::StopReview`] action (AB#1069): which in-flight session to
/// interrupt, keyed (together with the row's `project_id`) by `(project, pr, kind)` — the review
/// funnel's native session key, not an engine-assigned thread id, so it is restart-stable and
/// producible by a future Rule Engine (AB#1068). The executor resolves it to a live `thread_id` via
/// `SessionRegistry::stop_target`; no live session is a benign no-op (idempotent).
///
/// **Routing key is NOT here (AB#1069 F3):** the owning project is the OUTBOX ROW's single-source
/// `project_id` (the executor reads `action.project_id`), never a payload copy — same anti-dual-source
/// rationale as [`ReviewActionPayload`]. Same backend-internal, persisted-and-replayed status (not
/// mirrored in `src/types.ts`). serde camelCase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopReviewActionPayload {
    /// The PR / MR number whose session to stop (re-validated `> 0` at the executor boundary).
    pub pr_number: u64,
    /// Which session mode to stop: `"review"` | `"check"` (re-validated at the executor boundary).
    pub kind: String,
}

/// The lifecycle state of one persisted outbox action (AB#1066, epic AB#1078): surfaced to the
/// frontend's action-outbox panel and the worker's terminal state.
///
/// **Hard carrier** (sealed enum): the outbox store branches on an exhaustive
/// `match ActionStatus { ... }` ([`crate::outbox::store::status_as_wire`]), so adding a variant
/// without an arm is a compile error — the missing case cannot be expressed.
///
/// Three states (no transient `Processing`): a queued action is [`Pending`](Self::Pending)
/// (`Default` — every action starts here) until the worker executes it; on success it is
/// [`Done`](Self::Done); a failure that exhausts the retry budget is [`Dead`](Self::Dead) — the
/// terminal dead-letter. A transient failure stays `Pending` (the row's `attempt_count` /
/// `last_error` carry the detail and `next_attempt_at` reschedules it), so a crash mid-execute
/// re-runs the action next boot (at-least-once). Wire strings are pinned camelCase
/// (`"pending" | "done" | "dead"`) — the frontend's `OUTBOX_STATUSES` (`src/types.ts`) mirrors
/// them; the serde golden below is the **Medium** carrier locking them against a `rename_all` /
/// variant drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ActionStatus {
    /// Queued (or awaiting a retry); the worker will execute it when `next_attempt_at` is due.
    #[default]
    Pending,
    /// Executed successfully — terminal.
    Done,
    /// The retry budget was exhausted — terminal dead-letter (`last_error` carries the reason).
    Dead,
}

/// One persisted outbox action row (AB#1066, epic AB#1078): the side effect's kind + lifecycle,
/// surfaced to the frontend's action-outbox panel. The raw `payload` is NOT carried here (it is
/// backend-internal — a `Notification` JSON today); the panel fetches it on demand via
/// `outbox_get_raw`, mirroring the inbox's `inbox_get_raw`.
///
/// `Serialize` only (like [`InboxEntry`]): a front/back contract mirrored in `src/types.ts`
/// (`OutboxEntry`) — a field change must be synced there in lockstep (the open end of this funnel;
/// future Hard path = codegen `types.ts` from `model.rs` + `git diff --exit-code`). The store
/// hydrates it from columns, so no `Deserialize`. The serde golden below
/// (`outbox_entry_wire_shape_is_camel_case`) is the **Medium** carrier locking the camelCase shape.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxEntry {
    /// The outbox row id (the `action_outbox` table PRIMARY KEY) — the get-raw / retry key.
    pub id: i64,
    /// Routing key (#35): which project this action belongs to.
    pub project_id: String,
    /// The side effect's kind.
    pub kind: ActionKind,
    /// A short human-readable label the panel renders without deserializing the payload.
    pub summary: String,
    /// The action's lifecycle state.
    pub status: ActionStatus,
    /// How many execution attempts have run (0 until the worker first tries it).
    pub attempt_count: u32,
    /// When the action is next eligible to run (epoch seconds); a retry pushes it forward.
    pub next_attempt_at: u64,
    /// The most recent failure message, or `None` (→ JSON `null`) if it has never failed.
    pub last_error: Option<String>,
    /// When the action was enqueued (epoch seconds).
    pub created_at: u64,
    /// When the row last transitioned (epoch seconds).
    pub updated_at: u64,
}

/// Serde wire-shape locks for `model.rs`'s cross-slice types.
///
/// The **Medium carrier** for these serde shapes per
/// `.claude/rules/prmonitor/ai-robust.md`. Each is a LOCK (characterization)
/// test: it passes on current code and only fails if a field is renamed or the
/// camelCase serialization breaks. The two types differ in their *downstream*,
/// so their contracts are not the same thing:
///
/// - [`PullRequestView`] is a **front/back contract** mirrored in
///   `src/types.ts`; a key change must be synced there in lockstep — the open
///   end of that funnel (no machine check on the TS side yet; future Hard path =
///   codegen `types.ts` from `model.rs` + `git diff --exit-code`).
/// - [`Candidate`] is **backend-internal**, cross-Rust-slice only: per the
///   charter it is intentionally *not* mirrored in `src/types.ts`, so its lock
///   guards the camelCase wire shape the `pr`/`review` slices rely on, **not** a
///   front/back contract — do not sync it to the frontend.
#[cfg(test)]
mod tests {
    use super::*;

    // Backend-internal cross-slice lock: `Candidate` is not exposed to the
    // frontend and is intentionally absent from `src/types.ts` (per the charter).
    #[test]
    fn candidate_wire_shape_is_camel_case() {
        let candidate = Candidate {
            number: 1,
            head_sha: "abc123".to_string(),
            head_ref: "feature/x".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            kind: "review".to_string(),
        };

        let v = serde_json::to_value(&candidate).expect("Candidate serializes");

        // camelCase keys present.
        assert!(v.get("number").is_some());
        assert!(v.get("headSha").is_some());
        assert!(v.get("headRef").is_some());
        assert!(v.get("author").is_some());
        assert!(v.get("isCrossRepository").is_some());
        assert!(v.get("isDraft").is_some());
        assert!(v.get("kind").is_some());

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("head_sha").is_none());
        assert!(v.get("head_ref").is_none());
        assert!(v.get("is_cross_repository").is_none());
        assert!(v.get("is_draft").is_none());
    }

    // Serde wire-shape lock for the #1383 terminal listing contract (Medium carrier per
    // ai-robust.md): UNLIKE `Candidate`, `TerminalSession` IS a front/back contract (returned
    // by `list_terminal_sessions`), so `src/types.ts` mirrors it in lockstep — this golden is
    // the upstream lock; the TS interface is the open downstream end (future Hard path = codegen
    // `types.ts` from `model.rs` + `git diff --exit-code`). Locks camelCase keys present /
    // snake_case absent so a Rust-side rename surfaces here before it silently breaks the panel.
    #[test]
    fn terminal_session_wire_shape_is_camel_case() {
        let session = TerminalSession {
            session_id: "w0t0p0".to_string(),
            window_id: "w0".to_string(),
            tab_id: "t0".to_string(),
            title: "zsh".to_string(),
            is_active: true,
            rows: 24,
            cols: 80,
            backend: TerminalBackendKind::Iterm,
        };

        let v = serde_json::to_value(&session).expect("TerminalSession serializes");

        // camelCase keys present.
        assert!(v.get("sessionId").is_some());
        assert!(v.get("windowId").is_some());
        assert!(v.get("tabId").is_some());
        assert!(v.get("title").is_some());
        assert!(v.get("isActive").is_some());
        assert!(v.get("rows").is_some());
        assert!(v.get("cols").is_some());
        // #1372: the `backend` discriminator is ALWAYS present (no `skip`) and pins to "iterm"
        // for an iTerm row; the frontend reads it to label + route close.
        assert_eq!(v["backend"], "iterm");

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("session_id").is_none());
        assert!(v.get("window_id").is_none());
        assert!(v.get("tab_id").is_none());
        assert!(v.get("is_active").is_none());

        // A WebPty row pins to "webPty" (the second backend, #1372).
        let pty = serde_json::to_value(TerminalSession {
            session_id: "webpty-1".to_string(),
            window_id: "webpty".to_string(),
            tab_id: "webpty".to_string(),
            title: "sh".to_string(),
            is_active: true,
            rows: 24,
            cols: 80,
            backend: TerminalBackendKind::WebPty,
        })
        .expect("TerminalSession serializes");
        assert_eq!(pty["backend"], "webPty");
    }

    // #1372: a daemon row arrives with NO `backend` key; `#[serde(default)]` fills `Iterm`, so an
    // iTerm session re-serialized to the frontend carries `backend == "iterm"` without the daemon
    // ever sending it. This locks the deserialize-default half of the contract.
    #[test]
    fn terminal_session_defaults_backend_to_iterm_when_absent() {
        let daemon_row = serde_json::json!({
            "sessionId": "p0", "windowId": "w0", "tabId": "t0",
            "title": "zsh", "isActive": true, "rows": 24, "cols": 80
        });
        let parsed: TerminalSession =
            serde_json::from_value(daemon_row).expect("daemon row parses without `backend`");
        assert_eq!(parsed.backend, TerminalBackendKind::Iterm);
    }

    // `CreateSessionOpts`: camelCase + `skip_serializing_if` OMITS absent keys (an absent key,
    // not a JSON null), matching the optional `windowId?` / `profile?` TS mirror.
    #[test]
    fn create_session_opts_wire_shape_omits_none() {
        let full = CreateSessionOpts {
            window_id: Some("w0".to_string()),
            profile: Some("Default".to_string()),
            backend: Some(TerminalBackendKind::WebPty),
        };
        let v = serde_json::to_value(&full).expect("CreateSessionOpts serializes");
        assert_eq!(v["windowId"], "w0");
        assert_eq!(v["profile"], "Default");
        assert!(v.get("window_id").is_none());
        // #1372: `Some(WebPty)` pins to "webPty" (a create-time backend pick).
        assert_eq!(v["backend"], "webPty");

        // None → the key is OMITTED entirely (not a JSON null) — `backend: None` routes to Iterm.
        let empty = serde_json::to_value(CreateSessionOpts::default()).expect("serializes");
        assert!(empty.get("windowId").is_none(), "None omits windowId");
        assert!(empty.get("profile").is_none(), "None omits profile");
        assert!(empty.get("backend").is_none(), "None omits backend");
    }

    // Cross-agent wire contract lock for the #1372 backend discriminator: the frontend mirrors
    // these exact strings to pick a create-time backend + label/route sessions. A variant rename
    // or `rename_all` change surfaces here (Medium carrier; the exhaustive `match` in
    // `terminal::commands::RoutedBackend` is the Hard one). Default is `Iterm` ("iterm").
    #[test]
    fn terminal_backend_kind_wire_strings() {
        assert_eq!(
            serde_json::to_value(TerminalBackendKind::Iterm).expect("serializes"),
            "iterm"
        );
        assert_eq!(
            serde_json::to_value(TerminalBackendKind::WebPty).expect("serializes"),
            "webPty"
        );
        assert_eq!(
            serde_json::to_value(TerminalBackendKind::default()).expect("serializes"),
            "iterm"
        );
    }

    // Cross-agent wire contract lock: the frontend mirrors these exact strings.
    // A variant rename or `rename_all` change surfaces here.
    #[test]
    fn discriminator_enums_serialize_to_pinned_wire_strings() {
        assert_eq!(
            serde_json::to_value(SourceKind::Github).expect("SourceKind serializes"),
            "github"
        );
        // #818: the Azure source variant pins to "azure" (the frontend mirrors it).
        assert_eq!(
            serde_json::to_value(SourceKind::Azure).expect("SourceKind serializes"),
            "azure"
        );
        // AB#717: the Bitbucket Server source variant pins to "bitbucket".
        assert_eq!(
            serde_json::to_value(SourceKind::Bitbucket).expect("SourceKind serializes"),
            "bitbucket"
        );
        assert_eq!(
            serde_json::to_value(EngineKind::Codex).expect("EngineKind serializes"),
            "codex"
        );
        // #718: the Claude review engine variant pins to "claude" (the frontend
        // mirrors it in `ENGINE_KINDS`); a variant rename or `rename_all` change
        // surfaces here (Medium carrier; the exhaustive `match` wiring is the Hard one).
        assert_eq!(
            serde_json::to_value(EngineKind::Claude).expect("EngineKind serializes"),
            "claude"
        );
        // AB#717: per-project label source. camelCase wire strings the frontend mirrors;
        // a variant rename or `rename_all` change surfaces here. Default is `Native`
        // (the status-quo: provider's own PR labels).
        assert_eq!(
            serde_json::to_value(LabelSource::Native).expect("LabelSource serializes"),
            "native"
        );
        assert_eq!(
            serde_json::to_value(LabelSource::Title).expect("LabelSource serializes"),
            "title"
        );
        assert_eq!(
            serde_json::to_value(LabelSource::default()).expect("LabelSource serializes"),
            "native"
        );
        // #818: per-project data-update modes. kebab-case wire strings the frontend
        // mirrors; a variant rename or `rename_all` change surfaces here. Default is
        // `WebhookOnly` (the safe boot default: NO automatic CLI polling).
        assert_eq!(
            serde_json::to_value(UpdateMode::WebhookOnly).expect("UpdateMode serializes"),
            "webhook-only"
        );
        assert_eq!(
            serde_json::to_value(UpdateMode::PullOnly).expect("UpdateMode serializes"),
            "pull-only"
        );
        assert_eq!(
            serde_json::to_value(UpdateMode::Hybrid).expect("UpdateMode serializes"),
            "hybrid"
        );
        assert_eq!(
            serde_json::to_value(UpdateMode::Manual).expect("UpdateMode serializes"),
            "manual"
        );
        assert_eq!(
            serde_json::to_value(UpdateMode::default()).expect("UpdateMode serializes"),
            "webhook-only"
        );
    }

    // Cross-agent wire contract lock for `WebhookTunnelMode` (#9): the frontend's TS
    // union mirrors these exact lowercase strings. A variant rename or a
    // `rename_all` change surfaces here. Default is `Quick` (status-quo behavior).
    #[test]
    fn webhook_tunnel_mode_serializes_to_pinned_wire_strings() {
        assert_eq!(
            serde_json::to_value(WebhookTunnelMode::Quick).expect("serializes"),
            "quick"
        );
        assert_eq!(
            serde_json::to_value(WebhookTunnelMode::Command).expect("serializes"),
            "command"
        );
        assert_eq!(
            serde_json::to_value(WebhookTunnelMode::Listener).expect("serializes"),
            "listener"
        );
        assert_eq!(
            serde_json::to_value(WebhookTunnelMode::default()).expect("serializes"),
            "quick"
        );
    }

    // Front/back contract lock: `PullRequestView` is mirrored in `src/types.ts`;
    // a field change here must be synced to that interface in lockstep.
    #[test]
    fn pull_request_view_wire_shape_is_camel_case() {
        let view = PullRequestView {
            number: 1,
            title: "Add feature".to_string(),
            labels: vec!["review".to_string()],
            url: "https://example.com/pr/1".to_string(),
            kind: "review".to_string(),
            skip_reason: Some("draft PR".to_string()),
        };

        let v = serde_json::to_value(&view).expect("PullRequestView serializes");

        // camelCase / flat keys present.
        assert!(v.get("number").is_some());
        assert!(v.get("title").is_some());
        assert!(v.get("labels").is_some());
        assert!(v.get("url").is_some());
        assert!(v.get("kind").is_some());
        assert!(v.get("skipReason").is_some());

        // snake_case form absent — a rename of the one multi-word field
        // (`skip_reason`) would surface here.
        assert!(v.get("skip_reason").is_none());
    }

    // A non-skipped PR serializes `skipReason` as JSON null (not omitted) so the
    // frontend's `skipReason: string | null` mirror stays a closed contract.
    #[test]
    fn pull_request_view_none_skip_reason_serializes_to_null() {
        let view = PullRequestView {
            number: 2,
            title: "Ready".to_string(),
            labels: vec![],
            url: "https://example.com/pr/2".to_string(),
            kind: "check".to_string(),
            skip_reason: None,
        };

        let v = serde_json::to_value(&view).expect("PullRequestView serializes");
        assert_eq!(v["skipReason"], serde_json::Value::Null);
    }

    // Front/back contract lock for the persisted-retention row (Medium carrier per
    // ai-robust.md): `TrackedPrView` is mirrored in `src/types.ts`. It flattens
    // `PullRequestView`, so the inner keys (`number,title,labels,url,kind,skipReason`)
    // must surface at the top level alongside `presence` / `archived`; a flatten
    // regression or field rename surfaces here and must be synced to the TS mirror
    // in lockstep (the open end of this funnel; future Hard path = codegen from
    // `model.rs` + `git diff --exit-code`).
    #[test]
    fn tracked_pr_view_wire_shape_is_camel_case() {
        let view = TrackedPrView {
            pr: PullRequestView {
                number: 1,
                title: "Add feature".to_string(),
                labels: vec!["review".to_string()],
                url: "https://example.com/pr/1".to_string(),
                kind: "review".to_string(),
                skip_reason: None,
            },
            presence: PrPresence::Current,
            archived: false,
        };

        let v = serde_json::to_value(&view).expect("TrackedPrView serializes");

        // Flattened `PullRequestView` keys present at the top level.
        assert!(v.get("number").is_some());
        assert!(v.get("title").is_some());
        assert!(v.get("labels").is_some());
        assert!(v.get("url").is_some());
        assert!(v.get("kind").is_some());
        assert!(v.get("skipReason").is_some());

        // Retention keys present (camelCase).
        assert!(v.get("presence").is_some());
        assert!(v.get("archived").is_some());

        // `flatten` must hoist the inner fields, NOT nest them under a `pr` wrapper;
        // and the one multi-word field must not leak its snake_case form.
        assert!(v.get("skip_reason").is_none());
        assert!(
            v.get("pr").is_none(),
            "flatten must not nest a 'pr' wrapper"
        );

        // None `skip_reason` serializes as JSON null (not omitted), mirroring
        // `PullRequestView`'s closed `skipReason: string | null` contract at the
        // flattened depth.
        assert_eq!(v["skipReason"], serde_json::Value::Null);

        // `presence` serializes to the pinned lowercase wire strings the TS mirror
        // discriminates on.
        assert_eq!(v["presence"], "current");
        let stale = serde_json::to_value(PrPresence::Stale).expect("PrPresence serializes");
        assert_eq!(stale, "stale");
    }

    // Cross-agent wire contract lock for the AB#1079 event-pipeline discriminator
    // `EventType` (Medium carrier per ai-robust.md): the frontend's `EVENT_TYPES`
    // (`src/types.ts`) mirrors these exact camelCase strings. A variant rename or a
    // `rename_all` change surfaces here (the exhaustive `match` a future inbox/rule
    // consumer adds is the Hard carrier). Default is `PullRequest` (the only class the
    // current webhook path emits).
    #[test]
    fn event_type_serializes_to_pinned_wire_strings() {
        assert_eq!(
            serde_json::to_value(EventType::PullRequest).expect("EventType serializes"),
            "pullRequest"
        );
        assert_eq!(
            serde_json::to_value(EventType::Issue).expect("EventType serializes"),
            "issue"
        );
        assert_eq!(
            serde_json::to_value(EventType::Comment).expect("EventType serializes"),
            "comment"
        );
        assert_eq!(
            serde_json::to_value(EventType::Label).expect("EventType serializes"),
            "label"
        );
        assert_eq!(
            serde_json::to_value(EventType::Generic).expect("EventType serializes"),
            "generic"
        );
        assert_eq!(
            serde_json::to_value(EventType::default()).expect("EventType serializes"),
            "pullRequest"
        );
    }

    // Front/back contract lock for the AB#1079 normalized `Event` envelope (Medium carrier
    // per ai-robust.md): mirrored in `src/types.ts` (`Event`); a field change must be synced
    // there in lockstep (the open end of the funnel — future Hard path = codegen from
    // `model.rs` + `git diff --exit-code`). Locks camelCase keys present + snake_case absent,
    // the nested `source`/`eventType` wire strings, and that an absent `number` (a generic /
    // non-numbered event) serializes as JSON null (not omitted) so the TS mirror's
    // `number: number | null` stays a closed contract.
    #[test]
    fn event_wire_shape_is_camel_case() {
        let event = Event {
            dedupe_key: "github:pullRequest:owner/repo#7:labeled".to_string(),
            source: SourceKind::Github,
            event_type: EventType::PullRequest,
            project_id: "p1".to_string(),
            repo: "owner/repo".to_string(),
            number: Some(7),
            title: "Add feature".to_string(),
            body: "body text".to_string(),
            labels: vec!["pr-review".to_string()],
            url: "https://example.com/pr/7".to_string(),
            received_at_epoch: 1_700_000_000,
        };

        let v = serde_json::to_value(&event).expect("Event serializes");

        // camelCase keys present.
        assert!(v.get("dedupeKey").is_some());
        assert!(v.get("source").is_some());
        assert!(v.get("eventType").is_some());
        assert!(v.get("projectId").is_some());
        assert!(v.get("repo").is_some());
        assert!(v.get("number").is_some());
        assert!(v.get("title").is_some());
        assert!(v.get("body").is_some());
        assert!(v.get("labels").is_some());
        assert!(v.get("url").is_some());
        assert!(v.get("receivedAtEpoch").is_some());

        // snake_case forms absent — a rename of any multi-word field surfaces here.
        assert!(v.get("dedupe_key").is_none());
        assert!(v.get("event_type").is_none());
        assert!(v.get("project_id").is_none());
        assert!(v.get("received_at_epoch").is_none());

        // The nested kind enums serialize to their pinned wire strings.
        assert_eq!(v["source"], "github");
        assert_eq!(v["eventType"], "pullRequest");

        // An absent `number` (a generic / non-numbered event) serializes as JSON null
        // (not omitted), keeping the TS mirror's `number: number | null` a closed contract.
        let generic = Event {
            number: None,
            event_type: EventType::Generic,
            ..event
        };
        let gv = serde_json::to_value(&generic).expect("Event serializes");
        assert_eq!(gv["number"], serde_json::Value::Null);
    }

    // Cross-agent wire contract lock for the AB#1065 inbox status (Medium carrier per
    // ai-robust.md): the frontend's `INBOX_STATUSES` (`src/types.ts`) mirrors these exact
    // camelCase strings. A variant rename or a `rename_all` change surfaces here (the
    // exhaustive `match InboxStatus` in `inbox::store::status_as_wire` is the Hard carrier).
    // Default is `Received` (the state every delivery starts in).
    #[test]
    fn inbox_status_serializes_to_pinned_wire_strings() {
        assert_eq!(
            serde_json::to_value(InboxStatus::Received).expect("InboxStatus serializes"),
            "received"
        );
        assert_eq!(
            serde_json::to_value(InboxStatus::Processed).expect("InboxStatus serializes"),
            "processed"
        );
        assert_eq!(
            serde_json::to_value(InboxStatus::Failed).expect("InboxStatus serializes"),
            "failed"
        );
        assert_eq!(
            serde_json::to_value(InboxStatus::default()).expect("InboxStatus serializes"),
            "received"
        );
    }

    // Front/back contract lock for the AB#1065 `InboxEntry` row (Medium carrier per
    // ai-robust.md): mirrored in `src/types.ts` (`InboxEntry`); a field change must be synced
    // there in lockstep (the open end of the funnel — future Hard path = codegen from
    // `model.rs` + `git diff --exit-code`). Locks camelCase keys present + snake_case absent,
    // the NESTED `event` object (not flattened — its own camelCase keys surface under `event`),
    // the nested `status` wire string, and that an absent `processedAtEpoch` / `error`
    // serializes as JSON null (not omitted) so the TS mirror's `… | null` stays closed.
    #[test]
    fn inbox_entry_wire_shape_is_camel_case() {
        let entry = InboxEntry {
            id: 7,
            event: Event {
                dedupe_key: "github:abc-123".to_string(),
                source: SourceKind::Github,
                event_type: EventType::PullRequest,
                project_id: "p1".to_string(),
                repo: "owner/repo".to_string(),
                number: Some(7),
                title: "Add feature".to_string(),
                body: String::new(),
                labels: vec!["pr-review".to_string()],
                url: "https://example.com/pr/7".to_string(),
                received_at_epoch: 1_700_000_000,
            },
            status: InboxStatus::Processed,
            processed_at_epoch: Some(1_700_000_005),
            error: None,
        };

        let v = serde_json::to_value(&entry).expect("InboxEntry serializes");

        // camelCase keys present at the top level.
        assert!(v.get("id").is_some());
        assert!(v.get("event").is_some());
        assert!(v.get("status").is_some());
        assert!(v.get("processedAtEpoch").is_some());
        assert!(v.get("error").is_some());

        // snake_case form absent — a rename of the multi-word field surfaces here.
        assert!(v.get("processed_at_epoch").is_none());

        // The event is NESTED (a real object), NOT flattened — its camelCase keys live
        // UNDER `event`, and do not leak to the top level (the contrast with `TrackedPrView`).
        let ev = &v["event"];
        assert!(ev.is_object(), "event is a nested object, not flattened");
        assert!(ev.get("dedupeKey").is_some());
        assert!(ev.get("eventType").is_some());
        assert!(ev.get("receivedAtEpoch").is_some());
        assert!(
            v.get("dedupeKey").is_none(),
            "nested event keys must not hoist to the top level"
        );

        // The nested `status` enum serializes to its pinned wire string.
        assert_eq!(v["status"], "processed");

        // An absent `processedAtEpoch` / `error` serializes as JSON null (not omitted),
        // keeping the TS mirror's `processedAtEpoch: number | null` / `error: string | null`
        // a closed contract.
        let received = InboxEntry {
            status: InboxStatus::Received,
            processed_at_epoch: None,
            error: None,
            ..entry
        };
        let rv = serde_json::to_value(&received).expect("InboxEntry serializes");
        assert_eq!(rv["status"], "received");
        assert_eq!(rv["processedAtEpoch"], serde_json::Value::Null);
        assert_eq!(rv["error"], serde_json::Value::Null);
    }

    // Backend-internal cross-slice lock for the AB#1070 normalized `Notification` (Medium
    // carrier per ai-robust.md): the output-side mirror of `Event`. Like `Candidate` it is
    // NOT mirrored in `src/types.ts` (consumed only by Rust providers / a future outbox), so
    // this lock guards the camelCase wire shape the producer/consumer rely on — the funnel has
    // no open TS end. Locks camelCase keys present + snake_case absent + the nested level string.
    #[test]
    fn notification_wire_shape_is_camel_case() {
        let note = Notification::new(
            NotificationLevel::Info,
            "PR #7 review 完成".to_string(),
            "https://example.com/pr/7".to_string(),
            RedactedNotificationBody::action_url("https://example.com/pr/7".to_string()),
            "p1".to_string(),
        );

        let v = serde_json::to_value(&note).expect("Notification serializes");

        // camelCase keys present.
        assert!(v.get("level").is_some());
        assert!(v.get("title").is_some());
        assert!(v.get("url").is_some());
        assert!(v.get("body").is_some());
        assert!(v.get("projectId").is_some());

        // snake_case form absent — a rename of the multi-word field surfaces here.
        assert!(v.get("project_id").is_none());

        // The nested level enum serializes to its pinned wire string.
        assert_eq!(v["level"], "info");
        // The body newtype is serde-transparent, preserving the outbound wire shape.
        assert_eq!(v["body"], "https://example.com/pr/7");

        // Roundtrips back (a future outbox queue deserializes it) — exercises `Deserialize`.
        let back: Notification = serde_json::from_value(v).expect("Notification deserializes");
        assert_eq!(back.level, NotificationLevel::Info);
        assert_eq!(back.body.as_str(), "https://example.com/pr/7");
        assert_eq!(back.project_id, "p1");
    }

    // Cross-Rust-slice wire contract lock for `NotificationLevel` / `NotificationKind`
    // (AB#1070, Medium carrier): a variant rename or `rename_all` change surfaces here. The
    // exhaustive `match NotificationKind` in `review::notify::deliver` is the Hard carrier.
    #[test]
    fn notification_enums_serialize_to_pinned_wire_strings() {
        assert_eq!(
            serde_json::to_value(NotificationLevel::Info).expect("NotificationLevel serializes"),
            "info"
        );
        assert_eq!(
            serde_json::to_value(NotificationLevel::Warning).expect("NotificationLevel serializes"),
            "warning"
        );
        assert_eq!(
            serde_json::to_value(NotificationLevel::Error).expect("NotificationLevel serializes"),
            "error"
        );
        assert_eq!(
            serde_json::to_value(NotificationLevel::default())
                .expect("NotificationLevel serializes"),
            "info"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::Desktop).expect("NotificationKind serializes"),
            "desktop"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::Email).expect("NotificationKind serializes"),
            "email"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::Slack).expect("NotificationKind serializes"),
            "slack"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::Telegram).expect("NotificationKind serializes"),
            "telegram"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::WeChatWork)
                .expect("NotificationKind serializes"),
            "weChatWork"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::Feishu).expect("NotificationKind serializes"),
            "feishu"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::DingTalk).expect("NotificationKind serializes"),
            "dingTalk"
        );
        assert_eq!(
            serde_json::to_value(NotificationKind::default()).expect("NotificationKind serializes"),
            "desktop"
        );
    }

    #[test]
    fn notification_delivery_payload_excludes_channel_secrets() {
        let payload = NotificationDeliveryPayload {
            notification: Notification::new(
                NotificationLevel::Info,
                "PR #7 review 完成".to_string(),
                "https://example.com/pr/7".to_string(),
                RedactedNotificationBody::action_url("https://example.com/pr/7".to_string()),
                "p1".to_string(),
            ),
            channel_id: "slack-main".to_string(),
            kind: NotificationKind::Slack,
        };

        let json = serde_json::to_string(&payload).expect("payload serializes");
        assert!(json.contains("slack-main"));
        assert!(!json.contains("webhook"));
        assert!(!json.contains("token"));
        assert!(!json.contains("password"));
        assert!(!json.contains("secret"));
    }

    // Cross-agent wire contract lock for the AB#1066 outbox status / kind (Medium carrier per
    // ai-robust.md): the frontend's `OUTBOX_STATUSES` / `ACTION_KINDS` (`src/types.ts`) mirror
    // these exact camelCase strings. A variant rename or a `rename_all` change surfaces here (the
    // exhaustive `match` in `outbox::store::status_as_wire` / `kind_as_wire` is the Hard carrier).
    // Defaults: `Pending` (the state every action starts in) / `Notification` (the only kind).
    #[test]
    fn outbox_status_and_kind_serialize_to_pinned_wire_strings() {
        assert_eq!(
            serde_json::to_value(ActionStatus::Pending).expect("ActionStatus serializes"),
            "pending"
        );
        assert_eq!(
            serde_json::to_value(ActionStatus::Done).expect("ActionStatus serializes"),
            "done"
        );
        assert_eq!(
            serde_json::to_value(ActionStatus::Dead).expect("ActionStatus serializes"),
            "dead"
        );
        assert_eq!(
            serde_json::to_value(ActionStatus::default()).expect("ActionStatus serializes"),
            "pending"
        );
        assert_eq!(
            serde_json::to_value(ActionKind::Notification).expect("ActionKind serializes"),
            "notification"
        );
        assert_eq!(
            serde_json::to_value(ActionKind::Review).expect("ActionKind serializes"),
            "review"
        );
        assert_eq!(
            serde_json::to_value(ActionKind::Check).expect("ActionKind serializes"),
            "check"
        );
        assert_eq!(
            serde_json::to_value(ActionKind::StopReview).expect("ActionKind serializes"),
            "stopReview"
        );
        assert_eq!(
            serde_json::to_value(ActionKind::default()).expect("ActionKind serializes"),
            "notification"
        );
    }

    // Front/back contract lock for the AB#1066 `OutboxEntry` row (Medium carrier per ai-robust.md):
    // mirrored in `src/types.ts` (`OutboxEntry`); a field change must be synced there in lockstep
    // (the open end of the funnel — future Hard path = codegen from `model.rs` + `git diff
    // --exit-code`). Locks camelCase keys present + snake_case absent, the nested `kind` / `status`
    // wire strings, and that an absent `lastError` serializes as JSON null (not omitted) so the TS
    // mirror's `lastError: string | null` stays closed. The raw payload is intentionally NOT a
    // field (fetched via `outbox_get_raw`), mirroring the inbox's raw-payload split.
    #[test]
    fn outbox_entry_wire_shape_is_camel_case() {
        let entry = OutboxEntry {
            id: 7,
            project_id: "p1".to_string(),
            kind: ActionKind::Notification,
            summary: "PR #7 review 完成".to_string(),
            status: ActionStatus::Pending,
            attempt_count: 2,
            next_attempt_at: 1_700_000_060,
            last_error: Some("notify failed".to_string()),
            created_at: 1_700_000_000,
            updated_at: 1_700_000_030,
        };

        let v = serde_json::to_value(&entry).expect("OutboxEntry serializes");

        // camelCase keys present.
        assert!(v.get("id").is_some());
        assert!(v.get("projectId").is_some());
        assert!(v.get("kind").is_some());
        assert!(v.get("summary").is_some());
        assert!(v.get("status").is_some());
        assert!(v.get("attemptCount").is_some());
        assert!(v.get("nextAttemptAt").is_some());
        assert!(v.get("lastError").is_some());
        assert!(v.get("createdAt").is_some());
        assert!(v.get("updatedAt").is_some());

        // snake_case forms absent — a rename of a multi-word field surfaces here.
        assert!(v.get("project_id").is_none());
        assert!(v.get("attempt_count").is_none());
        assert!(v.get("next_attempt_at").is_none());
        assert!(v.get("last_error").is_none());
        assert!(v.get("created_at").is_none());
        assert!(v.get("updated_at").is_none());

        // The nested enums serialize to their pinned wire strings.
        assert_eq!(v["kind"], "notification");
        assert_eq!(v["status"], "pending");

        // A never-failed action's `lastError` is JSON null (not omitted), keeping the TS mirror's
        // `lastError: string | null` a closed contract.
        let fresh = OutboxEntry {
            attempt_count: 0,
            last_error: None,
            status: ActionStatus::Done,
            ..entry
        };
        let fv = serde_json::to_value(&fresh).expect("OutboxEntry serializes");
        assert_eq!(fv["status"], "done");
        assert_eq!(fv["lastError"], serde_json::Value::Null);
    }

    // Wire lock for the AB#1069 review/check action payload (Medium carrier per ai-robust.md):
    // backend-internal (read by the lib.rs executor, produced by a future Rule Engine), so NOT
    // mirrored in `src/types.ts` — but persisted in the outbox `payload` column and replayed, so its
    // camelCase shape must stay stable. Round-trips (the executor deserializes it).
    #[test]
    fn review_action_payload_wire_shape_is_camel_case() {
        let payload = ReviewActionPayload {
            candidate: Candidate {
                number: 7,
                head_sha: "sha".to_string(),
                head_ref: "main".to_string(),
                author: "octocat".to_string(),
                is_cross_repository: false,
                is_draft: false,
                kind: "review".to_string(),
            },
        };
        let v = serde_json::to_value(&payload).expect("ReviewActionPayload serializes");
        // camelCase present, snake_case absent.
        assert!(v.get("candidate").is_some());
        assert_eq!(v["candidate"]["headSha"], "sha");
        assert!(v.get("prNumber").is_none());
        assert!(v["candidate"].get("head_sha").is_none());
        // F3: the routing key is the outbox ROW's project_id, NOT a payload field — assert it is
        // absent so a producer can't reintroduce a dual project source.
        assert!(v.get("projectId").is_none());
        assert!(v.get("project_id").is_none());
        // Round-trips — the executor reads it back from the stored payload.
        let back: ReviewActionPayload =
            serde_json::from_value(v).expect("ReviewActionPayload round-trips");
        assert_eq!(back, payload);
    }

    // Wire lock for the AB#1069 stop-review action payload (Medium carrier): same backend-internal,
    // persisted-and-replayed status as the review payload; carries the `(pr, kind)` session key (the
    // project is the outbox row's, AB#1069 F3). Round-trips.
    #[test]
    fn stop_review_action_payload_wire_shape_is_camel_case() {
        let payload = StopReviewActionPayload {
            pr_number: 7,
            kind: "review".to_string(),
        };
        let v = serde_json::to_value(&payload).expect("StopReviewActionPayload serializes");
        assert!(v.get("prNumber").is_some());
        assert!(v.get("kind").is_some());
        assert!(v.get("pr_number").is_none());
        // F3: routing key is the row's project_id, not a payload field.
        assert!(v.get("projectId").is_none());
        assert!(v.get("project_id").is_none());
        let back: StopReviewActionPayload =
            serde_json::from_value(v).expect("StopReviewActionPayload round-trips");
        assert_eq!(back, payload);
    }
}
