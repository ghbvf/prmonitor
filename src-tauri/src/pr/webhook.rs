//! Webhook PR-trigger source (#9): a local `POST /webhook` receiver exposed to the
//! public internet so GitHub pushes a PR event the instant a trigger label lands
//! instead of waiting for the poll interval.
//!
//! **Tunnel is decoupled from the receiver.** The receiver only ever binds
//! `127.0.0.1`, verifies the HMAC, parses, and dispatches; HOW the local port reaches
//! the public internet is a config choice ([`WebhookTunnelMode`]) the manager branches
//! on in [`WebhookManager::start`]:
//! - `quick` (default, unchanged): spawn a Cloudflare Quick Tunnel and scrape the
//!   `*.trycloudflare.com` URL.
//! - `command`: spawn a user-supplied tunnel command (`{port}` placeholder, exec'd
//!   directly — never via a shell); the public URL comes from config, not scraped.
//! - `listener`: bind only, spawn NO child; the tunnel is fully external; the public
//!   URL comes from config.
//!
//! **Push, not pull — so it does NOT implement [`super::source::EventSourceProvider`].** That
//! trait's `discover_events()` is pull-shaped (the scheduler asks `gh` for the current
//! list); a webhook is push-shaped (GitHub hands us one event). The handler maps a payload to a
//! [`WebhookEvent`] and hands it to the injected [`WebhookIngestor`] (the composition root's
//! upsert/emit + gate + durable action producer), reusing the entire vetted dispatch path with zero
//! duplication.
//!
//! **Multi-project routing (#35).** One global receiver / port / secret / tunnel
//! serves EVERY monitored project. The handler routes each verified event to the
//! enabled project whose `repo` matches the payload's repository (case-insensitive),
//! classifies review-vs-check with THAT project's labels, and dispatches under that
//! project's id. A payload whose repo matches no enabled project is dropped
//! (fail-closed — same spirit as the old single-repo ownership gate). The route list
//! ([`WebhookCtx::routes`]) is a SNAPSHOT taken at [`WebhookManager::start`] time from
//! the enabled projects; adding/removing/enabling a project requires a webhook restart
//! to refresh it (the composition root wires that restart on `set_config`).
//!
//! **Two providers, one receiver (AB#822).** The same endpoint serves both GitHub and
//! Azure DevOps. [`handle_webhook`] detects the provider by header — GitHub sends
//! `X-GitHub-Event`; an Azure Service Hook does not — and branches to
//! [`handle_github_delivery`] (HMAC over the body → [`parse_delivery`]) or
//! [`handle_azure_delivery`] (a `{ eventType, resource }` payload). The GitHub path ingests
//! the payload into a [`WebhookEvent`]; the Azure path is a REFRESH SIGNAL — Azure PR Service
//! Hooks carry no labels and don't fire on label changes (only push/status/reviewer/vote), so
//! [`route_azure_delivery`] only routes the event to a `project_id` and the injected
//! [`WebhookRefresher`] re-runs `az` discovery to read the authoritative labels. Routing is
//! provider-isolated: a route carries its [`ProjectRoute::source_kind`], so an Azure event
//! (bare repo name) can never match a GitHub `owner/name` route and vice-versa.
//!
//! **Layering.** The axum handler is runtime-agnostic — it never names
//! `AppHandle<R>`. The list upsert + `prs:updated` emit (#61) and the static/cooldown
//! gates (parity with the poll path) live in the [`WebhookIngestor`]
//! closure the root installs via [`WebhookManager::set_ingestor`] (which holds the
//! concrete app handle), exactly as [`super::scheduler::Scheduler`] does. The handler's
//! only job is verify → parse → route → hand off (and record the early-exit delivery
//! diagnostics, #62).
//!
//! **Security.** The endpoint is public (via the tunnel), so every request is
//! authenticated before it can act: a GitHub delivery is HMAC-verified
//! (`X-Hub-Signature-256`) against the configured secret before the body is even parsed;
//! an Azure delivery (no body HMAC) is verified by a constant-time compare of its
//! `Authorization: Bearer <secret>` header against the SAME `webhookSecret` (AB#822). An
//! unverified or secret-less request on either path is rejected (fail-closed). The local
//! server binds `127.0.0.1` only — the raw port is never world-reachable, only the
//! cloudflared tunnel is. The app registers NO webhook in GitHub or Azure (that would be a
//! write, breaking the app's read-only CLI surface) — the user pastes the tunnel URL +
//! secret into the repo's GitHub webhook / Azure Service Hook settings by hand.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::Router;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use tauri::async_runtime::{spawn, JoinHandle};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

use subtle::ConstantTimeEq;

use super::ledger;
use crate::config::service::ResolvedCli;
use crate::error::{AppError, AppResult};
use crate::model::{
    Candidate, EventEnvelope, EventSubject, EventType, InboxDedupeKey, LabelSource, ReviewKind,
    SourceKind, WebhookTunnelMode,
};

type HmacSha256 = Hmac<Sha256>;

/// The webhook ingest hook the composition root installs. Called with one parsed, routed
/// [`WebhookEvent`]; its body (the root's `ingest_webhook` wrapper in `commands.rs`)
/// upserts the persisted PR list, emits `prs:updated`, and returns the gated candidate for rule
/// processing — keeping the axum handler runtime-agnostic (the closure holds the concrete
/// `AppHandle<R>`, the handler never names it). Boxed-future + `Arc` so it is `Clone`able
/// into the `WebhookCtx` the handler shares. Installed once as a closure and Arc-shared —
/// the same lifecycle convention as the scheduler event sink (their
/// signatures differ: this takes a [`WebhookEvent`], the event sink takes
/// `(String, Vec<Candidate>)`).
///
/// **AB#1065 seam widening (single seam, no dual path).** The signature carries
/// `(raw: String, guid: Option<String>, WebhookEvent)`: the verbatim delivery body and the
/// `X-GitHub-Delivery` GUID the inbox needs to persist + dedup the delivery, alongside the
/// already-parsed event the dispatch path consumes. The composition root installs ONE
/// closure (the inbox `ingest_github`) here — it persists/dedups, then re-feeds the SAME
/// `WebhookEvent` through `pr::commands::ingest_webhook`. The `WebhookEvent` struct itself is
/// unchanged; the Routable arm calls this one seam (there is no second parallel ingestor).
///
/// **Returns [`AppResult`] = DURABLE PERSIST success (AB#1065 F1).** The handler AWAITS this and
/// gates the HTTP ACK on it: `Ok` → 200 (the delivery is durably persisted), `Err` → 500 (the
/// platform retries, so no delivery is lost between ACK and the SQLite commit). The closure
/// returns after the durable insert and wakes the single inbox worker for post-ACK processing;
/// the persist gates the ACK, the processing does not.
pub type WebhookIngestor = Arc<
    dyn Fn(
            String,
            Option<String>,
            WebhookEvent,
        ) -> Pin<Box<dyn Future<Output = AppResult<()>> + Send>>
        + Send
        + Sync,
>;

/// The Azure refresh hook the composition root installs (AB#822). Called with one routed
/// `project_id` when an Azure DevOps Service Hook PR event arrives. Azure PR Service Hooks
/// carry NO labels and do NOT fire on label changes (only push/status/reviewer/vote — see
/// [`route_azure_delivery`]), so the payload can't classify review-vs-check; the event is a
/// REFRESH SIGNAL. The closure (the root wires it to `scheduler::SchedulerSet::discover_once`)
/// re-runs `az repos pr list` discovery for that project — reading the AUTHORITATIVE current
/// labels — and upserts + dispatches via the same poll path. Same `Arc<dyn Fn>` lifecycle as
/// [`WebhookIngestor`]; keeps the axum handler runtime-agnostic.
///
/// **AB#1065 seam widening.** The signature carries `(raw: String, project_id: String,
/// repo: String)`: the verbatim delivery body + the routed identity the inbox needs to
/// persist an Azure audit entry, alongside the `project_id` the existing refresh consumes.
/// The composition root installs ONE closure (the inbox `ingest_azure_refresh`) here — it
/// persists an audit entry, then invokes the SAME `az` re-discovery it holds internally. The
/// Azure refresh arm calls this one seam (no second parallel hook).
///
/// **Returns [`AppResult`] = DURABLE PERSIST success (AB#1065 F1).** Like [`WebhookIngestor`], the
/// handler awaits this and gates the ACK on it (`Err` → 500 → platform retry). The audit-entry
/// persist gates the ACK; the spawned `az` re-discovery runs post-ACK.
pub type WebhookRefresher = Arc<
    dyn Fn(String, String, String) -> Pin<Box<dyn Future<Output = AppResult<()>> + Send>>
        + Send
        + Sync,
>;

/// Cap on the retained webhook-delivery diagnostics ring (#62). Old deliveries are
/// popped from the front once the buffer is full — the panel only ever needs a recent
/// window for "did GitHub reach us, and what did we do with it".
const DELIVERY_RING_CAP: usize = 50;

/// One terminal classification of a received webhook delivery (#62), recorded EXACTLY
/// once per request so the settings panel can diagnose "GitHub posted but nothing
/// happened" without new event types (pulled via the `webhook_deliveries` command).
///
/// camelCase wire enum mirrored in `src/pr/types.ts` (Medium carrier per
/// `.claude/rules/prmonitor/ai-robust.md`; a `webhook_delivery_wire_shape_*` golden
/// test pins the strings + key shape so a rename can't silently drift the TS mirror).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DeliveryStatus {
    /// HMAC verification failed (401) — wrong/absent signature.
    Unauthorized,
    /// Body did not parse as JSON, or a `pull_request` payload was malformed.
    BadPayload,
    /// A non-`pull_request` event (e.g. GitHub's `ping`) — acknowledged, no action.
    Ignored,
    /// Verified but the event's repo matches no enabled project's route (fail-closed).
    WrongRepo,
    /// Open PR carrying neither trigger label (label removed) — list updated, no rule action.
    NoTriggerLabel,
    /// PR not open (closed/merged) — list updated to reflect it, never sent to rules.
    NotOpen,
    /// A single trigger label, but a gate (conflict / draft / fork / author / cooldown)
    /// blocked rule processing — list updated, review NOT auto-started.
    Gated,
    /// A clean candidate entered the list; the rule engine decides follow-up actions.
    ListUpdated,
    /// An Azure DevOps PR Service Hook (created/updated) was received and triggered a
    /// re-discovery (AB#822). Azure hooks carry no labels and don't fire on label changes, so
    /// the webhook is only a refresh signal — the actual list-update / rule outcome is
    /// recorded by the poll path's `PollStatus` (via `discover_once`), not here.
    Refreshed,
}

/// One recorded webhook delivery diagnostic (#62). MUST NOT carry the secret/token or
/// the raw signature — only the routing + classification metadata the panel renders.
///
/// camelCase wire type mirrored in `src/pr/types.ts` (Medium carrier per
/// `.claude/rules/prmonitor/ai-robust.md`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookDelivery {
    /// When the request was received (epoch secs, from [`ledger::now_epoch`]).
    pub received_at_epoch: u64,
    /// The `X-GitHub-Event` header value (e.g. `"pull_request"`, `"ping"`, or `""`).
    pub event: String,
    /// The PR `action` (`"labeled"` / `"opened"` / …) when parseable; else `None`.
    pub action: Option<String>,
    /// The event repo `owner/name` when known; else `None`.
    pub repo: Option<String>,
    /// The PR number when known; else `None`.
    pub pr_number: Option<u64>,
    /// The classified turn kind (`"review"` / `"check"`) when applicable; else `None`.
    pub kind: Option<String>,
    /// The terminal classification of this delivery.
    pub status: DeliveryStatus,
    /// A short human-readable (Chinese) note (skip reason / error); `None` → null.
    pub message: Option<String>,
}

/// Outcome of parsing a verified `pull_request` payload against the route snapshot
/// (#61). Pure (no `AppHandle`) so the whole classification is unit-tested; the
/// `AppHandle`-bound ingest in `commands.rs` consumes it. The handler maps each
/// non-`Routable` arm to a terminal [`DeliveryStatus`] inline; `Routable` is handed to
/// `ingest_webhook`, which records the terminal status from the [`IngestIntent`].
#[derive(Debug)]
pub enum ParseResult {
    /// The event routes to an enabled project and parsed cleanly — proceed to ingest.
    /// `Box`ed because `WebhookEvent` is much larger than the other variants
    /// (`clippy::large_enum_variant`) and is always heap-handed to the ingestor anyway.
    Routable(Box<WebhookEvent>),
    /// Verified, but the event's repo matched no enabled route (fail-closed drop). The
    /// repo (when known) is carried for the delivery diagnostic.
    WrongRepo { repo: Option<String> },
    /// No `pull_request`, or a required field (number / head.sha / head.ref / labels) is
    /// missing or structurally invalid (`labels` not an array — F3).
    Malformed,
}

/// One parsed + routed webhook event (#61): the metadata the ingest needs to upsert the
/// PR list row, emit `prs:updated`, and (when the intent yields a candidate) dispatch.
/// Built purely by [`parse_delivery`]; consumed by `commands::ingest_webhook`.
///
/// `Serialize` + `Deserialize` (AB#1065): the inbox persists the parsed event as JSON in
/// `inbox_event.webhook_event_json` so a GitHub replay re-feeds the SAME classified event
/// through `ingest_webhook` WITHOUT re-running the route-dependent [`parse_delivery`] (which
/// needs the live route snapshot, unavailable at replay time).
///
/// **PERSISTED REPLAY CONTRACT (AB#1065 F3).** Because `webhook_event_json` lives in long-term
/// SQLite, this struct's serde shape is a durable contract: a stored row from an OLD app version
/// must still deserialize on a NEW one, so a field rename / removal would silently break replay of
/// already-persisted deliveries. The serde golden `webhook_event_replay_json_round_trips_and_shape_is_pinned`
/// (Medium carrier) locks the field/key set + round-trip. (Possible future STRUCTURAL follow-up:
/// relocate `WebhookEvent` to `model.rs` as a versioned cross-slice contract; out of scope here —
/// the proportionate fix is the golden lock.)
#[derive(Debug, Serialize, Deserialize)]
pub struct WebhookEvent {
    /// The routing key (the matched [`ProjectRoute::id`]).
    pub project_id: String,
    /// When the request was received (epoch secs), stamped by the handler so the routable
    /// path's terminal delivery diagnostic uses the SAME receipt time as the early-exit
    /// records (not the later record-time). The pure parsers leave this `0`; `handle_webhook`
    /// overwrites it on the `Routable` arm before handing the event to the ingestor.
    pub received_at: u64,
    /// The wire event type for the delivery diagnostic — always `"pull_request"` (only the
    /// GitHub path produces a `WebhookEvent`; the Azure path is a refresh signal that never
    /// builds one, see [`route_azure_delivery`]). Carried on the event so
    /// `commands::ingest_webhook` records it without a hardcoded literal.
    pub event: String,
    /// The PR `action` (for the delivery diagnostic): GitHub's `action` (`"labeled"`/…).
    pub action: Option<String>,
    /// The matched repo `owner/name` (for the delivery diagnostic).
    pub repo: String,
    /// The PR number.
    pub number: u64,
    /// The PR title (`""` when absent).
    pub title: String,
    /// The PR's current label names.
    pub labels: Vec<String>,
    /// The PR's HTML URL (`""` when absent).
    pub url: String,
    /// What to do with this event: track (with an optional dispatch candidate) or
    /// update the list's status only.
    pub intent: IngestIntent,
}

/// Normalize a GitHub [`WebhookEvent`] into the cross-slice [`Event`] envelope (AB#1065). Lives
/// HERE (not in the `inbox` slice) because `pr` OWNS `WebhookEvent` — the inbox depends only on the
/// neutral [`crate::model::Event`], never on this pr-internal type. The composition root calls this
/// at the webhook seam and hands the resulting `Event` to `inbox::service::ingest_github`.
///
/// PURE (no `AppHandle`); the only IO is the `now` fallback clock. The event class is always
/// [`EventType::PullRequest`] (the only class the GitHub webhook path emits today); `dedupe_key` is
/// [`event_dedupe_key`]; the display fields mirror the `WebhookEvent`; `body` is `""` (the PR
/// webhook carries no body the inbox needs); `received_at_epoch` is the handler-stamped receipt
/// time, falling back to `now` when the pure parser left it `0`.
pub(crate) fn event_from_webhook(
    ev: &WebhookEvent,
    guid: Option<&str>,
    raw: &str,
) -> EventEnvelope {
    EventEnvelope::observation(
        InboxDedupeKey::new(event_dedupe_key(guid, raw)).expect("webhook dedupe key is non-empty"),
        SourceKind::Github,
        ev.project_id.clone(),
        ev.repo.clone(),
        EventType::PullRequest,
        EventSubject {
            number: Some(ev.number),
            title: ev.title.clone(),
            body: String::new(),
            labels: ev.labels.clone(),
            url: ev.url.clone(),
        },
        if ev.received_at > 0 {
            ev.received_at
        } else {
            ledger::now_epoch()
        },
    )
    .expect("routed webhook event has a project id")
}

/// The inbox dedupe key for a GitHub delivery (AB#1065): `github:{guid}` when the
/// `X-GitHub-Delivery` GUID is present (GitHub's own per-delivery identity — a retry of the SAME
/// delivery carries the SAME guid), else a body-hash fallback `github:sha256:{hash}` so a
/// guid-less delivery (a malformed / replayed request) still dedups on identical content. Pure.
fn event_dedupe_key(guid: Option<&str>, raw: &str) -> String {
    match guid {
        Some(g) if !g.is_empty() => format!("github:{g}"),
        _ => format!("github:sha256:{}", sha256_hex(raw.as_bytes())),
    }
}

/// Lowercase hex SHA-256 of `bytes` — the GitHub guid-less dedupe-key fallback. Reuses the `sha2`
/// crate already pulled in for the HMAC verify (no new dependency).
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// What an ingest should do with a [`WebhookEvent`] (#61).
#[derive(Debug, Serialize, Deserialize)]
pub enum IngestIntent {
    /// An OPEN PR with a clean single trigger label (`candidate: Some`) — upsert + maybe
    /// dispatch — or BOTH trigger labels (`conflict: true`, `candidate: None`) — upsert as
    /// a skipped row, never dispatch (mirrors the poll path's conflict drop).
    Track {
        candidate: Option<Candidate>,
        conflict: bool,
    },
    /// The PR should appear in the list with a skip reason but never dispatch: a
    /// closed/merged PR, or an open PR whose trigger label was removed. The ingest
    /// refreshes an EXISTING row's status (no insert). The [`StatusOnlyKind`]
    /// SINGLE-SOURCES both the skip-reason text and the terminal [`DeliveryStatus`].
    StatusOnly { kind: StatusOnlyKind },
}

/// Which status-only outcome a non-dispatch [`IngestIntent::StatusOnly`] is (#61). The
/// Hard carrier (sealed enum) for the reason-text ↔ delivery-status pairing per
/// `.claude/rules/prmonitor/ai-robust.md`: it SINGLE-SOURCES both the human-readable
/// skip reason and the terminal [`DeliveryStatus`], so the ingest can no longer derive
/// the status from a free-form string compare (the old `if reason == "PR 已关闭或合并"`
/// cross-function string protocol — fragile, Soft). A new status-only case must add a
/// variant here and the compiler forces both [`Self::reason`] and
/// [`Self::delivery_status`] to handle it, making a reason/status mismatch unexpressible.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum StatusOnlyKind {
    /// The PR is not open (closed / merged) — list row reflects it, never dispatched.
    ClosedOrMerged,
    /// An OPEN PR carrying neither trigger label (label removed) — list-only, no dispatch.
    TriggerLabelRemoved,
}

impl StatusOnlyKind {
    /// The human-readable (Chinese) skip reason for this status-only outcome — the row's
    /// `skip_reason` and the delivery diagnostic's `message`. Single-sourced here so the
    /// text can't drift between `parse_delivery` and `ingest_webhook`.
    pub fn reason(self) -> &'static str {
        match self {
            StatusOnlyKind::ClosedOrMerged => "PR 已关闭或合并",
            StatusOnlyKind::TriggerLabelRemoved => "触发 label 已移除",
        }
    }

    /// The terminal [`DeliveryStatus`] for this status-only outcome. Single-sourced here
    /// so `ingest_webhook` maps it by type, never by re-comparing the reason string.
    pub fn delivery_status(self) -> DeliveryStatus {
        match self {
            StatusOnlyKind::ClosedOrMerged => DeliveryStatus::NotOpen,
            StatusOnlyKind::TriggerLabelRemoved => DeliveryStatus::NoTriggerLabel,
        }
    }
}

/// cloudflared prints the assigned Quick Tunnel URL to stderr within a few seconds;
/// cap the wait so a stuck binary can't hang `start_webhook`.
const TUNNEL_URL_TIMEOUT: Duration = Duration::from_secs(20);
/// `cloudflared --version` probe budget (the install check).
const CLOUDFLARED_VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// Bind attempts (and inter-attempt delay) for the receiver socket, so a rapid restart
/// can ride out the brief window where the OS hasn't yet released the prior listener's
/// port (F3) instead of failing on a transient "address in use".
const BIND_RETRIES: u32 = 10;
const BIND_RETRY_DELAY: Duration = Duration::from_millis(20);
/// `(tunnel child, drain task, shared public-URL handle)` — the per-mode result of
/// resolving the tunnel half of a [`WebhookManager::start`]. Aliased to keep that `let`
/// readable and satisfy `clippy::type_complexity`.
type TunnelParts = (
    Option<Child>,
    Option<JoinHandle<()>>,
    Arc<StdMutex<Option<String>>>,
);

/// The single path the axum receiver serves — also the suffix appended to the tunnel
/// root to form the GitHub "Payload URL". ONE source for both the route registration
/// and the URL the UI tells the user to paste; they must match or every delivery 404s.
const WEBHOOK_PATH: &str = "/webhook";

/// The two Azure DevOps Service Hook `eventType`s this receiver acts on (AB#822): a PR being
/// opened and any subsequent update (label add/remove, push, status change). Single-sourced
/// here so the handler's gate and the `route_azure_delivery` doc reference one spelling.
const AZURE_PR_CREATED: &str = "git.pullrequest.created";
const AZURE_PR_UPDATED: &str = "git.pullrequest.updated";

/// webhook receiver + tunnel status reported to the frontend (pr-slice-private wire
/// type; not a cross-slice contract, so it is mirrored in `src/pr/types.ts`, not
/// `model.rs` / `src/types.ts` — same placement as [`super::gh::GhStatus`]).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookStatus {
    /// Whether the local receiver + tunnel are currently running.
    pub running: bool,
    /// The public `https://*.trycloudflare.com` tunnel ROOT, when resolved. `None`
    /// while stopped or if the URL didn't appear within the timeout. This is the
    /// tunnel itself, NOT the value to paste into GitHub — see [`Self::payload_url`].
    pub public_url: Option<String>,
    /// The full GitHub "Payload URL" = [`Self::public_url`] + [`WEBHOOK_PATH`], the
    /// ONLY route the receiver serves. The UI shows/copies THIS (pasting `public_url`
    /// alone 404s every delivery). Derived in [`Self::new`] so it can't drift.
    pub payload_url: Option<String>,
    /// Whether `cloudflared` is runnable (so the UI can prompt to install it).
    pub cloudflared_installed: bool,
    /// Human-readable (Chinese) status line for the UI.
    pub message: String,
}

impl WebhookStatus {
    /// Build a status, deriving [`Self::payload_url`] from `public_url` + the route
    /// the receiver serves ([`WEBHOOK_PATH`]) as the SINGLE source — a route rename
    /// forces this format to follow (locked by `webhook_status_wire_shape_*`), so the
    /// pasted URL and the served path can never disagree. The ONLY constructor, so no
    /// caller can build a status whose `payload_url` drifts from `public_url`.
    fn new(
        running: bool,
        public_url: Option<String>,
        cloudflared_installed: bool,
        message: String,
    ) -> Self {
        // Trim a trailing '/' on the URL root before appending the route so a user-entered
        // `https://host/` (command/listener mode) yields `https://host/webhook`, not
        // `https://host//webhook` (F7). Quick-mode scraped URLs carry no trailing slash, so
        // this is a no-op there.
        let payload_url = public_url
            .as_ref()
            .map(|u| format!("{}{WEBHOOK_PATH}", u.trim_end_matches('/')));
        Self {
            running,
            public_url,
            payload_url,
            cloudflared_installed,
            message,
        }
    }
}

/// How the receiver's local port reaches the public internet — the tunnel half of a
/// [`WebhookManager::start`] call, grouped so the receiver params (port / secret /
/// labels / cloudflared credential) and the tunnel params don't blur into one flat arg list.
/// All three fields come straight off `AppConfig` (`webhook_tunnel_mode` /
/// `webhook_tunnel_command` / `webhook_public_url`); `start` branches on `mode`:
/// `command` reads `command`, `command`/`listener` read `public_url` (`quick` ignores
/// both and scrapes the URL from cloudflared).
pub struct TunnelSpec {
    pub mode: WebhookTunnelMode,
    /// The `command`-mode tunnel command (`{port}` placeholder; whitespace-split; exec'd
    /// directly, no shell). Ignored by `quick` / `listener`.
    pub command: String,
    /// The `command`/`listener`-mode public URL root (empty → `None`). Ignored by `quick`
    /// (which scrapes the `*.trycloudflare.com` URL instead).
    pub public_url: String,
}

/// One enabled project's webhook routing info (#35) — the minimal slice of
/// [`crate::config::model::Project`] the handler needs to route a push event:
/// match the event repo, classify review-vs-check by THIS project's labels, and
/// dispatch under THIS project's id. The composition root builds the list from
/// every `enabled` project at [`WebhookManager::start`] time (see [`WebhookCtx::routes`]);
/// it does NOT carry the full `Project` so the `pr` slice stays decoupled from the
/// config slice's domain model (parity with how `start` takes flat receiver params).
pub struct ProjectRoute {
    /// The routing key emitted on the dispatched candidate (`Project::id`).
    pub id: String,
    /// Which provider this project is — GitHub or Azure DevOps (AB#822). Routing is
    /// provider-isolated: [`parse_delivery`] only matches `Github` routes, and
    /// [`route_azure_delivery`] only `Azure` routes, so a bare Azure repo name can never
    /// collide with a GitHub `owner/name` (and vice-versa).
    pub source_kind: SourceKind,
    /// The monitored repo matched (case-insensitively) against the event's repository —
    /// `owner/name` for GitHub, the bare repository name for Azure.
    pub repo: String,
    /// The Azure DevOps project (AB#822) — matched against the event's
    /// `resource.repository.project.name` as a routing guard so an Azure event for a
    /// coincidentally same-named repo in a DIFFERENT project doesn't refresh this one
    /// (config already rejects duplicate bare repo names, so repo is globally unique; this
    /// is the extra provider-scoped guard). Empty for GitHub projects.
    pub azure_project: String,
    /// Where this project's trigger labels come from (AB#717). The GitHub path
    /// ([`parse_delivery`]) resolves effective labels (native vs title-parsed) from the
    /// payload via this before classifying, so a `title`-source project classifies a
    /// webhook PR by its title tags — parity with the poll path. The Azure path re-runs
    /// `az` discovery (which already honors it), so it does NOT read this.
    pub label_source: LabelSource,
}

/// Owns the running receiver + tunnel. `&self` methods + interior mutability so it
/// lives in `AppState` (which stays `Default`), mirroring `Scheduler`/`CodexManager`.
#[derive(Default)]
pub struct WebhookManager {
    /// Installed once by the composition root (lib.rs) BEFORE any start, like the
    /// scheduler's event sink. The closure is called with one
    /// parsed, routed [`WebhookEvent`] (#61); its body upserts the persisted PR list,
    /// emits `prs:updated`, and (when gated-clean) dispatches — keeping the axum handler
    /// runtime-agnostic (the closure holds the concrete `AppHandle<R>`).
    ingestor: StdMutex<Option<WebhookIngestor>>,
    /// Installed once by the composition root alongside `ingestor` (AB#822). The Azure
    /// refresh hook — called with a routed `project_id` to re-discover via `az` (see
    /// [`WebhookRefresher`]). Keeps the handler runtime-agnostic, same as `ingestor`.
    refresher: StdMutex<Option<WebhookRefresher>>,
    runtime: StdMutex<Option<WebhookRuntime>>,
    /// Serializes `start` (bind + spawn + tunnel-URL await) so concurrent starts
    /// can't double-bind the port.
    start_lock: tokio::sync::Mutex<()>,
    /// The webhook-delivery diagnostics ring (#62), capped at [`DELIVERY_RING_CAP`].
    /// Recorded EXACTLY once per request (by the handler for early exits, by
    /// `ingest_webhook` for the routable terminal status) and read by the
    /// `webhook_deliveries` command. A process-shared `StdMutex<VecDeque<_>>` so it
    /// survives start/stop cycles (a stop tears down the runtime, not the diagnostics),
    /// keeping `#[derive(Default)]` (an empty ring).
    deliveries: Arc<StdMutex<VecDeque<WebhookDelivery>>>,
}

/// The live receiver + (optional) tunnel handles. [`Self::teardown`] aborts
/// `server_task` and (if present) `drain_task` (explicit — neither is aborted by
/// `Drop`), then explicitly `start_kill`s the tunnel child and reaps it on a detached
/// task (mirroring `engines/codex/process.rs::kill_and_reap`) so it can't linger as a
/// zombie; `kill_on_drop(true)` stays as the drop backstop. `status` reaps a dead
/// `tunnel` via `try_wait` to self-heal a crashed tunnel.
///
/// `tunnel` / `drain_task` are `Option` because the `listener` mode spawns NO child
/// process (the tunnel is fully external) — both are `None` there, and `teardown` /
/// `status` treat `None` as "nothing to abort / always-running" (no crash self-heal
/// applies when there's no child to crash).
struct WebhookRuntime {
    server_task: JoinHandle<()>,
    /// Fires axum's graceful shutdown so teardown can AWAIT the server task's end and be
    /// sure the listener is dropped (port freed) before returning — what makes an
    /// immediate same-port re-bind safe (F3). Sending consumes it, hence it lives on the
    /// owned-`self` teardown methods.
    shutdown: oneshot::Sender<()>,
    /// stderr-drain task for the tunnel child (keeps the pipe from filling). In `quick`
    /// mode it ALSO keeps scanning for a late `*.trycloudflare.com` URL and writes it
    /// into [`Self::public_url`] (so a URL printed after the initial scan window is still
    /// captured — F4). Aborted in teardown for lifecycle symmetry rather than relying on
    /// the child-kill → pipe-EOF chain. `None` in `listener` mode (no child to drain).
    drain_task: Option<JoinHandle<()>>,
    /// The tunnel child (cloudflared in `quick` mode, the user command in `command`
    /// mode). Kept to keep the tunnel alive (and `kill_on_drop` it on drop), and probed
    /// by `status` via `try_wait` to detect a crashed tunnel. `None` in `listener` mode
    /// (tunnel external — no child).
    tunnel: Option<Child>,
    /// The resolved public URL root, SHARED with `drain_task` (`quick` mode fills it in
    /// when cloudflared prints the URL — possibly after `start` returned, F4). `status`
    /// reads the live value through this handle, so a late URL surfaces on the next
    /// status poll. For `command`/`listener` the URL is from config and never changes.
    public_url: Arc<StdMutex<Option<String>>>,
    /// The tunnel mode this runtime was started with (Copy off `TunnelSpec`). Held so
    /// `status` is mode-aware: only `Quick` owns/depends on cloudflared, so only `Quick`
    /// runs the install probe + renders cloudflared-specific crash/install text.
    mode: WebhookTunnelMode,
    cloudflared_fingerprint: Option<String>,
}

impl WebhookRuntime {
    /// Sync best-effort teardown: abort the server + (if any) drain task, then
    /// `start_kill` the tunnel child and reap it on a DETACHED task (mirroring
    /// `engines/codex/process.rs::kill_and_reap`). Synchronous so it is safe from the
    /// app-exit `RunEvent` handler ([`WebhookManager::shutdown`]) and `status`'s crash
    /// self-heal, where no `.await` is available; it does NOT wait for the server task
    /// to drop the listener. Use [`Self::teardown_awaiting`] on any path that re-binds
    /// the port (restart / explicit stop). `kill_on_drop(true)` + OS exit reaping are
    /// the backstops if the detached reaper can't run.
    fn teardown(self) {
        // Signal graceful shutdown, then `abort` as the immediate backstop (this sync
        // path can't await the server to fully stop). It never re-binds the port (app
        // exit / crash self-heal), so it needn't wait for the listener to drop.
        let _ = self.shutdown.send(());
        self.server_task.abort();
        if let Some(drain) = self.drain_task {
            drain.abort();
        }
        if let Some(mut child) = self.tunnel {
            let _ = child.start_kill(); // immediate SIGKILL — synchronous.
            tauri::async_runtime::spawn(async move {
                // Move `child` in so it (and its pipes) live until reaped.
                let _ = child.wait().await;
            });
        }
    }

    /// Async teardown that AWAITS the server task's end before returning, so the bound
    /// `127.0.0.1:port` is actually released (the listener is owned by `server_task`;
    /// `abort()` only schedules cancellation). Used by the restart path in
    /// [`WebhookManager::start`] and by [`WebhookManager::stop`] so an immediate re-bind
    /// on the same port can't hit `address in use` (F3). Also reaps the tunnel child
    /// inline (await `wait` after `start_kill`) rather than detaching, since this path
    /// can wait.
    async fn teardown_awaiting(self) {
        // Graceful shutdown + AWAIT: axum stops accepting and drops the listener, and the
        // await returns only after the server task has fully ended — so the port is
        // released before we return (a bare `abort` does not reliably drop the listener
        // before the next `bind`). Webhook handlers are sub-second, so this completes fast.
        let _ = self.shutdown.send(());
        let _ = self.server_task.await;
        if let Some(drain) = self.drain_task {
            drain.abort();
            let _ = drain.await;
        }
        if let Some(mut child) = self.tunnel {
            let _ = child.start_kill();
            let _ = child.wait().await; // reap inline (no zombie, no detached task).
        }
    }
}

impl WebhookManager {
    /// Install the ingest hook (composition root, before any start). Replaces any prior
    /// hook (last writer wins).
    pub fn set_ingestor(&self, i: WebhookIngestor) {
        *self.ingestor.lock().unwrap() = Some(i);
    }

    /// Install the Azure refresh hook (AB#822; composition root, before any start). Replaces
    /// any prior hook (last writer wins). Mirror of [`Self::set_ingestor`].
    pub fn set_refresher(&self, r: WebhookRefresher) {
        *self.refresher.lock().unwrap() = Some(r);
    }

    /// Record one webhook-delivery diagnostic (#62), popping the oldest when the ring is
    /// full ([`DELIVERY_RING_CAP`]). Called EXACTLY once per request — by the handler for
    /// the early-exit classifications, by `ingest_webhook` for the routable terminal
    /// status. Sync + interior-mutable so it is callable from the runtime-agnostic
    /// handler and from `ingest_webhook` alike (via `AppState`).
    pub fn record_delivery(&self, d: WebhookDelivery) {
        // Delegate to the free `record_into` so the ring-push (cap + pop-front) lives in
        // ONE implementation — the handler records via `record_into(&ctx.deliveries, ..)`,
        // this records via the manager's own handle, both through the same body.
        record_into(&self.deliveries, d);
    }

    /// Snapshot the delivery ring oldest→newest (#62) for the `webhook_deliveries`
    /// command. A clone so the lock is released before the caller serializes.
    pub fn deliveries_snapshot(&self) -> Vec<WebhookDelivery> {
        self.deliveries.lock().unwrap().iter().cloned().collect()
    }

    /// Start (or restart) the local receiver + (per-`mode`) tunnel. Tears down any prior
    /// runtime first (so a config change re-binds cleanly). Returns the resolved status.
    ///
    /// Branches on [`WebhookTunnelMode`]:
    /// - `Quick` (unchanged): require `cloudflared` (a missing binary short-circuits to
    ///   `running: false` + `cloudflared_installed: false` rather than an error, so the
    ///   UI can prompt to install it), bind, spawn the Quick Tunnel, scrape the
    ///   `*.trycloudflare.com` URL.
    /// - `Command`: bind, spawn `tunnel_command` (split on whitespace, `{port}` →
    ///   actual port, exec'd directly — no shell), `public_url` from config (`None` if
    ///   `public_url` is empty). Does NOT require cloudflared.
    /// - `Listener`: bind only, spawn no child; `public_url` from config. Does NOT
    ///   require cloudflared.
    ///
    /// `routes` (#35) is the SNAPSHOT of enabled projects' routing info the handler
    /// matches each event against (the composition root builds it from every `enabled`
    /// [`crate::config::model::Project`] at this call). It is captured into
    /// [`WebhookCtx::routes`] and never refreshed for the life of the runtime — a
    /// project add/remove/enable change requires a restart (the root wires that on
    /// `set_config`). The receiver params (port / secret / cloudflared credential) stay GLOBAL
    /// (one receiver serves all projects).
    pub async fn start(
        &self,
        port: u16,
        secret: String,
        routes: Vec<ProjectRoute>,
        cloudflared: AppResult<Option<ResolvedCli>>,
        tunnel: TunnelSpec,
    ) -> AppResult<WebhookStatus> {
        let TunnelSpec {
            mode,
            command: tunnel_command,
            public_url,
        } = tunnel;
        let _guard = self.start_lock.lock().await;
        // Only Quick consumes cloudflared. Preserve the resolver's original error for
        // that mode; command/listener must remain independent of cloudflared settings.
        let cloudflared = if mode == WebhookTunnelMode::Quick {
            cloudflared?
        } else {
            None
        };
        // Restart: tear down any prior runtime and AWAIT the old server task's end so the
        // bound port is actually released before we re-bind below. A sync abort (the old
        // `stop_inner`) only schedules cancellation, racing the re-bind into a transient
        // "address in use" (F3). Holding `start_lock` across the whole start also closes
        // the F1 window: `stop`/`shutdown` issued mid-start can't interleave (see `stop`).
        // Take out of the std mutex BEFORE awaiting (never hold a `StdMutex` guard across
        // an `.await` — it would make the command future non-`Send`).
        let prior = self.runtime.lock().unwrap().take();
        if let Some(rt) = prior {
            rt.teardown_awaiting().await;
        }

        // Only the `quick` mode owns/depends on cloudflared; check it up front there so a
        // missing binary short-circuits BEFORE we bind. `command` / `listener` never
        // touch cloudflared (their tunnel is the user command / fully external).
        let cloudflared_available = match cloudflared.as_ref() {
            Some(cli) if mode == WebhookTunnelMode::Quick => cloudflared_installed(cli).await,
            Some(_) => true,
            None => mode != WebhookTunnelMode::Quick,
        };
        if !cloudflared_available {
            return Ok(WebhookStatus::new(
                false,
                None,
                false,
                "未找到 cloudflared，请先安装：brew install cloudflared".to_string(),
            ));
        }

        let ingestor = self
            .ingestor
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| AppError::new("webhook ingestor 未初始化".to_string()))?;
        let refresher = self
            .refresher
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| AppError::new("webhook refresher 未初始化".to_string()))?;

        // LOCAL bind only — the public path is the tunnel; the raw port is never
        // world-reachable. Shared by all three modes.
        //
        // Bind with a brief bounded retry (F3): on a rapid restart the prior server's
        // graceful shutdown has been awaited, but the OS can still need a beat to release
        // the listening socket, so an immediate re-bind may transiently see "address in
        // use". Retry a few times before surfacing the error rather than failing the
        // restart on a race the user can't act on.
        let listener = {
            let mut last_err = None;
            let mut bound = None;
            for attempt in 0..BIND_RETRIES {
                match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                    Ok(l) => {
                        bound = Some(l);
                        break;
                    }
                    Err(e) => {
                        last_err = Some(e);
                        if attempt + 1 < BIND_RETRIES {
                            tokio::time::sleep(BIND_RETRY_DELAY).await;
                        }
                    }
                }
            }
            match bound {
                Some(l) => l,
                None => {
                    let e = last_err.expect("a failed bind recorded its error");
                    return Err(AppError::new(format!(
                        "webhookPort 监听失败（端口 {port}）：{e}"
                    )));
                }
            }
        };

        let ctx = Arc::new(WebhookCtx {
            secret,
            routes,
            ingestor,
            refresher,
            deliveries: self.deliveries.clone(),
        });
        let router = Router::new()
            .route(WEBHOOK_PATH, post(handle_webhook))
            // Cap the public endpoint's request body. GitHub webhook and Azure Service Hook
            // PR payloads are well under this (typically < 25 KiB); the limit bounds the memory
            // a forged POST can make us buffer before the auth check (HMAC / Bearer) rejects it.
            .layer(DefaultBodyLimit::max(1024 * 1024))
            .with_state(ctx);
        // Graceful-shutdown signal: on teardown we fire `shutdown_tx` and AWAIT
        // `server_task`, so axum stops accepting + drops the listener BEFORE teardown
        // returns — the deterministic basis for an immediate same-port re-bind (F3). A
        // bare `abort()` does not reliably drop the listener before the next `bind`.
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_task = spawn(async move {
            let _ = axum::serve(listener, router.into_make_service())
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        // Configured public URL for the non-scraping modes (empty → None, so the status
        // honestly reports "no URL yet" rather than a blank string).
        let configured_url = (!public_url.trim().is_empty()).then(|| public_url.trim().to_string());

        // Resolve the tunnel per mode. On a spawn error the server task (already holding
        // the bound port) MUST be aborted here — otherwise it detaches, leaks the port,
        // and `self.runtime` stays `None` so a later `stop_inner` can't reap it.
        let (tunnel, drain_task, public_url): TunnelParts = match mode {
            WebhookTunnelMode::Quick => match spawn_quick_tunnel(
                cloudflared.as_ref().expect("quick mode checked resolver"),
                port,
            )
            .await
            {
                Ok((child, drain, url)) => (Some(child), Some(drain), url),
                Err(e) => {
                    let _ = shutdown_tx.send(());
                    let _ = server_task.await; // graceful stop frees the bound port on failed start.
                    return Err(e);
                }
            },
            WebhookTunnelMode::Command => match spawn_custom_tunnel(&tunnel_command, port) {
                Ok((child, drain)) => (
                    Some(child),
                    Some(drain),
                    Arc::new(StdMutex::new(configured_url)),
                ),
                Err(e) => {
                    let _ = shutdown_tx.send(());
                    let _ = server_task.await;
                    return Err(e);
                }
            },
            // No child: bind-only. The tunnel is external; the URL is whatever the
            // user configured.
            WebhookTunnelMode::Listener => (None, None, Arc::new(StdMutex::new(configured_url))),
        };

        // Snapshot the URL known at this instant for the immediate return status. In
        // `quick` mode it may still be `None` (cloudflared hasn't printed the URL yet) —
        // the drain task fills the shared `public_url` in later (F4), and the UI re-polls
        // status to surface it. `command`/`listener` already have the configured URL.
        let resolved_url = public_url.lock().unwrap().clone();

        *self.runtime.lock().unwrap() = Some(WebhookRuntime {
            server_task,
            shutdown: shutdown_tx,
            drain_task,
            tunnel,
            public_url,
            mode,
            cloudflared_fingerprint: (mode == WebhookTunnelMode::Quick).then(|| {
                cloudflared
                    .as_ref()
                    .expect("quick mode resolved")
                    .fingerprint()
                    .to_string()
            }),
        });

        let message = match (mode, &resolved_url) {
            (WebhookTunnelMode::Quick, Some(u)) => format!("已启动，公网 URL：{u}"),
            (WebhookTunnelMode::Quick, None) => {
                "隧道已启动，但未能在超时内解析公网 URL（请查看 cloudflared 日志）".to_string()
            }
            (WebhookTunnelMode::Command, Some(u)) => format!("已启动自定义隧道，公网 URL：{u}"),
            (WebhookTunnelMode::Command, None) => {
                "已启动自定义隧道（未配置 webhookPublicUrl，无法显示公网 URL）".to_string()
            }
            (WebhookTunnelMode::Listener, Some(u)) => format!("接收端已监听，公网 URL：{u}"),
            (WebhookTunnelMode::Listener, None) => {
                "接收端已监听（隧道外置，未配置 webhookPublicUrl）".to_string()
            }
        };
        // `cloudflared_installed` is always `true` on this success path: `quick` mode
        // only reaches here past the install short-circuit above, and the non-quick
        // modes don't use cloudflared (reporting `true` keeps the UI's "install
        // cloudflared" prompt from firing spuriously for a mode that doesn't need it).
        Ok(WebhookStatus::new(true, resolved_url, true, message))
    }

    /// Sync teardown used where no `.await` is available — `shutdown` (app exit) and
    /// `status`'s crash self-heal. Idempotent. Does NOT wait for the port to free; an
    /// awaiting caller that re-binds uses [`Self::stop`] / the restart path instead.
    fn stop_inner(&self) {
        if let Some(rt) = self.runtime.lock().unwrap().take() {
            rt.teardown();
        }
    }

    /// Explicit stop (the `stop_webhook` command). Async + serialized against `start` via
    /// `start_lock`: a stop issued WHILE a start is in flight waits for the start to
    /// finish, then tears the just-started runtime down — so the stop is honored rather
    /// than silently lost in the pre-registration window (F1). Awaits the server task's
    /// end so a subsequent start can re-bind the port cleanly (F3).
    pub async fn stop(&self) {
        let _guard = self.start_lock.lock().await;
        // Take out of the std mutex BEFORE awaiting (never hold a `StdMutex` guard across
        // an `.await`).
        let runtime = self.runtime.lock().unwrap().take();
        if let Some(rt) = runtime {
            rt.teardown_awaiting().await;
        }
    }

    /// App-shutdown cleanup (wired to `RunEvent::Exit` in `lib.rs`, like
    /// `CodexManager::shutdown`) so the tunnel child never outlives the app. Sync
    /// best-effort (the exit handler can't await); the OS reaps anything in flight.
    pub fn shutdown(&self) {
        self.stop_inner();
    }

    pub fn active_cloudflared_fingerprint(&self) -> Option<String> {
        let mut runtime = self.runtime.lock().unwrap();
        let runtime = runtime.as_mut()?;
        if runtime.mode != WebhookTunnelMode::Quick {
            return None;
        }
        match runtime.tunnel.as_mut().map(Child::try_wait) {
            Some(Ok(None)) => runtime.cloudflared_fingerprint.clone(),
            // A finished child (or a liveness probe error) is not evidence of a live
            // resident. `status` owns teardown/self-healing; the probe only reports.
            Some(Ok(Some(_))) | Some(Err(_)) | None => None,
        }
    }

    /// Current status (running + URL), mode-aware so it only touches cloudflared for the
    /// one mode that owns it.
    ///
    /// Self-heals a crashed tunnel: a `runtime: Some` whose tunnel child has exited is a
    /// DEAD tunnel that would otherwise still report `running: true`. We probe the child
    /// with `try_wait` (non-blocking — no await held across the `StdMutex`), and on an
    /// exited child take the runtime + tear it down (mirroring `stop_inner`) and report
    /// not-running. Mirrors codex's "drop a dead resident process" self-heal
    /// (`engines/codex/process.rs::kill_and_reap`).
    ///
    /// `listener` mode has NO tunnel child (`tunnel: None`), so there is nothing to
    /// crash and nothing to self-heal — it stays `running` until an explicit stop.
    ///
    /// Mode is taken from the live `runtime.mode` when running; when stopped there is no
    /// runtime to read it from, so the caller passes `configured_mode` (the persisted
    /// `webhook_tunnel_mode`). Only `Quick` owns cloudflared, so:
    /// - the `cloudflared_installed` probe (a ~5s subprocess) runs ONLY for `Quick` —
    ///   `command` / `listener` report `cloudflared_installed: true` unconditionally
    ///   (their tunnel is the user command / fully external; the panel filters this field
    ///   per mode anyway, so this only saves the probe + suppresses a spurious "install
    ///   cloudflared" prompt for a mode that doesn't need it);
    /// - the crash message names cloudflared only for `Quick`; `command` mode says
    ///   "自定义隧道进程已退出" (`listener` never crashes — no child);
    /// - the not-running install nag ("brew install cloudflared") fires only for `Quick`.
    pub async fn status(
        &self,
        cloudflared: AppResult<Option<ResolvedCli>>,
        configured_mode: WebhookTunnelMode,
    ) -> WebhookStatus {
        let (cloudflared, resolution_error) = match cloudflared {
            Ok(cloudflared) => (cloudflared, None),
            Err(error) => (None, Some(error.message)),
        };
        let mut crashed = false;
        let (running, public_url, mode) = {
            let mut guard = self.runtime.lock().unwrap();
            // Probe liveness + snapshot the URL + mode in one borrow, then release it so
            // the self-heal `take()` below can re-borrow the guard mutably. A `None`
            // tunnel (listener mode) has no child to probe → `None` try_wait = "alive".
            let probe = guard.as_mut().map(|rt| {
                let wait = rt.tunnel.as_mut().map(Child::try_wait);
                // Read the LIVE shared URL: in quick mode the drain task may have filled
                // it in after `start` returned (F4), so a late URL surfaces here.
                (wait, rt.public_url.lock().unwrap().clone(), rt.mode)
            });
            match probe {
                // Tunnel child exited → dead tunnel: take + teardown, report not-running.
                // Keep the runtime's mode for the crash message even though it's gone.
                Some((Some(Ok(Some(_exit))), _, m)) => {
                    crashed = true;
                    if let Some(dead) = guard.take() {
                        dead.teardown();
                    }
                    (false, None, m)
                }
                // Alive: a live child (`Some(Ok(None))`), a transient wait Err
                // (`Some(Err(_))` — best-effort: stay running, the next probe retries),
                // or no child at all (`None`, listener mode). Never tear down a healthy
                // (or childless) tunnel. Use the running runtime's own mode.
                Some((_, url, m)) => (true, url, m),
                // No runtime → stopped. Fall back to the configured mode for the text.
                None => (false, None, configured_mode),
            }
        };
        // Only `Quick` owns cloudflared, so only `Quick` pays for the install probe; the
        // other modes report `true` (no nag for a mode that doesn't use cloudflared).
        let installed = if mode == WebhookTunnelMode::Quick {
            match cloudflared.as_ref() {
                Some(cli) => cloudflared_installed(cli).await,
                None => false,
            }
        } else {
            true
        };
        let mut message = if running {
            match &public_url {
                Some(u) => format!("运行中，公网 URL：{u}"),
                None => "运行中（公网 URL 尚未解析）".to_string(),
            }
        } else if crashed {
            // Crash text is mode-specific: only `Quick`'s child is cloudflared.
            match mode {
                WebhookTunnelMode::Quick => {
                    "隧道已退出（cloudflared 进程已退出），请重新启动 Webhook".to_string()
                }
                WebhookTunnelMode::Command => {
                    "自定义隧道进程已退出，请重新启动 Webhook".to_string()
                }
                // `listener` has no child to crash; unreachable on this branch, but a
                // neutral message keeps the match exhaustive without nagging cloudflared.
                WebhookTunnelMode::Listener => "隧道已退出，请重新启动 Webhook".to_string(),
            }
        } else if mode == WebhookTunnelMode::Quick && !installed {
            resolution_error.clone().unwrap_or_else(|| {
                "未运行；未检测到 cloudflared（brew install cloudflared）".to_string()
            })
        } else {
            "未运行".to_string()
        };
        if running && mode == WebhookTunnelMode::Quick {
            if let Some(error) = resolution_error {
                message.push_str("；当前 cloudflared 配置错误：");
                message.push_str(&error);
            }
        }
        WebhookStatus::new(running, public_url, installed, message)
    }
}

/// Shared, runtime-agnostic state for the axum handler.
struct WebhookCtx {
    secret: String,
    /// The enabled projects' routing info (#35), a SNAPSHOT taken at
    /// [`WebhookManager::start`] time from every `enabled`
    /// [`crate::config::model::Project`]. The handler routes a verified payload to the
    /// project whose `repo` matches the event repository (case-insensitive); a payload
    /// matching no route is dropped (fail-closed — HMAC proves the secret is known, not
    /// that the event is for a repo this app reviews). This list does NOT refresh for
    /// the runtime's life — a project add/remove/enable requires a webhook restart (the
    /// composition root wires that on `set_config`).
    routes: Vec<ProjectRoute>,
    /// The injected ingest hook (#61): handed each parsed, routed [`WebhookEvent`] (GitHub).
    ingestor: WebhookIngestor,
    /// The injected Azure refresh hook (AB#822): handed a routed `project_id` to re-discover.
    refresher: WebhookRefresher,
    /// The manager's delivery-diagnostics ring (#62), shared so the handler can record
    /// the early-exit classifications directly (the routable terminal status is recorded
    /// by `ingest_webhook` via `AppState`).
    deliveries: Arc<StdMutex<VecDeque<WebhookDelivery>>>,
}

/// Record one early-exit delivery diagnostic into the shared ring (#62), popping the
/// oldest when full. Mirrors [`WebhookManager::record_delivery`] but operates on the
/// `WebhookCtx`'s shared handle (the handler has no `&WebhookManager`). The `event` /
/// `repo` / etc. are whatever is known at the exit point.
fn record_into(ring: &Arc<StdMutex<VecDeque<WebhookDelivery>>>, d: WebhookDelivery) {
    let mut ring = ring.lock().unwrap();
    if ring.len() >= DELIVERY_RING_CAP {
        ring.pop_front();
    }
    ring.push_back(d);
}

/// `POST /webhook`. Detect the provider by header (AB#822) and dispatch: GitHub sends
/// `X-GitHub-Event`, an Azure Service Hook does not. BOTH branches authenticate before
/// acting (GitHub: HMAC over the body; Azure: a `Bearer` token), so an unauthenticated POST
/// is rejected whichever way it routes; the body parse + route + hand-off to the injected
/// ingestor (off the request path, for a fast 2xx) is provider-specific but produces the
/// SAME [`WebhookEvent`].
///
/// Records EXACTLY ONE webhook-delivery diagnostic (#62) per request: the early-exit
/// classifications (`Unauthorized` / `Ignored` / `BadPayload` / `WrongRepo`) are recorded by
/// the provider helper; for a `Routable` nothing is recorded here — `ingest_webhook` records
/// the terminal status (it knows whether the candidate gated / dispatched).
async fn handle_webhook(
    State(ctx): State<Arc<WebhookCtx>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let received_at = ledger::now_epoch();
    // Provider detection by the `X-GitHub-Event` header (GitHub sends it; an Azure Service
    // Hook does not). A non-UTF-8 value on a PRESENT header is a corrupt GitHub delivery — it
    // must NOT silently fall to the Azure branch (whose `Bearer` 401 would misdirect
    // diagnosis), so it's recorded as `BadPayload`. A present-but-empty or absent header → the
    // Azure branch (which fail-closes on its own Bearer check if it isn't a real Azure POST).
    match headers.get("x-github-event").map(|v| v.to_str()) {
        Some(Ok(event)) if !event.is_empty() => {
            handle_github_delivery(&ctx, received_at, event.to_string(), &headers, &body).await
        }
        Some(Err(_)) => {
            record_into(
                &ctx.deliveries,
                WebhookDelivery {
                    received_at_epoch: received_at,
                    event: String::new(),
                    action: None,
                    repo: None,
                    pr_number: None,
                    kind: None,
                    status: DeliveryStatus::BadPayload,
                    message: Some("X-GitHub-Event 头不是合法 UTF-8".to_string()),
                },
            );
            StatusCode::BAD_REQUEST
        }
        _ => handle_azure_delivery(&ctx, received_at, &headers, &body).await,
    }
}

/// GitHub provider branch (AB#822 split out of `handle_webhook`; logic unchanged). Verify the
/// `X-Hub-Signature-256` HMAC over the raw body, require a `pull_request` event, then parse +
/// route via [`parse_delivery`]. Records the early-exit diagnostics; a `Routable` is handed to
/// the ingestor and recorded by `ingest_webhook`.
async fn handle_github_delivery(
    ctx: &Arc<WebhookCtx>,
    received_at: u64,
    event: String,
    headers: &HeaderMap,
    body: &Bytes,
) -> StatusCode {
    // A missing header or a non-UTF-8 value both collapse to "" — equivalent to an
    // absent signature, which `verify_signature`'s `strip_prefix("sha256=")` gate then
    // rejects (fail-closed). The public endpoint never acts on an unverified request.
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !verify_signature(&ctx.secret, body, signature) {
        // The signature/secret is NEVER recorded — only that verification failed.
        record_into(
            &ctx.deliveries,
            WebhookDelivery {
                received_at_epoch: received_at,
                event,
                action: None,
                repo: None,
                pr_number: None,
                kind: None,
                status: DeliveryStatus::Unauthorized,
                message: Some("HMAC 校验失败（签名/密钥不匹配）".to_string()),
            },
        );
        return StatusCode::UNAUTHORIZED;
    }

    if event != "pull_request" {
        record_into(
            &ctx.deliveries,
            WebhookDelivery {
                received_at_epoch: received_at,
                event,
                action: None,
                repo: None,
                pr_number: None,
                kind: None,
                status: DeliveryStatus::Ignored,
                message: Some("非 pull_request 事件（已确认，不处理）".to_string()),
            },
        );
        return StatusCode::OK;
    }

    let payload: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            record_into(
                &ctx.deliveries,
                WebhookDelivery {
                    received_at_epoch: received_at,
                    event,
                    action: None,
                    repo: None,
                    pr_number: None,
                    kind: None,
                    status: DeliveryStatus::BadPayload,
                    message: Some("请求体不是合法 JSON".to_string()),
                },
            );
            return StatusCode::BAD_REQUEST;
        }
    };
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .map(str::to_string);

    match parse_delivery(&payload, &ctx.routes) {
        ParseResult::Routable(mut ev) => {
            // Stamp the receipt time so the terminal diagnostic matches the early-exit records
            // (the pure parser leaves it 0).
            ev.received_at = received_at;
            // AB#1065: carry the verbatim body + the `X-GitHub-Delivery` GUID through the
            // widened seam so the inbox can persist + dedup this delivery before re-feeding the
            // SAME parsed `WebhookEvent` to `ingest_webhook`. A non-UTF-8 / absent GUID → None
            // (the inbox falls back to a body-hash dedupe key). The raw body is already bounded
            // by the request body limit, so cloning it to an owned String is safe.
            let raw = String::from_utf8_lossy(body).into_owned();
            let guid = headers
                .get("x-github-delivery")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let ingestor = ctx.ingestor.clone();
            // AB#1065 F1: AWAIT the DURABLE PERSIST and gate the ACK on it. `Ok` = the delivery is
            // persisted and the single inbox worker has been woken; `Err` = the durable insert
            // failed → 500 so the platform RETRIES (a 2xx is
            // never retried, so persisting before ACK is what stops a crash from losing a delivery).
            // The terminal processing status (Processed/Failed) is recorded by that worker,
            // independently of this ACK.
            match ingestor(raw, guid, *ev).await {
                Ok(()) => StatusCode::OK,
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
            }
        }
        ParseResult::WrongRepo { repo } => {
            record_into(
                &ctx.deliveries,
                WebhookDelivery {
                    received_at_epoch: received_at,
                    event,
                    action,
                    repo,
                    pr_number: None,
                    kind: None,
                    status: DeliveryStatus::WrongRepo,
                    message: Some("仓库未匹配任何已启用项目（已忽略）".to_string()),
                },
            );
            StatusCode::OK
        }
        // A malformed `pull_request` payload is acknowledged (200) — GitHub need not
        // retry a payload we can't parse — but recorded as `BadPayload` for diagnosis.
        ParseResult::Malformed => {
            record_into(
                &ctx.deliveries,
                WebhookDelivery {
                    received_at_epoch: received_at,
                    event,
                    action,
                    repo: None,
                    pr_number: None,
                    kind: None,
                    status: DeliveryStatus::BadPayload,
                    message: Some("pull_request 载荷缺少必要字段".to_string()),
                },
            );
            StatusCode::OK
        }
    }
}

/// Azure DevOps provider branch (AB#822). An Azure Service Hook carries no `X-GitHub-Event`
/// and no body HMAC; it authenticates via a user-configured `Authorization: Bearer <secret>`
/// header (the same `webhookSecret`), constant-time compared by [`verify_azure_token`]. The
/// PR event payload is `{ eventType, resource }`; only `git.pullrequest.created` /
/// `git.pullrequest.updated` drive a refresh (others acknowledged, parity with GitHub's
/// non-`pull_request` Ignored).
///
/// **Refresh signal, not a payload ingest.** Azure PR Service Hooks carry NO `labels` and do
/// NOT fire on label changes (the documented triggers are push / reviewers / status / vote),
/// so the payload can't classify review-vs-check the way the GitHub path does. Instead this
/// routes the event to a project (by repo + Azure project) and hands the `project_id` to the
/// injected [`WebhookRefresher`], which re-runs `az repos pr list` discovery — reading the
/// AUTHORITATIVE current labels — and upserts + dispatches via the poll path. The webhook is a
/// low-latency "PR activity happened, re-check now" nudge; the poll path remains the backstop
/// for a pure label-add (which fires no hook). The list/dispatch outcome is recorded by
/// `PollStatus`; here we only record `Refreshed` (or the early-exit drops).
async fn handle_azure_delivery(
    ctx: &Arc<WebhookCtx>,
    received_at: u64,
    headers: &HeaderMap,
    body: &Bytes,
) -> StatusCode {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !verify_azure_token(&ctx.secret, auth) {
        // The token/secret is NEVER recorded — only that verification failed. `event` is ""
        // (the eventType is only known after a successful auth + JSON parse).
        record_into(
            &ctx.deliveries,
            WebhookDelivery {
                received_at_epoch: received_at,
                event: String::new(),
                action: None,
                repo: None,
                pr_number: None,
                kind: None,
                status: DeliveryStatus::Unauthorized,
                message: Some("Azure 鉴权失败（Authorization 缺失或不匹配）".to_string()),
            },
        );
        return StatusCode::UNAUTHORIZED;
    }

    let payload: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            record_into(
                &ctx.deliveries,
                WebhookDelivery {
                    received_at_epoch: received_at,
                    event: String::new(),
                    action: None,
                    repo: None,
                    pr_number: None,
                    kind: None,
                    status: DeliveryStatus::BadPayload,
                    message: Some("请求体不是合法 JSON".to_string()),
                },
            );
            return StatusCode::BAD_REQUEST;
        }
    };
    let event_type = payload
        .get("eventType")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Only PR created/updated trigger a refresh; other Azure events (push outside a PR,
    // comment, build, …) are acknowledged without acting (parity with GitHub's Ignored).
    if event_type != AZURE_PR_CREATED && event_type != AZURE_PR_UPDATED {
        record_into(
            &ctx.deliveries,
            WebhookDelivery {
                received_at_epoch: received_at,
                event: event_type,
                action: None,
                repo: None,
                pr_number: None,
                kind: None,
                status: DeliveryStatus::Ignored,
                message: Some("非 PR 创建/更新事件（已确认，不处理）".to_string()),
            },
        );
        return StatusCode::OK;
    }

    match route_azure_delivery(&payload, &ctx.routes) {
        AzureRoute::Refresh { project_id, repo } => {
            // AB#1065 F1: the widened seam carries the verbatim body + routed identity so the inbox
            // persists an Azure AUDIT entry (AWAITED — gates the ACK) BEFORE the `az` re-discovery
            // (spawned post-ACK inside the closure; `discover_once` coalesces per project). `Err` =
            // the durable audit insert failed → 500 so the platform retries (no lost delivery).
            let raw = String::from_utf8_lossy(body).into_owned();
            let refresher = ctx.refresher.clone();
            match refresher(raw, project_id, repo.clone()).await {
                Ok(()) => {
                    record_into(
                        &ctx.deliveries,
                        WebhookDelivery {
                            received_at_epoch: received_at,
                            event: event_type,
                            action: None,
                            repo: Some(repo),
                            pr_number: None,
                            kind: None,
                            status: DeliveryStatus::Refreshed,
                            message: Some("已触发 az 重新发现（读取当前标签）".to_string()),
                        },
                    );
                    StatusCode::OK
                }
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
            }
        }
        AzureRoute::WrongRepo { repo } => {
            record_into(
                &ctx.deliveries,
                WebhookDelivery {
                    received_at_epoch: received_at,
                    event: event_type,
                    action: None,
                    repo,
                    pr_number: None,
                    kind: None,
                    status: DeliveryStatus::WrongRepo,
                    message: Some("仓库/项目未匹配任何已启用 Azure 项目（已忽略）".to_string()),
                },
            );
            StatusCode::OK
        }
        AzureRoute::Malformed => {
            record_into(
                &ctx.deliveries,
                WebhookDelivery {
                    received_at_epoch: received_at,
                    event: event_type,
                    action: None,
                    repo: None,
                    pr_number: None,
                    kind: None,
                    status: DeliveryStatus::BadPayload,
                    message: Some("Azure PR 载荷缺少必要字段".to_string()),
                },
            );
            StatusCode::OK
        }
    }
}

/// Constant-time verify of a GitHub `X-Hub-Signature-256` header (`sha256=<hex>`)
/// against `HMAC-SHA256(secret, body)`. An empty secret, a malformed header, or a
/// mismatch all fail closed (the public endpoint must never accept an unsigned POST).
/// Pure — unit-tested without a server.
fn verify_signature(secret: &str, body: &[u8], header: &str) -> bool {
    if secret.is_empty() {
        return false;
    }
    let Some(hex_digest) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_digest) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// Constant-time verify of an Azure Service Hook `Authorization: Bearer <secret>` header
/// against the configured `webhookSecret` (AB#822). Azure Service Hooks carry no body HMAC,
/// so the user configures this shared-secret header on the subscription (see the module
/// **Security** note). An empty secret, an absent / non-`Bearer` header, or a token mismatch
/// all fail closed — the public endpoint must never accept an unauthenticated Azure POST. The
/// token compare is constant-time via [`subtle::ConstantTimeEq`] (the Azure analogue of the
/// GitHub path's `hmac::Mac::verify_slice`): it leaks only the secret LENGTH (the `ct_eq`
/// length short-circuit), never the secret CONTENT through timing. The `Bearer` scheme is
/// matched case-INSENSITIVELY (RFC 7235 §2.1 auth-scheme is case-insensitive; a proxy / SDK
/// may send `bearer`), so a correctly-configured delivery is never spuriously 401'd. Pure —
/// unit-tested without a server.
fn verify_azure_token(secret: &str, header: &str) -> bool {
    if secret.is_empty() {
        return false;
    }
    // Split "<scheme> <token>" and accept the token only when the scheme is `Bearer`
    // (case-insensitive). A header with no space, or a non-Bearer scheme, fails closed.
    let Some(token) = header
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token)
    else {
        return false;
    };
    // `ct_eq` on byte slices returns `Choice(0)` for differing lengths (leaking only length,
    // never content) and otherwise compares in constant time.
    token.as_bytes().ct_eq(secret.as_bytes()).into()
}

/// Parse + route a GitHub `pull_request` webhook payload (#61) into a
/// [`ParseResult`]. PURE (no `AppHandle`) so the whole classification is unit-tested
/// without a server; the `AppHandle`-bound ingest (`commands::ingest_webhook`) consumes
/// the `Routable` arm.
///
/// Unlike the old `payload_to_candidate` (which returned `None` for every non-dispatch
/// case, so a webhook never touched the persisted PR list), this surfaces the FULL
/// outcome the ingest needs to upsert + emit even when nothing dispatches (the #61 fix):
///
/// - missing `pull_request`, or a required field (`number` / `head.sha` / `head.ref` /
///   `labels`) absent / structurally invalid → [`ParseResult::Malformed`] (`labels` must
///   be an array — GitHub always sends one, possibly empty `[]`; an absent / `null` /
///   non-array `labels` is malformed, NOT silently "no trigger label" — F3);
/// - repo matches no enabled route → [`ParseResult::WrongRepo`] (fail-closed: HMAC
///   proves the secret is known, NOT that the event is for a monitored repo);
/// - PR not open (closed/merged) → `Routable` with [`IngestIntent::StatusOnly`]
///   ("PR 已关闭或合并") so the list row reflects it (the poll path never lists closed
///   PRs; this is the push-path equivalent — list-only, never dispatched);
/// - open PR → `Track { candidate: Some(..), conflict: false }` (the remaining
///   static/cooldown gates run downstream in `webhook_view`; rule matching decides whether
///   labels produce review/check/notify actions).
///
/// The metadata (title / labels / url) is extracted for the list row regardless of the
/// dispatch decision — the webhook payload carries it, so the ingest never re-fetches
/// via `gh pr view`.
fn parse_delivery(payload: &Value, routes: &[ProjectRoute]) -> ParseResult {
    let Some(pr) = payload.get("pull_request") else {
        return ParseResult::Malformed;
    };

    // Repo-routing gate (#35, was F2's single-repo ownership gate): match the event's
    // repo (top-level `repository.full_name`, falling back to the PR's
    // `base.repo.full_name`) case-insensitively against each route's `repo`. A payload
    // matching none is dropped fail-closed (the HMAC proves the secret is known, not
    // that the event is for a repo this app monitors). The matched route supplies the
    // project id to track under AND the labels to classify by.
    let event_repo = payload
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(Value::as_str)
        .or_else(|| {
            pr.get("base")
                .and_then(|b| b.get("repo"))
                .and_then(|r| r.get("full_name"))
                .and_then(Value::as_str)
        });
    let route = match event_repo.and_then(|repo| {
        routes
            .iter()
            // Provider-isolated (AB#822): a GitHub delivery only matches GitHub routes.
            .find(|r| r.source_kind == SourceKind::Github && r.repo.eq_ignore_ascii_case(repo))
    }) {
        Some(r) => r,
        None => {
            // Carry the repo (when known) for the delivery diagnostic.
            return ParseResult::WrongRepo {
                repo: event_repo.map(str::to_string),
            };
        }
    };

    // Required fields for a usable row + dispatch candidate. A `pull_request` lacking
    // any of these is malformed (GitHub always sends them on a real PR event).
    let Some(number) = pr.get("number").and_then(Value::as_u64) else {
        return ParseResult::Malformed;
    };
    let head = pr.get("head");
    let Some(head_sha) = head
        .and_then(|h| h.get("sha"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return ParseResult::Malformed;
    };
    let Some(head_ref) = head
        .and_then(|h| h.get("ref"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return ParseResult::Malformed;
    };

    // Display metadata for the list row — extracted regardless of the dispatch decision
    // so a webhook-tracked row carries title/labels/url without a `gh pr view` round
    // trip. `html_url` is the field GitHub's PR webhook carries (the poll path uses
    // gh's `url`, the same PR HTML URL); absent → "".
    let title = pr
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let url = pr
        .get("html_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // `labels` is a REQUIRED structural field, same tier as number/head.sha/head.ref:
    // GitHub ALWAYS sends a `labels` array on a real PR event (possibly empty `[]`), so an
    // absent / `null` / non-array `labels` is a malformed payload, NOT "no labels". Coercing
    // it to an empty Vec (the old `unwrap_or_default()`) silently turned a malformed payload
    // into a valid "no trigger label" state update on a tracked PR (→ StatusOnly
    // TriggerLabelRemoved) — F3. Validated HERE (alongside the other required-field
    // extractions, before the open/closed + label classification that needs the names) so it
    // rejects regardless of open/closed: a malformed payload is malformed either way. An
    // EXPLICIT empty array `[]` is still valid → empty Vec → genuine "no trigger label".
    let Some(labels_arr) = pr.get("labels").and_then(Value::as_array) else {
        return ParseResult::Malformed;
    };
    let native_labels: Vec<String> = labels_arr
        .iter()
        .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    // AB#717: resolve effective labels (native vs title-parsed) before classifying, so a
    // project using title labels classifies a webhook PR by its title tags too (parity with
    // the poll path). For a native-source project this is exactly the provider labels. The
    // structural `labels` array is still REQUIRED above (GitHub always sends it) — we only
    // change which names feed classification + the tracked row's display labels.
    let labels = super::labels::effective_labels(native_labels, &title, route.label_source);
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .map(str::to_string);

    let author = pr
        .get("user")
        .and_then(|u| u.get("login"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let is_draft = pr.get("draft").and_then(Value::as_bool).unwrap_or(false);

    // Fork PR ⇒ head repo differs from base repo. Missing repo info (e.g. a deleted
    // fork) is treated as cross-repo (fail safe): `should_skip` then drops it, so the
    // app never runs codex against untrusted fork code it can't attribute.
    let full_name = |repo: Option<&Value>| {
        repo.and_then(|r| r.get("full_name"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let head_repo = full_name(head.and_then(|h| h.get("repo")));
    let base_repo = full_name(pr.get("base").and_then(|b| b.get("repo")));
    let is_cross_repository = match (head_repo, base_repo) {
        // Case-insensitive: GitHub `full_name` is case-insensitive (same as the route
        // match's `eq_ignore_ascii_case`), so a case-only difference is NOT a fork.
        (Some(h), Some(b)) => !h.eq_ignore_ascii_case(&b),
        _ => true,
    };

    // Helper to build the routed event with a given intent (the metadata is shared).
    // `received_at` is left 0 here (pure parser) and stamped by `handle_webhook` on Routable.
    let event = |intent: IngestIntent| WebhookEvent {
        project_id: route.id.clone(),
        received_at: 0,
        event: "pull_request".to_string(),
        action: action.clone(),
        repo: route.repo.clone(),
        number,
        title: title.clone(),
        labels: labels.clone(),
        url: url.clone(),
        intent,
    };

    // Parity with the poll path's `--state open` (`gh.rs`): a closed/merged PR is NOT a
    // dispatch candidate, but unlike the old drop it now upserts a list row reflecting
    // the closed state (StatusOnly — list-only, never dispatched).
    if pr.get("state").and_then(Value::as_str) != Some("open") {
        return ParseResult::Routable(Box::new(event(IngestIntent::StatusOnly {
            kind: StatusOnlyKind::ClosedOrMerged,
        })));
    }

    let intent = IngestIntent::Track {
        candidate: Some(Candidate {
            number,
            head_sha,
            head_ref,
            author,
            is_cross_repository,
            is_draft,
            // The rule engine stamps the concrete review/check action kind later.
            kind: ReviewKind::Review,
        }),
        conflict: false,
    };
    ParseResult::Routable(Box::new(event(intent)))
}

/// Outcome of routing an Azure DevOps Service Hook PR event to a project (AB#822). Unlike the
/// GitHub [`parse_delivery`], this does NOT build a [`WebhookEvent`]: Azure PR Service Hooks
/// carry no labels and don't fire on label changes, so the payload can't classify a candidate
/// — the matched project is re-discovered via `az` instead (see [`handle_azure_delivery`]).
#[derive(Debug)]
enum AzureRoute {
    /// Routed to this project — trigger a re-discovery. `repo` is carried for the diagnostic.
    Refresh { project_id: String, repo: String },
    /// Repo + project both present but matching no enabled `Azure` route (fail-closed drop).
    WrongRepo { repo: Option<String> },
    /// No `resource`, or `resource.repository.name` / `…project.name` absent — a real Azure PR
    /// Service Hook always carries them, so absence is a broken payload (not "not for us").
    Malformed,
}

/// Route an Azure DevOps Service Hook PR payload (AB#822) to a `project_id` to re-discover. PURE
/// (no `AppHandle`) so routing is unit-tested without a server. The handler has already gated
/// `eventType` to created/updated.
///
/// Matches `resource.repository.name` (BARE repo name) AND `resource.repository.project.name`
/// against an `Azure`-source route (provider-isolated — a bare name can't collide with a GitHub
/// `owner/name`; config rejects duplicate bare repos, so repo is globally unique and the project
/// match is the extra provider-scoped guard). Both present but no match → [`AzureRoute::WrongRepo`];
/// missing `resource` / repo name / project name → [`AzureRoute::Malformed`].
///
/// It deliberately does NOT read `resource.labels`: Azure Service Hooks omit labels and don't
/// fire on label changes, so the current labels are read authoritatively by the subsequent `az`
/// discovery the refresh triggers — not from this payload.
fn route_azure_delivery(payload: &Value, routes: &[ProjectRoute]) -> AzureRoute {
    let Some(res) = payload.get("resource") else {
        return AzureRoute::Malformed;
    };
    let repository = res.get("repository");
    let Some(repo_name) = repository
        .and_then(|r| r.get("name"))
        .and_then(Value::as_str)
    else {
        return AzureRoute::Malformed;
    };
    let Some(project_name) = repository
        .and_then(|r| r.get("project"))
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
    else {
        return AzureRoute::Malformed;
    };
    match routes.iter().find(|r| {
        r.source_kind == SourceKind::Azure
            && r.repo.eq_ignore_ascii_case(repo_name)
            && r.azure_project.eq_ignore_ascii_case(project_name)
    }) {
        Some(r) => AzureRoute::Refresh {
            project_id: r.id.clone(),
            repo: r.repo.clone(),
        },
        None => AzureRoute::WrongRepo {
            repo: Some(repo_name.to_string()),
        },
    }
}

/// Probe whether `cloudflared` is runnable (`cloudflared --version`). Never errors;
/// any failure (missing binary, non-zero exit, timeout) → false. Read-only.
async fn cloudflared_installed(cloudflared: &ResolvedCli) -> bool {
    let mut cmd = cloudflared.command();
    cmd.arg("--version").kill_on_drop(true);
    matches!(
        tokio::time::timeout(CLOUDFLARED_VERSION_TIMEOUT, cmd.output()).await,
        Ok(Ok(out)) if out.status.success()
    )
}

/// Spawn a Cloudflare Quick Tunnel for `http://127.0.0.1:<port>` and capture the
/// assigned `https://*.trycloudflare.com` URL from cloudflared's stderr. The child
/// is `kill_on_drop(true)`, so the manager's runtime drop kills the tunnel. Returns
/// `(child, drain_task, Some(url))`, or `…None` if the URL didn't appear within the
/// timeout (the tunnel may still come up; the UI can re-query status). The caller
/// owns the returned `drain_task` and aborts it on stop (lifecycle symmetry).
///
/// `bin` is exec'd directly by `tokio::process::Command::new` — NOT via a shell — so
/// a configured cloudflared path with spaces/special chars is treated as one program name,
/// never word-split or shell-interpreted (no command injection). `port` is a `u16`
/// formatted into a fixed arg, also unable to inject.
async fn spawn_quick_tunnel(
    cloudflared: &ResolvedCli,
    port: u16,
) -> AppResult<(Child, JoinHandle<()>, Arc<StdMutex<Option<String>>>)> {
    let mut cmd = cloudflared.command();
    cmd.args([
        "tunnel",
        "--no-autoupdate",
        "--url",
        &format!("http://127.0.0.1:{port}"),
    ])
    // cloudflared logs (incl. the URL banner) go to stderr; stdout stays unused →
    // null it so an unread pipe can't ever block the child.
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::new(format!("无法启动 cloudflared：{e}")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::new("cloudflared stderr 不可用".to_string()))?;
    let mut lines = BufReader::new(stderr).lines();

    // Initial bounded scan: the happy path prints the URL within a few seconds.
    let initial = tokio::time::timeout(TUNNEL_URL_TIMEOUT, scan_for_url(&mut lines))
        .await
        .ok()
        .flatten();

    // Distinguish "URL still coming up" from "child already dead". The timeout
    // collapses both into `None`, but a child that EXITED before printing a URL is a
    // failed tunnel — fail fast rather than hand back a dead child the manager would
    // report as `running`. `try_wait` is non-blocking; a live-but-slow child stays
    // alive and its URL is captured late by the drain task below (F4).
    if initial.is_none() {
        if let Ok(Some(exit)) = child.try_wait() {
            return Err(AppError::new(format!(
                "cloudflared 在解析公网 URL 前已退出（{exit}）；请检查 cloudflared 日志"
            )));
        }
    }

    // Shared URL handle: seeded with the initial scan, then kept current by the drain
    // task. If the initial scan TIMED OUT (slow cloudflared) the drain loop captures the
    // URL when it finally appears and `status` surfaces it — the old drain DISCARDED
    // every post-scan line, stranding a late URL forever (F4).
    let public_url = Arc::new(StdMutex::new(initial));
    let drain_task = spawn(drain_scanning_url(lines, public_url.clone()));

    Ok((child, drain_task, public_url))
}

/// Tokenize a user-supplied tunnel command into `(program, args)`, substituting the
/// literal `{port}` placeholder in EACH token with the actual listen `port`.
///
/// Splits on ASCII whitespace (so quoting / shell metacharacters carry NO meaning):
/// the result is exec'd directly via [`Command::new`] in [`spawn_custom_tunnel`], never
/// handed to a shell, so a token can't word-split further or inject (`command` mode
/// keeps the same anti-injection property `spawn_quick_tunnel` has for `bin`). Returns
/// `None` when the command is blank (no program token) — the caller turns that into an
/// `AppError` (defense in depth; `validate` rejects an empty command for this mode
/// upstream). Pure — unit-tested.
fn build_tunnel_command_argv(command: &str, port: u16) -> Option<(String, Vec<String>)> {
    let port = port.to_string();
    let mut tokens = command
        .split_whitespace()
        .map(|tok| tok.replace("{port}", &port));
    let program = tokens.next()?;
    let args: Vec<String> = tokens.collect();
    Some((program, args))
}

/// Spawn a user-supplied tunnel command (`command` mode) bridging the public internet
/// to `http://127.0.0.1:<port>`. Mirrors [`spawn_quick_tunnel`]'s child handling
/// (`kill_on_drop(true)` so the runtime drop kills it; stdout nulled + stderr drained so
/// a full pipe can't stall the child), but does NOT scan for a URL — the public URL in
/// `command` mode comes from config, not the child's output. Returns `(child,
/// drain_task)`; the caller owns the drain task and aborts it on stop.
///
/// The command is tokenized by [`build_tunnel_command_argv`] (`{port}` substituted) and
/// exec'd DIRECTLY via [`Command::new`] — NOT via a shell — so no token is word-split
/// or shell-interpreted (same anti-injection property as `spawn_quick_tunnel`'s `bin`).
/// A blank command is an `AppError` (defense in depth; `validate` already rejects it
/// upstream for this mode).
fn spawn_custom_tunnel(command: &str, port: u16) -> AppResult<(Child, JoinHandle<()>)> {
    let (program, args) = build_tunnel_command_argv(command, port).ok_or_else(|| {
        AppError::new("webhookTunnelCommand 不能为空（command 模式需填隧道命令）".to_string())
    })?;

    let mut cmd = Command::new(&program);
    cmd.args(&args)
        // stdout unused → null it; stderr piped + drained so an unread pipe can't block.
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| AppError::new(format!("无法启动自定义隧道命令（{program}）：{e}")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::new("自定义隧道命令 stderr 不可用".to_string()))?;
    let mut lines = BufReader::new(stderr).lines();
    let drain_task = spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

    Ok((child, drain_task))
}

/// Read a tunnel child's stderr line-by-line until a trycloudflare URL appears (or EOF).
/// Generic over the reader so it (and [`drain_scanning_url`]) are unit-testable with an
/// in-memory cursor, not only a real `ChildStderr`.
async fn scan_for_url<R: AsyncBufRead + Unpin>(lines: &mut Lines<R>) -> Option<String> {
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(url) = extract_trycloudflare_url(&line) {
            return Some(url);
        }
    }
    None
}

/// Drain the tunnel child's stderr for its lifetime (so a full pipe can't stall
/// cloudflared after the initial scan), AND while the shared URL is still unresolved
/// keep scanning for the `*.trycloudflare.com` URL — capturing a URL cloudflared prints
/// after the initial scan window (F4). Once the URL is set it is a pure drain. Generic
/// over the reader for unit-testing with an in-memory cursor.
async fn drain_scanning_url<R: AsyncBufRead + Unpin>(
    mut lines: Lines<R>,
    url: Arc<StdMutex<Option<String>>>,
) {
    while let Ok(Some(line)) = lines.next_line().await {
        // Cheap pre-check avoids re-extracting once resolved (the common steady state).
        if url.lock().unwrap().is_none() {
            if let Some(found) = extract_trycloudflare_url(&line) {
                *url.lock().unwrap() = Some(found);
            }
        }
    }
}

/// Extract a `https://*.trycloudflare.com` URL from one cloudflared log line (it
/// prints the URL inside a box drawn with `|`). Pure — unit-tested.
fn extract_trycloudflare_url(line: &str) -> Option<String> {
    line.split(|c: char| c.is_whitespace() || c == '|')
        .map(str::trim)
        .find(|tok| tok.starts_with("https://") && tok.contains(".trycloudflare.com"))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::service::{resolve_cli_from, CliPath, CliResolver, CliToolsConfig},
        model::CliTool,
    };

    #[cfg(unix)]
    fn resolved_cloudflared(test_name: &str, script: &str) -> (std::path::PathBuf, ResolvedCli) {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "prmonitor-webhook-cloudflared-{test_name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("cloudflared");
        std::fs::write(&executable, script).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tools = CliToolsConfig {
            cloudflared_path: CliPath::try_from(executable.to_string_lossy().into_owned()).unwrap(),
            ..CliToolsConfig::default()
        };
        let resolved =
            resolve_cli_from(&CliResolver::default(), &tools, CliTool::Cloudflared, false).unwrap();
        (root, resolved)
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    /// A minimal `WebhookEvent` for the `event_from_webhook` normalization tests (AB#1065). The
    /// `intent` is irrelevant to normalization (it maps the display/identity fields only).
    fn sample_webhook_event(project_id: &str, number: u64) -> WebhookEvent {
        WebhookEvent {
            project_id: project_id.to_string(),
            received_at: 1_700_000_000,
            event: "pull_request".to_string(),
            action: Some("labeled".to_string()),
            repo: "owner/repo".to_string(),
            number,
            title: "Add feature".to_string(),
            labels: vec!["pr-review".to_string()],
            url: "https://example.com/pr/7".to_string(),
            intent: IngestIntent::StatusOnly {
                kind: StatusOnlyKind::ClosedOrMerged,
            },
        }
    }

    // `event_from_webhook` (AB#1065): a present guid keys `github:{guid}`; the class is
    // PullRequest; the display fields mirror the WebhookEvent; the handler-stamped receipt time is
    // preserved. The inbox depends only on the resulting `model::Event`, not on `WebhookEvent`.
    #[test]
    fn event_from_webhook_uses_guid_key_and_mirrors_fields() {
        let ev = sample_webhook_event("p1", 7);
        let event = event_from_webhook(&ev, Some("abc-123"), "{\"raw\":1}");
        let observation = event.as_observation().unwrap();
        assert_eq!(event.dedupe_key().as_str(), "github:abc-123");
        assert_eq!(event.source(), SourceKind::Github);
        assert_eq!(observation.event_type, EventType::PullRequest);
        assert_eq!(event.project_id(), "p1");
        assert_eq!(observation.subject.number, Some(7));
        assert_eq!(observation.subject.labels, vec!["pr-review".to_string()]);
        // received_at (1_700_000_000) > 0, so it wins over the `now` fallback.
        assert_eq!(event.received_at_epoch(), 1_700_000_000);
    }

    // `event_from_webhook` guid-less fallback (AB#1065): an absent / empty guid hashes the body, so
    // the SAME body yields the SAME key (dedups) and a DIFFERENT body a different key.
    #[test]
    fn event_from_webhook_falls_back_to_body_hash_without_guid() {
        let ev = sample_webhook_event("p1", 7);
        let a = event_from_webhook(&ev, None, "body-A");
        let a2 = event_from_webhook(&ev, Some(""), "body-A");
        let b = event_from_webhook(&ev, None, "body-B");
        assert!(a.dedupe_key().starts_with("github:sha256:"));
        assert_eq!(
            a.dedupe_key(),
            a2.dedupe_key(),
            "empty guid == no guid (body hash)"
        );
        assert_ne!(
            a.dedupe_key(),
            b.dedupe_key(),
            "different body → different key"
        );
    }

    // `event_from_webhook` with received_at == 0 (AB#1065): the pure parser leaves received_at 0;
    // normalization falls back to `now` (a non-zero wall clock) so a row never stamps epoch 0.
    #[test]
    fn event_from_webhook_falls_back_to_now_when_received_at_is_zero() {
        let mut ev = sample_webhook_event("p1", 7);
        ev.received_at = 0; // the pure parser leaves it 0
        let event = event_from_webhook(&ev, Some("g1"), "raw");
        assert!(
            event.received_at_epoch() > 0,
            "received_at == 0 falls back to a non-zero now"
        );
    }

    // PERSISTED REPLAY CONTRACT lock (AB#1065 F3, Medium carrier): `WebhookEvent`'s serde shape is
    // written into long-term SQLite (`inbox_event.webhook_event_json`) and read back on a LATER app
    // version to replay an old delivery, so a field rename/removal would silently break replay of
    // already-stored rows. This (a) round-trips a `WebhookEvent` through serde_json (the replay
    // contract) and (b) pins the top-level key SET + the nested `intent` tag, so a drift fails here.
    // (Possible future structural follow-up: relocate WebhookEvent to model.rs as a versioned
    // cross-slice contract — out of scope; the golden is the proportionate lock.)
    #[test]
    fn webhook_event_replay_json_round_trips_and_shape_is_pinned() {
        // Use the dispatch-candidate Track variant so the round-trip exercises the richest payload
        // (a nested `Candidate`), the one a real GitHub PR delivery persists.
        let ev = WebhookEvent {
            project_id: "p1".to_string(),
            received_at: 1_700_000_000,
            event: "pull_request".to_string(),
            action: Some("labeled".to_string()),
            repo: "owner/repo".to_string(),
            number: 7,
            title: "Add feature".to_string(),
            labels: vec!["pr-review".to_string()],
            url: "https://example.com/pr/7".to_string(),
            intent: IngestIntent::Track {
                candidate: Some(Candidate {
                    number: 7,
                    head_sha: "abc123".to_string(),
                    head_ref: "feature/x".to_string(),
                    author: "octocat".to_string(),
                    is_cross_repository: false,
                    is_draft: false,
                    kind: ReviewKind::Review,
                }),
                conflict: false,
            },
        };

        // (a) Replay contract: serialize → deserialize must reconstruct the same key fields.
        let json = serde_json::to_string(&ev).expect("WebhookEvent serializes");
        let back: WebhookEvent = serde_json::from_str(&json).expect("WebhookEvent deserializes");
        assert_eq!(back.project_id, ev.project_id);
        assert_eq!(back.number, ev.number);
        assert_eq!(back.received_at, ev.received_at);
        assert_eq!(back.labels, ev.labels);
        assert!(matches!(
            back.intent,
            IngestIntent::Track {
                candidate: Some(_),
                conflict: false
            }
        ));

        // (b) Pinned shape: the EXACT top-level field set (a rename/removal/addition surfaces here,
        // breaking old persisted-row replay). `WebhookEvent` has no `rename_all`, so keys are the
        // field names verbatim.
        let v = serde_json::to_value(&ev).expect("to_value");
        let obj = v.as_object().expect("WebhookEvent is a JSON object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "action",
                "event",
                "intent",
                "labels",
                "number",
                "project_id",
                "received_at",
                "repo",
                "title",
                "url",
            ],
            "WebhookEvent persisted-replay key set is pinned"
        );

        // The `intent` is externally tagged (no rename): the Track variant key is pinned.
        assert!(
            obj["intent"].get("Track").is_some(),
            "intent Track variant tag pinned"
        );
    }

    #[test]
    fn verify_signature_accepts_correct_hmac() {
        let body = br#"{"action":"labeled"}"#;
        assert!(verify_signature(
            "topsecret",
            body,
            &sign("topsecret", body)
        ));
    }

    #[test]
    fn verify_signature_rejects_tampered_body_wrong_secret_and_bad_header() {
        let body = br#"{"action":"labeled"}"#;
        let good = sign("topsecret", body);
        // Tampered body.
        assert!(!verify_signature(
            "topsecret",
            br#"{"action":"opened"}"#,
            &good
        ));
        // Wrong secret.
        assert!(!verify_signature("other", body, &good));
        // Missing `sha256=` prefix.
        assert!(!verify_signature("topsecret", body, "deadbeef"));
        // Non-hex digest.
        assert!(!verify_signature("topsecret", body, "sha256=zzzz"));
        // Empty secret fails closed even with a structurally valid header.
        assert!(!verify_signature("", body, &sign("", body)));
    }

    fn pr_payload(labels: &[&str], extra: serde_json::Value) -> Value {
        let label_objs: Vec<Value> = labels
            .iter()
            .map(|n| serde_json::json!({ "name": n }))
            .collect();
        let mut pr = serde_json::json!({
            "number": 42,
            "state": "open",
            "draft": false,
            "labels": label_objs,
            "user": { "login": "octocat" },
            "head": { "sha": "abc123", "ref": "feature", "repo": { "full_name": "owner/repo" } },
            "base": { "repo": { "full_name": "owner/repo" } },
        });
        if let (Value::Object(pr_map), Value::Object(extra_map)) = (&mut pr, extra) {
            for (k, v) in extra_map {
                pr_map.insert(k, v);
            }
        }
        serde_json::json!({ "action": "labeled", "pull_request": pr })
    }

    fn route(id: &str, repo: &str, _review_label: &str, _check_label: &str) -> ProjectRoute {
        ProjectRoute {
            id: id.to_string(),
            source_kind: SourceKind::Github,
            repo: repo.to_string(),
            azure_project: String::new(),
            label_source: LabelSource::Native,
        }
    }

    /// An Azure-source route (AB#822): bare `repo` name + Azure project (the routing guard).
    /// The Azure analogue of [`route`].
    fn azure_route(
        id: &str,
        project: &str,
        repo: &str,
        _review_label: &str,
        _check_label: &str,
    ) -> ProjectRoute {
        ProjectRoute {
            id: id.to_string(),
            source_kind: SourceKind::Azure,
            repo: repo.to_string(),
            azure_project: project.to_string(),
            label_source: LabelSource::Native,
        }
    }

    /// A one-project Azure route list for `myproject/myrepo` (id `"az"`), the Azure analogue of
    /// [`single_route`]. Project/repo match [`azure_pr_payload`]'s defaults.
    fn single_azure_route() -> Vec<ProjectRoute> {
        vec![azure_route("az", "myproject", "myrepo", "review", "check")]
    }

    /// Build a minimal Azure DevOps Service Hook PR payload (`{ eventType, resource }`). Only
    /// the routing-relevant fields matter now (`route_azure_delivery` ignores labels/status —
    /// they're read by the `az` re-discovery). `extra` is merged into the `resource` object so a
    /// test can override / drop `repository` etc.
    fn azure_pr_payload(extra: serde_json::Value) -> Value {
        let mut resource = serde_json::json!({
            "pullRequestId": 42,
            "status": "active",
            "repository": { "name": "myrepo", "project": { "name": "myproject" } },
        });
        if let (Value::Object(res_map), Value::Object(extra_map)) = (&mut resource, extra) {
            for (k, v) in extra_map {
                res_map.insert(k, v);
            }
        }
        serde_json::json!({ "eventType": "git.pullrequest.updated", "resource": resource })
    }

    /// Test helper: assert `route_azure_delivery` returns `Refresh` and return its `project_id`.
    fn azure_refresh_project(p: &Value, routes: &[ProjectRoute]) -> String {
        match route_azure_delivery(p, routes) {
            AzureRoute::Refresh { project_id, .. } => project_id,
            AzureRoute::WrongRepo { repo } => {
                panic!("expected Refresh, got WrongRepo {{ repo: {repo:?} }}")
            }
            AzureRoute::Malformed => panic!("expected Refresh, got Malformed"),
        }
    }

    /// A one-project route list for `owner/repo` (id `"default"`) — the single-project
    /// analogue of the old flat `(repo, review_label, check_label)` args, so the
    /// existing parse/map tests read unchanged apart from the routing wrapper.
    fn single_route(review_label: &str, check_label: &str) -> Vec<ProjectRoute> {
        vec![route("default", "owner/repo", review_label, check_label)]
    }

    /// Test helper: assert a `parse_delivery` result is `Routable` and return its event.
    fn routable(p: &Value, routes: &[ProjectRoute]) -> WebhookEvent {
        match parse_delivery(p, routes) {
            ParseResult::Routable(ev) => *ev,
            ParseResult::WrongRepo { repo } => {
                panic!("expected Routable, got WrongRepo {{ repo: {repo:?} }}")
            }
            ParseResult::Malformed => panic!("expected Routable, got Malformed"),
        }
    }

    /// Test helper: assert a `Routable` event tracks a dispatchable candidate and return
    /// it (panics on a conflict / StatusOnly / non-Routable result).
    fn dispatch_candidate(p: &Value, routes: &[ProjectRoute]) -> Candidate {
        match routable(p, routes).intent {
            IngestIntent::Track {
                candidate: Some(c), ..
            } => c,
            IngestIntent::Track {
                candidate: None, ..
            } => panic!("expected a dispatch candidate, got a conflict Track"),
            IngestIntent::StatusOnly { kind } => {
                panic!(
                    "expected a dispatch candidate, got StatusOnly: {}",
                    kind.reason()
                )
            }
        }
    }

    /// The minimal route list every webhook `start` test needs (one project for
    /// `owner/repo`). The receiver params are global; routing/labels live here now.
    fn start_routes() -> Vec<ProjectRoute> {
        single_route("review", "check")
    }

    #[test]
    fn parse_delivery_maps_review_label() {
        let p = pr_payload(&["needs-review"], serde_json::json!({}));
        let ev = routable(&p, &single_route("needs-review", "needs-check"));
        assert_eq!(ev.project_id, "default");
        assert_eq!(ev.number, 42);
        assert_eq!(ev.action.as_deref(), Some("labeled"));
        let c = dispatch_candidate(&p, &single_route("needs-review", "needs-check"));
        assert_eq!(c.number, 42);
        assert_eq!(c.kind, crate::model::ReviewKind::Review);
        assert_eq!(c.head_sha, "abc123");
        assert_eq!(c.head_ref, "feature");
        assert_eq!(c.author, "octocat");
        assert!(!c.is_draft);
        assert!(!c.is_cross_repository);
    }

    #[test]
    fn parse_delivery_carries_metadata_for_the_list_row() {
        // #61: the event carries title/labels/url for the persisted list row (no
        // `gh pr view` round trip). `html_url` is the field GitHub's PR webhook sends.
        let p = pr_payload(
            &["needs-review"],
            serde_json::json!({
                "title": "Add the thing",
                "html_url": "https://github.com/owner/repo/pull/42",
            }),
        );
        let ev = routable(&p, &single_route("needs-review", "needs-check"));
        assert_eq!(ev.title, "Add the thing");
        assert_eq!(ev.url, "https://github.com/owner/repo/pull/42");
        assert_eq!(ev.labels, vec!["needs-review".to_string()]);
        assert_eq!(ev.repo, "owner/repo");
    }

    #[test]
    fn parse_delivery_title_source_uses_title_tags_as_effective_labels() {
        // AB#717: a title-source project resolves effective labels from bracketed title tags,
        // ignoring native labels. Rules consume those labels later.
        let routes = vec![ProjectRoute {
            label_source: LabelSource::Title,
            ..route("default", "owner/repo", "needs-review", "needs-check")
        }];
        // Native label carries a trigger-like value, but the title does NOT → still Track,
        // with no effective labels.
        let native_only = pr_payload(&["needs-review"], serde_json::json!({ "title": "No tags" }));
        let ev = routable(&native_only, &routes);
        assert!(matches!(ev.intent, IngestIntent::Track { .. }));
        assert!(ev.labels.is_empty());
        // Title carries the tag → effective labels from title.
        let title_tagged = pr_payload(
            &[],
            serde_json::json!({ "title": "Fix login [needs-review]" }),
        );
        let ev = routable(&title_tagged, &routes);
        assert_eq!(ev.labels, vec!["needs-review".to_string()]);
        let c = dispatch_candidate(&title_tagged, &routes);
        assert_eq!(c.kind, crate::model::ReviewKind::Review);
    }

    #[test]
    fn parse_delivery_maps_check_label() {
        let p = pr_payload(&["needs-check"], serde_json::json!({}));
        let ev = routable(&p, &single_route("needs-review", "needs-check"));
        assert_eq!(ev.project_id, "default");
        let c = dispatch_candidate(&p, &single_route("needs-review", "needs-check"));
        assert_eq!(c.kind, crate::model::ReviewKind::Review);
    }

    #[test]
    fn parse_delivery_multiple_labels_tracks_one_candidate() {
        // Multiple labels are rule match input only; they no longer create an ingest conflict.
        let both = pr_payload(&["needs-review", "needs-check"], serde_json::json!({}));
        match routable(&both, &single_route("needs-review", "needs-check")).intent {
            IngestIntent::Track {
                candidate,
                conflict,
            } => {
                assert!(candidate.is_some());
                assert!(!conflict);
            }
            other => panic!("expected a conflict Track, got something else: {other:?}"),
        }
    }

    #[test]
    fn parse_delivery_no_trigger_label_still_tracks_open_pr() {
        // Label matching is now the rule engine's job. Webhook ingest keeps the normalized
        // open PR event even when no configured trigger label can be inferred here.
        let none = pr_payload(&["unrelated"], serde_json::json!({}));
        match routable(&none, &single_route("needs-review", "needs-check")).intent {
            IngestIntent::Track {
                candidate,
                conflict,
            } => {
                assert!(candidate.is_some());
                assert!(!conflict);
            }
            other => panic!("expected Track, got {other:?}"),
        }

        // F3: a `null` or non-array `labels`, and the `labels` key entirely absent, are
        // structurally MALFORMED (GitHub always sends a `labels` array), NOT silently "no
        // trigger label". The old behavior (`unwrap_or_default()` → empty Vec →
        // TriggerLabelRemoved) turned a malformed payload into a valid state update on a
        // tracked PR; this test now locks the rejection. (A `null` JSON value and a
        // structurally non-array value both fail `Value::as_array`.)
        for labels in [serde_json::json!(null), serde_json::json!("not-an-array")] {
            let mut p = pr_payload(&["unrelated"], serde_json::json!({}));
            p["pull_request"]["labels"] = labels;
            assert!(
                matches!(
                    parse_delivery(&p, &single_route("needs-review", "needs-check")),
                    ParseResult::Malformed
                ),
                "null/non-array labels must be Malformed, not a coerced empty set"
            );
        }
        // `labels` key entirely absent (removed from the PR object) → Malformed too.
        let mut p = pr_payload(&["unrelated"], serde_json::json!({}));
        p["pull_request"].as_object_mut().unwrap().remove("labels");
        assert!(
            matches!(
                parse_delivery(&p, &single_route("needs-review", "needs-check")),
                ParseResult::Malformed
            ),
            "absent labels key must be Malformed"
        );

        // …but an EXPLICIT empty array `[]` is the GENUINE "no trigger label" case and stays
        // valid: an OPEN PR with `[]` → Track with no effective labels. This is the case the
        // absent/null subcases above must NOT be conflated with — `[]` is a real empty-label
        // state, absent `labels` is malformed.
        let empty_open = pr_payload(&[], serde_json::json!({}));
        match routable(&empty_open, &single_route("needs-review", "needs-check")).intent {
            IngestIntent::Track {
                candidate,
                conflict,
            } => {
                assert!(candidate.is_some());
                assert!(!conflict);
            }
            other => panic!("empty [] on open PR: expected Track, got {other:?}"),
        }
        // An EXPLICIT empty array `[]` on a CLOSED PR → StatusOnly { ClosedOrMerged } (the
        // closed-state check precedes label classification, so `[]` doesn't shadow it).
        let empty_closed = pr_payload(&[], serde_json::json!({ "state": "closed" }));
        match routable(&empty_closed, &single_route("needs-review", "needs-check")).intent {
            IngestIntent::StatusOnly { kind } => {
                assert!(matches!(kind, StatusOnlyKind::ClosedOrMerged));
            }
            other => panic!("empty [] on closed PR: expected StatusOnly, got {other:?}"),
        }
    }

    #[test]
    fn parse_delivery_non_open_pr_is_status_only() {
        // A closed/merged PR is now Routable as StatusOnly (list row reflects it) rather
        // than dropped — parity with the poll path's `--state open` for DISPATCH, but the
        // row still updates (#61). closed / merged → StatusOnly("PR 已关闭或合并").
        // Assert via the type-locked `StatusOnlyKind` (no bare string coupling).
        for state in ["closed", "merged"] {
            let p = pr_payload(&["needs-review"], serde_json::json!({ "state": state }));
            match routable(&p, &single_route("needs-review", "needs-check")).intent {
                IngestIntent::StatusOnly { kind } => {
                    assert!(matches!(kind, StatusOnlyKind::ClosedOrMerged));
                    assert_eq!(kind.reason(), "PR 已关闭或合并");
                }
                other => panic!("state {state}: expected StatusOnly, got {other:?}"),
            }
        }
        // A payload with `state: null` is also non-open → StatusOnly { ClosedOrMerged }.
        let no_state = pr_payload(&["needs-review"], serde_json::json!({ "state": null }));
        match routable(&no_state, &single_route("needs-review", "needs-check")).intent {
            IngestIntent::StatusOnly { kind } => {
                assert!(matches!(kind, StatusOnlyKind::ClosedOrMerged))
            }
            other => panic!("null state: expected StatusOnly, got {other:?}"),
        }
        // Sanity: the default helper payload IS open and dispatches.
        let open = pr_payload(&["needs-review"], serde_json::json!({}));
        let _ = dispatch_candidate(&open, &single_route("needs-review", "needs-check"));
    }

    #[test]
    fn parse_delivery_preserves_draft_and_fork_flags_for_downstream_gates() {
        // draft + fork flags are PRESERVED (not dropped here) — the dispatcher's
        // should_skip applies them. A draft fork PR still yields a candidate; the
        // gate, not the parse, decides to skip it.
        let draft = pr_payload(&["needs-review"], serde_json::json!({ "draft": true }));
        assert!(dispatch_candidate(&draft, &single_route("needs-review", "needs-check")).is_draft);

        let fork = pr_payload(
            &["needs-review"],
            serde_json::json!({ "head": { "sha": "s", "ref": "r", "repo": { "full_name": "forker/repo" } } }),
        );
        assert!(
            dispatch_candidate(&fork, &single_route("needs-review", "needs-check"))
                .is_cross_repository
        );
    }

    #[test]
    fn parse_delivery_treats_missing_repo_as_cross_repo() {
        // A deleted-fork head with no repo info → fail safe to cross-repo (skipped
        // downstream), never run codex against unattributable code.
        let p = pr_payload(
            &["needs-review"],
            serde_json::json!({ "head": { "sha": "s", "ref": "r", "repo": null } }),
        );
        assert!(
            dispatch_candidate(&p, &single_route("needs-review", "needs-check"))
                .is_cross_repository
        );
    }

    #[test]
    fn parse_delivery_malformed_without_pull_request_or_required_fields() {
        // No `pull_request` → Malformed.
        let no_pr = serde_json::json!({ "action": "labeled" });
        assert!(matches!(
            parse_delivery(&no_pr, &single_route("needs-review", "needs-check")),
            ParseResult::Malformed
        ));
        // Missing required head fields (no sha) → Malformed (route matches, but the PR
        // can't form a candidate / row key).
        let no_sha = pr_payload(
            &["needs-review"],
            serde_json::json!({ "head": { "ref": "r", "repo": { "full_name": "owner/repo" } } }),
        );
        assert!(matches!(
            parse_delivery(&no_sha, &single_route("needs-review", "needs-check")),
            ParseResult::Malformed
        ));
        // Missing `number` (route matches, head is fine) → Malformed: no usable row key.
        let mut no_number = pr_payload(&["needs-review"], serde_json::json!({}));
        no_number["pull_request"]
            .as_object_mut()
            .unwrap()
            .remove("number");
        assert!(matches!(
            parse_delivery(&no_number, &single_route("needs-review", "needs-check")),
            ParseResult::Malformed
        ));
        // `head` present but missing `ref` (sha present) → Malformed: a partial head can't
        // form a candidate (the head_ref extraction fails).
        let no_head_ref = pr_payload(
            &["needs-review"],
            serde_json::json!({ "head": { "sha": "s", "repo": { "full_name": "owner/repo" } } }),
        );
        assert!(matches!(
            parse_delivery(&no_head_ref, &single_route("needs-review", "needs-check")),
            ParseResult::Malformed
        ));
    }

    #[test]
    fn parse_delivery_wrong_repo_when_no_route_matches() {
        // Repo-routing gate (#35): a verified payload whose repo matches NO enabled
        // route is `WrongRepo` (HMAC proves the secret is known, not that the event is
        // for a repo this app monitors). Payload repo `owner/repo` (helper default)
        // against routes for `owner/a` + `owner/b` → WrongRepo carrying the repo.
        let p = pr_payload(&["needs-review"], serde_json::json!({}));
        let routes = vec![
            route("a", "owner/a", "needs-review", "needs-check"),
            route("b", "owner/b", "needs-review", "needs-check"),
        ];
        match parse_delivery(&p, &routes) {
            ParseResult::WrongRepo { repo } => assert_eq!(repo.as_deref(), Some("owner/repo")),
            other => panic!("expected WrongRepo, got {other:?}"),
        }
        // Sanity: adding the matching route makes the SAME payload route + dispatch,
        // so the WrongRepo above is the routing gate, not a parse failure.
        let mut routes_with_match = routes;
        routes_with_match.push(route("c", "owner/repo", "needs-review", "needs-check"));
        assert_eq!(routable(&p, &routes_with_match).project_id, "c");
    }

    #[test]
    fn extract_trycloudflare_url_from_boxed_log_line() {
        let line = "2024-01-01T00:00:00Z INF |  https://random-words-here.trycloudflare.com  |";
        assert_eq!(
            extract_trycloudflare_url(line).as_deref(),
            Some("https://random-words-here.trycloudflare.com")
        );
        assert_eq!(extract_trycloudflare_url("INF Starting tunnel"), None);
    }

    // Wire-shape lock for `WebhookStatus` — the webhook commands' front/back wire
    // type, mirrored in `src/pr/types.ts` (Medium carrier per ai-robust.md; a field
    // rename would otherwise drift the TS mirror silently). Same pattern as
    // `gh_status_wire_shape_is_camel_case`.
    #[test]
    fn webhook_status_wire_shape_is_camel_case() {
        let v = serde_json::to_value(WebhookStatus::new(
            true,
            Some("https://x.trycloudflare.com".to_string()),
            true,
            "ok".to_string(),
        ))
        .expect("WebhookStatus serializes");
        assert!(v.get("running").is_some());
        assert!(v.get("publicUrl").is_some());
        assert!(v.get("cloudflaredInstalled").is_some());
        assert!(v.get("message").is_some());
        // payloadUrl is derived from publicUrl + the served route (WEBHOOK_PATH). A
        // route rename that forgets to follow surfaces here — the URL the UI tells the
        // user to paste must equal the path the receiver actually serves.
        assert_eq!(
            v.get("payloadUrl").and_then(Value::as_str),
            Some("https://x.trycloudflare.com/webhook")
        );
        // snake_case forms absent — a rename would surface here.
        assert!(v.get("public_url").is_none());
        assert!(v.get("payload_url").is_none());
        assert!(v.get("cloudflared_installed").is_none());
        // public_url None ⇒ payload_url None (no suffix on nothing).
        assert!(WebhookStatus::new(false, None, false, String::new())
            .payload_url
            .is_none());
    }

    /// F3 self-heal: a `runtime: Some` whose cloudflared child has exited must flip
    /// `status` to not-running (rather than the old "运行中（公网 URL 尚未解析）" for a
    /// dead tunnel). CI-safe — `true` exits 0 on darwin + Linux, no custom test bin.
    #[tokio::test]
    async fn status_self_heals_when_tunnel_child_exited() {
        let mut child = Command::new("true")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn `true`");
        let _ = child.wait().await; // ensure it has exited before we probe.

        let mgr = WebhookManager::default();
        *mgr.runtime.lock().unwrap() = Some(WebhookRuntime {
            server_task: spawn(async {}),
            shutdown: oneshot::channel().0, // rx dropped; the dead-child self-heal path doesn't await it.
            drain_task: Some(spawn(async {})),
            tunnel: Some(child),
            public_url: Arc::new(StdMutex::new(Some(
                "https://x.trycloudflare.com".to_string(),
            ))),
            mode: WebhookTunnelMode::Quick,
            cloudflared_fingerprint: Some("exited-runtime".to_string()),
        });

        assert_eq!(mgr.active_cloudflared_fingerprint(), None);

        // Bogus install bin → the probe returns false fast (no real cloudflared in CI);
        // the assertion is the self-heal flip, independent of install state.
        let s = mgr.status(Ok(None), WebhookTunnelMode::Quick).await;
        assert!(!s.running, "a dead tunnel child must flip running → false");
        assert!(s.public_url.is_none());
        assert!(s.payload_url.is_none());
        assert!(
            s.message.contains("退出"),
            "message reports the crash: {}",
            s.message
        );
        // Quick mode crash text names cloudflared.
        assert!(
            s.message.contains("cloudflared"),
            "quick-mode crash text names cloudflared: {}",
            s.message
        );
        // Runtime was taken (self-healed) → a second status is a clean not-running.
        assert!(mgr.runtime.lock().unwrap().is_none());
    }

    /// F3 fail-fast: a cloudflared that exits BEFORE printing a URL is a failed tunnel
    /// → `Err`, not `Ok((.., None))` (which the manager would report as running). CI-
    /// safe — `false` exits 1 immediately on darwin + Linux.
    #[tokio::test]
    async fn spawn_quick_tunnel_errs_when_child_exits_without_url() {
        #[cfg(unix)]
        let (root, cloudflared) = resolved_cloudflared("exit-before-url", "#!/bin/sh\nexit 1\n");
        #[cfg(not(unix))]
        let cloudflared = ResolvedCli::for_test("false");
        let r = spawn_quick_tunnel(&cloudflared, 0).await;
        let msg = r.expect_err("child exiting before a URL must Err").message;
        assert!(
            msg.contains("已退出"),
            "error reports the early exit: {msg}"
        );
        #[cfg(unix)]
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn quick_runtime_reports_only_its_live_resolved_fingerprint() {
        let (root, cloudflared) = resolved_cloudflared(
            "live-runtime",
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then exit 0; fi\necho 'https://runtime.trycloudflare.com' >&2\nsleep 30\n",
        );
        let expected = cloudflared.fingerprint().to_string();
        let mgr = WebhookManager::default();
        mgr.set_ingestor(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));
        mgr.set_refresher(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));

        let status = mgr
            .start(
                0,
                "secret".to_string(),
                start_routes(),
                Ok(Some(cloudflared)),
                TunnelSpec {
                    mode: WebhookTunnelMode::Quick,
                    command: String::new(),
                    public_url: String::new(),
                },
            )
            .await
            .unwrap();
        assert!(status.running);
        assert_eq!(
            mgr.active_cloudflared_fingerprint().as_deref(),
            Some(expected.as_str())
        );

        mgr.stop().await;
        assert_eq!(mgr.active_cloudflared_fingerprint(), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn quick_start_and_status_preserve_resolver_error_message() {
        let root = std::env::temp_dir().join(format!(
            "prmonitor-webhook-missing-cloudflared-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let missing = root.join("cloudflared");
        let tools = CliToolsConfig {
            cloudflared_path: CliPath::try_from(missing.to_string_lossy().into_owned()).unwrap(),
            ..CliToolsConfig::default()
        };
        let resolve =
            || resolve_cli_from(&CliResolver::default(), &tools, CliTool::Cloudflared, false);
        let expected = resolve().unwrap_err().message;
        assert!(expected.contains(missing.to_string_lossy().as_ref()));

        let mgr = WebhookManager::default();
        let start_error = mgr
            .start(
                0,
                "secret".to_string(),
                start_routes(),
                Err(resolve().unwrap_err()),
                TunnelSpec {
                    mode: WebhookTunnelMode::Quick,
                    command: String::new(),
                    public_url: String::new(),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(start_error.message, expected);

        let status = mgr
            .status(Err(resolve().unwrap_err()), WebhookTunnelMode::Quick)
            .await;
        assert_eq!(status.message, expected);
    }

    /// Pure tokenization lock for `command` mode: split on whitespace, substitute every
    /// literal `{port}`, first token = program, rest = args. Exec'd directly (no shell),
    /// so this is the whole parse surface.
    #[test]
    fn build_tunnel_command_argv_splits_and_substitutes_port() {
        let (prog, args) = build_tunnel_command_argv(
            "cloudflared tunnel run --url http://127.0.0.1:{port} my-tunnel",
            8787,
        )
        .expect("non-empty command");
        assert_eq!(prog, "cloudflared");
        assert_eq!(
            args,
            vec![
                "tunnel".to_string(),
                "run".to_string(),
                "--url".to_string(),
                "http://127.0.0.1:8787".to_string(),
                "my-tunnel".to_string(),
            ]
        );

        // `{port}` substituted even when it is the whole token, and multiple
        // occurrences across tokens are all replaced.
        let (prog, args) = build_tunnel_command_argv("ngrok http {port} --log {port}", 9000)
            .expect("non-empty command");
        assert_eq!(prog, "ngrok");
        assert_eq!(
            args,
            vec![
                "http".to_string(),
                "9000".to_string(),
                "--log".to_string(),
                "9000".to_string(),
            ]
        );

        // Extra whitespace collapses (split_whitespace), and a port-only program token
        // still substitutes.
        let (prog, args) =
            build_tunnel_command_argv("  proxy-{port}   --to   {port}  ", 80).expect("non-empty");
        assert_eq!(prog, "proxy-80");
        assert_eq!(args, vec!["--to".to_string(), "80".to_string()]);

        // Blank command → None (the caller turns this into an AppError; validate also
        // rejects it upstream for command mode).
        assert!(build_tunnel_command_argv("", 8787).is_none());
        assert!(build_tunnel_command_argv("   ", 8787).is_none());
    }

    /// `command` mode `start` → `status`: spawns the user command (no URL scrape) and
    /// reports the CONFIGURED `public_url` (not a scraped one). CI-safe — `sleep` is a
    /// long-lived child on darwin + Linux, so the tunnel stays "alive" for the probe.
    #[tokio::test]
    async fn command_mode_start_reports_configured_public_url() {
        let mgr = WebhookManager::default();
        mgr.set_ingestor(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));
        mgr.set_refresher(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));

        // port 0 → OS picks a free port; `{port}` substitutes into the (harmless) sleep
        // args. cloudflared is absent on purpose — command mode must NOT require it.
        let s = mgr
            .start(
                0,
                "shh".to_string(),
                start_routes(),
                Ok(None),
                TunnelSpec {
                    mode: WebhookTunnelMode::Command,
                    command: "sleep 30 {port}".to_string(),
                    public_url: "https://my.example.com".to_string(),
                },
            )
            .await
            .expect("command-mode start succeeds without cloudflared");

        assert!(s.running);
        assert_eq!(s.public_url.as_deref(), Some("https://my.example.com"));
        assert_eq!(
            s.payload_url.as_deref(),
            Some("https://my.example.com/webhook")
        );

        // status() re-reports the configured URL while the child is alive (no self-heal).
        // The configured_mode arg is unused here (a live runtime carries its own mode);
        // pass Command to keep the call honest.
        let s2 = mgr.status(Ok(None), WebhookTunnelMode::Command).await;
        assert!(s2.running);
        assert_eq!(s2.public_url.as_deref(), Some("https://my.example.com"));

        mgr.stop().await;
    }

    /// `command` mode with a child that exits IMMEDIATELY (`true`) self-heals on the next
    /// `status` exactly like quick mode — the `Option<Child>` probe still flips a dead
    /// tunnel to not-running.
    #[tokio::test]
    async fn command_mode_self_heals_when_child_exits() {
        let mgr = WebhookManager::default();
        mgr.set_ingestor(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));
        mgr.set_refresher(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));

        let s = mgr
            .start(
                0,
                "shh".to_string(),
                start_routes(),
                Ok(None),
                TunnelSpec {
                    mode: WebhookTunnelMode::Command,
                    command: "true".to_string(), // exits 0 immediately
                    public_url: String::new(),
                },
            )
            .await
            .expect("start spawns the (short-lived) child");
        assert!(s.running);
        // Empty public_url → None.
        assert!(s.public_url.is_none());

        // The command-mode child (`true`) exits ~immediately, but `status`'s `try_wait`
        // is non-blocking, so the FIRST probe can still race the child's exit (this used
        // a separate `true`+wait as a timing proxy, which flaked on CI). Poll status until
        // the self-heal observes the exit — bounded so a genuine hang fails instead of
        // looping. The child WILL exit, so this converges deterministically.
        let mut s2 = mgr.status(Ok(None), WebhookTunnelMode::Command).await;
        for _ in 0..200 {
            if !s2.running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            s2 = mgr.status(Ok(None), WebhookTunnelMode::Command).await;
        }
        assert!(
            !s2.running,
            "an exited command-mode child flips running → false"
        );
        // Command-mode crash text is mode-specific — names the custom tunnel, NOT
        // cloudflared (which command mode doesn't use).
        assert!(
            s2.message.contains("自定义隧道") && !s2.message.contains("cloudflared"),
            "command-mode crash text names the custom tunnel, not cloudflared: {}",
            s2.message
        );
        assert!(mgr.runtime.lock().unwrap().is_none());
    }

    /// `listener` mode spawns NO child (`tunnel: None`): it stays running across
    /// repeated `status` probes (no crash self-heal — there's no child to crash) and
    /// reports the configured `public_url`. The bogus cloudflared bin proves listener
    /// mode doesn't require it.
    #[tokio::test]
    async fn listener_mode_has_no_child_and_does_not_self_heal() {
        let mgr = WebhookManager::default();
        mgr.set_ingestor(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));
        mgr.set_refresher(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));

        let s = mgr
            .start(
                0,
                "shh".to_string(),
                start_routes(),
                Ok(None),
                TunnelSpec {
                    mode: WebhookTunnelMode::Listener,
                    command: String::new(),
                    public_url: "https://external.example.com".to_string(),
                },
            )
            .await
            .expect("listener-mode start succeeds without cloudflared");
        assert!(s.running);
        assert_eq!(
            s.public_url.as_deref(),
            Some("https://external.example.com")
        );

        // No tunnel child → runtime carries `tunnel: None` / `drain_task: None`.
        {
            let guard = mgr.runtime.lock().unwrap();
            let rt = guard.as_ref().expect("runtime present");
            assert!(rt.tunnel.is_none(), "listener mode spawns no tunnel child");
            assert!(rt.drain_task.is_none(), "listener mode has no drain task");
        }

        // Probe repeatedly: a childless runtime never self-heals to not-running.
        for _ in 0..3 {
            let st = mgr.status(Ok(None), WebhookTunnelMode::Listener).await;
            assert!(st.running, "listener mode stays running across probes");
            assert_eq!(
                st.public_url.as_deref(),
                Some("https://external.example.com")
            );
        }
        assert!(
            mgr.runtime.lock().unwrap().is_some(),
            "listener runtime is never self-heal-taken"
        );

        mgr.stop().await;
    }

    /// G1: `stop` (→ `teardown`) must explicitly kill AND reap the tunnel child, not
    /// just abort tasks and lean on `kill_on_drop`. We start a long-lived `sleep` child
    /// (command mode), snapshot its OS pid, `stop`, then poll until the OS reports the
    /// pid gone — proving the child was killed (and the detached `wait` reaped it rather
    /// than leaving a zombie). CI-safe: `sleep` + `kill -0` exist on darwin + Linux.
    #[tokio::test]
    async fn stop_kills_and_reaps_tunnel_child() {
        let mgr = WebhookManager::default();
        mgr.set_ingestor(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));
        mgr.set_refresher(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));

        let s = mgr
            .start(
                0,
                "shh".to_string(),
                start_routes(),
                Ok(None),
                TunnelSpec {
                    mode: WebhookTunnelMode::Command,
                    command: "sleep 300".to_string(), // long-lived: stays alive until killed
                    public_url: String::new(),
                },
            )
            .await
            .expect("command-mode start spawns the sleep child");
        assert!(s.running);

        // Snapshot the live child's OS pid before stopping.
        let pid = {
            let guard = mgr.runtime.lock().unwrap();
            let rt = guard.as_ref().expect("runtime present");
            rt.tunnel
                .as_ref()
                .expect("command mode has a tunnel child")
                .id()
                .expect("a live child has a pid")
        };

        // `kill -0 <pid>` succeeds iff the process exists (alive OR an unreaped zombie).
        let alive = |pid: u32| {
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|st| st.success())
                .unwrap_or(false)
        };
        assert!(alive(pid), "sleep child is alive before stop");

        mgr.stop().await;
        assert!(
            mgr.runtime.lock().unwrap().is_none(),
            "stop tears down the runtime"
        );

        // Poll: the detached kill+reap is async; the pid must disappear (killed + reaped,
        // not a lingering zombie). Generous bound so a loaded CI box doesn't flake.
        let mut gone = false;
        for _ in 0..200 {
            if !alive(pid) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(gone, "stop must kill + reap the tunnel child (pid {pid})");
    }

    /// G6: `listener` mode with an EMPTY `public_url` reports `public_url: None` /
    /// `payload_url: None` (an empty config string is honestly "no URL", not a blank
    /// string), while still `running: true` (the receiver bound; the tunnel is external).
    /// Complements `listener_mode_has_no_child_and_does_not_self_heal` (non-empty URL).
    #[tokio::test]
    async fn listener_mode_empty_public_url_reports_none() {
        let mgr = WebhookManager::default();
        mgr.set_ingestor(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));
        mgr.set_refresher(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));

        let s = mgr
            .start(
                0,
                "shh".to_string(),
                start_routes(),
                Ok(None),
                TunnelSpec {
                    mode: WebhookTunnelMode::Listener,
                    command: String::new(),
                    public_url: String::new(), // empty → None, not a blank string
                },
            )
            .await
            .expect("listener-mode start succeeds without cloudflared");
        assert!(s.running, "the receiver bound — running even without a URL");
        assert!(s.public_url.is_none(), "empty public_url → None");
        assert!(s.payload_url.is_none(), "no public_url → no payload_url");

        // status() agrees: running, but no URL.
        let st = mgr.status(Ok(None), WebhookTunnelMode::Listener).await;
        assert!(st.running);
        assert!(st.public_url.is_none());
        assert!(st.payload_url.is_none());
        // Non-quick mode never nags about cloudflared.
        assert!(
            st.cloudflared_installed,
            "non-quick mode reports cloudflared_installed: true (probe skipped)"
        );

        mgr.stop().await;
    }

    /// G2/G3 (stopped path): a non-quick `configured_mode` on a STOPPED manager skips the
    /// cloudflared probe (reports `cloudflared_installed: true` despite a bogus bin) and
    /// gives a neutral "未运行" — no "brew install cloudflared" nag for a mode that
    /// doesn't use cloudflared. The quick branch's nag is still covered by the running
    /// install-state assertions elsewhere; here we lock the mode gating.
    #[tokio::test]
    async fn status_stopped_non_quick_skips_probe_and_neutral_message() {
        let mgr = WebhookManager::default();
        // No runtime → stopped; the bogus bin would make a real probe report false.
        for mode in [WebhookTunnelMode::Command, WebhookTunnelMode::Listener] {
            let st = mgr.status(Ok(None), mode).await;
            assert!(!st.running);
            assert!(
                st.cloudflared_installed,
                "non-quick stopped status skips the probe → reports installed: true"
            );
            assert_eq!(
                st.message, "未运行",
                "neutral not-running message, no brew nag"
            );
        }

        // Quick + missing cloudflared → the install nag DOES fire (probe runs, bogus bin
        // → false), proving the gating is mode-conditional and not a blanket skip.
        let st = mgr.status(Ok(None), WebhookTunnelMode::Quick).await;
        assert!(!st.running);
        assert!(!st.cloudflared_installed, "quick mode runs the probe");
        assert!(
            st.message.contains("cloudflared"),
            "quick + missing → brew nag: {}",
            st.message
        );
    }

    /// `command` mode with a blank command is a defensive `AppError` at `start` (validate
    /// rejects it upstream, but `start` must not silently spawn nothing).
    #[tokio::test]
    async fn command_mode_blank_command_errs() {
        let mgr = WebhookManager::default();
        mgr.set_ingestor(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));
        mgr.set_refresher(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));

        let r = mgr
            .start(
                0,
                "shh".to_string(),
                start_routes(),
                Ok(None),
                TunnelSpec {
                    mode: WebhookTunnelMode::Command,
                    command: "   ".to_string(),
                    public_url: String::new(),
                },
            )
            .await;
        assert!(r.is_err(), "blank command-mode command must Err");
    }

    /// F2 (now #35 routing): a verified payload whose repository matches NO enabled
    /// project's route is `WrongRepo` (fail closed) — the HMAC proves the secret is known,
    /// not that the event is for a repo this app reviews. A misconfigured webhook / reused
    /// secret on an unmonitored repo never tracks/dispatches.
    #[test]
    fn parse_delivery_requires_matching_repo() {
        // A different `base.repo.full_name` (no top-level `repository`) → WrongRepo despite
        // a valid trigger label: no route matches `evil/repo`.
        let other = pr_payload(
            &["needs-review"],
            serde_json::json!({ "base": { "repo": { "full_name": "evil/repo" } } }),
        );
        match parse_delivery(&other, &single_route("needs-review", "needs-check")) {
            ParseResult::WrongRepo { repo } => assert_eq!(repo.as_deref(), Some("evil/repo")),
            other => panic!("a payload for an unrouted repo must be WrongRepo, got {other:?}"),
        }

        // Top-level `repository.full_name` (what GitHub actually sends) is honored and
        // takes precedence: matching it routes to the matched id.
        let mut top = pr_payload(&["needs-review"], serde_json::json!({}));
        top.as_object_mut().unwrap().insert(
            "repository".to_string(),
            serde_json::json!({ "full_name": "owner/repo" }),
        );
        assert_eq!(
            routable(&top, &single_route("needs-review", "needs-check")).project_id,
            "default"
        );

        // Case-insensitive (GitHub repo-name semantics): a route for `Owner/Repo` matches
        // the event's `owner/repo`.
        let p = pr_payload(&["needs-review"], serde_json::json!({}));
        let _ = dispatch_candidate(
            &p,
            &[route(
                "default",
                "Owner/Repo",
                "needs-review",
                "needs-check",
            )],
        );

        // Missing repo entirely (no top-level `repository`, no `base.repo`) → WrongRepo
        // with `repo: None`.
        let no_repo = pr_payload(
            &["needs-review"],
            serde_json::json!({ "base": { "repo": null } }),
        );
        match parse_delivery(&no_repo, &single_route("needs-review", "needs-check")) {
            ParseResult::WrongRepo { repo } => assert!(repo.is_none()),
            other => panic!("missing repo must be WrongRepo {{ repo: None }}, got {other:?}"),
        }

        // Empty route list (no enabled projects) → nothing can match → WrongRepo.
        let any = pr_payload(&["needs-review"], serde_json::json!({}));
        assert!(matches!(
            parse_delivery(&any, &[]),
            ParseResult::WrongRepo { .. }
        ));
    }

    /// #35: with several enabled projects sharing ONE receiver, a payload routes to the
    /// project whose repo matches (NOT the first in the list) AND is classified by THAT
    /// project's labels — project B's `b-review` admits a review under B's id even though
    /// project A (a different repo, different labels) comes first. A right-repo / wrong-label
    /// event is StatusOnly (the row still updates, no dispatch — #61), NOT WrongRepo.
    #[test]
    fn parse_delivery_routes_to_matching_project_and_uses_its_labels() {
        let routes = vec![
            route("proj-a", "owner/a", "a-review", "a-check"),
            route("proj-b", "owner/b", "b-review", "b-check"),
        ];

        // A payload for owner/b carrying B's review label → routed to proj-b, kind review.
        let mut for_b = pr_payload(&["b-review"], serde_json::json!({}));
        for_b.as_object_mut().unwrap().insert(
            "repository".to_string(),
            serde_json::json!({ "full_name": "owner/b" }),
        );
        let ev = routable(&for_b, &routes);
        assert_eq!(
            ev.project_id, "proj-b",
            "routed to the matching project, not the first"
        );
        assert_eq!(
            dispatch_candidate(&for_b, &routes).kind,
            crate::model::ReviewKind::Review
        );

        // The SAME repo with project A's label still routes to proj-b. Label-to-action
        // matching is rule-engine work, not webhook routing work.
        let mut wrong_label = pr_payload(&["a-review"], serde_json::json!({}));
        wrong_label.as_object_mut().unwrap().insert(
            "repository".to_string(),
            serde_json::json!({ "full_name": "owner/b" }),
        );
        let wl = routable(&wrong_label, &routes);
        assert_eq!(wl.project_id, "proj-b");
        assert!(
            matches!(wl.intent, IngestIntent::Track { .. }),
            "project B routing keeps the event; rules decide whether A's label matters"
        );

        // A payload for owner/a with B's check label routes to proj-a and preserves the label
        // as event data for the rule engine.
        let mut for_a = pr_payload(&["b-check"], serde_json::json!({}));
        for_a.as_object_mut().unwrap().insert(
            "repository".to_string(),
            serde_json::json!({ "full_name": "owner/a" }),
        );
        let fa = routable(&for_a, &routes);
        assert_eq!(fa.project_id, "proj-a");
        assert!(
            matches!(fa.intent, IngestIntent::Track { .. }),
            "owner/a routing keeps labels as rule input"
        );
    }

    /// F7: a user-entered public URL with a trailing slash (command/listener mode) must
    /// still yield a single-slash payload URL — `https://host//webhook` would 404 every
    /// delivery against the receiver's `/webhook` route.
    #[test]
    fn webhook_status_payload_url_trims_trailing_slash() {
        let one = WebhookStatus::new(
            true,
            Some("https://example.com/".to_string()),
            true,
            "ok".to_string(),
        );
        assert_eq!(
            one.payload_url.as_deref(),
            Some("https://example.com/webhook")
        );
        // Multiple trailing slashes collapse too.
        let many = WebhookStatus::new(
            true,
            Some("https://example.com///".to_string()),
            true,
            "ok".to_string(),
        );
        assert_eq!(
            many.payload_url.as_deref(),
            Some("https://example.com/webhook")
        );
        // No trailing slash (quick-mode scraped URL): unchanged.
        let none = WebhookStatus::new(
            true,
            Some("https://x.trycloudflare.com".to_string()),
            true,
            "ok".to_string(),
        );
        assert_eq!(
            none.payload_url.as_deref(),
            Some("https://x.trycloudflare.com/webhook")
        );
    }

    /// F4: the drain loop keeps scanning AFTER the initial window and captures a URL
    /// cloudflared prints late, writing it to the shared handle (the old drain DISCARDED
    /// post-scan lines, stranding a late URL forever). Driven with an in-memory reader.
    #[tokio::test]
    async fn drain_scanning_url_captures_late_url() {
        let stderr: &[u8] =
            b"INF starting tunnel\nINF connecting...\nINF |  https://late-words.trycloudflare.com  |\nINF registered\n";
        let url = Arc::new(StdMutex::new(None));
        drain_scanning_url(BufReader::new(stderr).lines(), url.clone()).await;
        assert_eq!(
            url.lock().unwrap().as_deref(),
            Some("https://late-words.trycloudflare.com")
        );
    }

    /// The drain never OVERWRITES an already-resolved URL (the initial scan won): a later
    /// line carrying a different URL is ignored.
    #[tokio::test]
    async fn drain_scanning_url_keeps_first_resolved_url() {
        let stderr: &[u8] = b"INF |  https://second.trycloudflare.com  |\n";
        let url = Arc::new(StdMutex::new(Some(
            "https://first.trycloudflare.com".to_string(),
        )));
        drain_scanning_url(BufReader::new(stderr).lines(), url.clone()).await;
        assert_eq!(
            url.lock().unwrap().as_deref(),
            Some("https://first.trycloudflare.com")
        );
    }

    /// F3: `stop` fires graceful shutdown and AWAITS the server task, so an immediate
    /// restart on the SAME port re-binds cleanly (a bare abort raced the re-bind into
    /// "address in use"). Uses listener mode (no child / no cloudflared) on a freed
    /// ephemeral port, looped. Runs on tauri's runtime via `block_on` — the same runtime
    /// `start`'s `TcpListener::bind` + `spawn` share in production, so the listener and
    /// the server task that owns it live on ONE IO driver (a `#[tokio::test]` would bind
    /// on the test runtime but spawn the server on tauri's global one, deferring the
    /// listener's close and defeating the determinism this asserts).
    #[test]
    fn restart_on_same_port_rebinds_after_stop() {
        // Grab a likely-free port: bind to :0, read the assigned port, drop the listener.
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .expect("bind ephemeral")
            .local_addr()
            .expect("local addr")
            .port();

        tauri::async_runtime::block_on(async move {
            let mgr = WebhookManager::default();
            mgr.set_ingestor(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));
            mgr.set_refresher(Arc::new(|_, _, _| Box::pin(async { Ok(()) })));

            for i in 0..3 {
                let s = mgr
                    .start(
                        port,
                        "shh".to_string(),
                        start_routes(),
                        Ok(None),
                        TunnelSpec {
                            mode: WebhookTunnelMode::Listener,
                            command: String::new(),
                            public_url: String::new(),
                        },
                    )
                    .await
                    .unwrap_or_else(|e| {
                        panic!("restart #{i} must re-bind port {port}: {}", e.message)
                    });
                assert!(s.running, "restart #{i} running");
                mgr.stop().await;
            }
        });
    }

    // Wire-shape lock for `WebhookDelivery` (#62) — the `webhook_deliveries` command's
    // wire type, mirrored in `src/pr/types.ts` (Medium carrier per ai-robust.md; a field
    // rename would drift the TS mirror silently). Same pattern as
    // `webhook_status_wire_shape_is_camel_case`.
    #[test]
    fn webhook_delivery_wire_shape_is_camel_case() {
        let d = WebhookDelivery {
            received_at_epoch: 1_700_000_000,
            event: "pull_request".to_string(),
            action: Some("labeled".to_string()),
            repo: Some("owner/repo".to_string()),
            pr_number: Some(42),
            kind: Some("review".to_string()),
            status: DeliveryStatus::ListUpdated,
            message: None,
        };
        let v = serde_json::to_value(&d).expect("WebhookDelivery serializes");

        // camelCase keys present.
        assert!(v.get("receivedAtEpoch").is_some());
        assert!(v.get("event").is_some());
        assert!(v.get("action").is_some());
        assert!(v.get("repo").is_some());
        assert!(v.get("prNumber").is_some());
        assert!(v.get("kind").is_some());
        assert!(v.get("status").is_some());

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("received_at_epoch").is_none());
        assert!(v.get("pr_number").is_none());

        // `message: None` serializes to JSON null (not omitted), so the TS mirror's
        // `message: string | null` stays a closed contract.
        assert_eq!(v["message"], serde_json::Value::Null);
        // The status discriminator serializes camelCase (see the dedicated lock below).
        assert_eq!(v["status"], "listUpdated");
    }

    // Every `Option` field of `WebhookDelivery` serializes to JSON `null` (NOT omitted)
    // when `None`, so the TS mirror's `field: T | null` stays a closed contract (an
    // `Option` that omitted-on-None would force the TS side to also mark the field
    // optional `?`, drifting the shape). An all-None instance pins this for each field.
    #[test]
    fn webhook_delivery_all_none_fields_serialize_to_json_null() {
        let d = WebhookDelivery {
            received_at_epoch: 0,
            event: String::new(),
            action: None,
            repo: None,
            pr_number: None,
            kind: None,
            status: DeliveryStatus::Ignored,
            message: None,
        };
        let v = serde_json::to_value(&d).expect("WebhookDelivery serializes");
        for field in ["action", "repo", "prNumber", "kind", "message"] {
            assert_eq!(
                v[field],
                serde_json::Value::Null,
                "{field} must serialize to JSON null (not be omitted) when None"
            );
        }
    }

    // Cross-agent wire contract lock for `DeliveryStatus` (#62): the frontend mirrors
    // these exact camelCase strings. A variant rename or `rename_all` change surfaces here.
    #[test]
    fn delivery_status_serializes_to_pinned_wire_strings() {
        let cases = [
            (DeliveryStatus::Unauthorized, "unauthorized"),
            (DeliveryStatus::BadPayload, "badPayload"),
            (DeliveryStatus::Ignored, "ignored"),
            (DeliveryStatus::WrongRepo, "wrongRepo"),
            (DeliveryStatus::NoTriggerLabel, "noTriggerLabel"),
            (DeliveryStatus::NotOpen, "notOpen"),
            (DeliveryStatus::Gated, "gated"),
            (DeliveryStatus::ListUpdated, "listUpdated"),
            (DeliveryStatus::Refreshed, "refreshed"),
        ];
        for (status, wire) in cases {
            assert_eq!(
                serde_json::to_value(status).expect("DeliveryStatus serializes"),
                serde_json::Value::String(wire.to_string()),
                "{status:?} must serialize to {wire:?}"
            );
        }
    }

    // The delivery ring caps at DELIVERY_RING_CAP, popping the oldest (FIFO) when full,
    // and the snapshot is oldest→newest. Drives the manager's record/snapshot directly.
    #[test]
    fn delivery_ring_caps_and_snapshots_oldest_first() {
        let mgr = WebhookManager::default();
        for i in 0..(DELIVERY_RING_CAP as u64 + 10) {
            mgr.record_delivery(WebhookDelivery {
                received_at_epoch: i,
                event: "pull_request".to_string(),
                action: None,
                repo: None,
                pr_number: Some(i),
                kind: None,
                status: DeliveryStatus::Ignored,
                message: None,
            });
        }
        let snap = mgr.deliveries_snapshot();
        assert_eq!(snap.len(), DELIVERY_RING_CAP, "ring capped at the cap");
        // The 10 oldest were popped: the surviving window is [10, .., CAP+9], oldest first.
        assert_eq!(snap.first().unwrap().pr_number, Some(10));
        assert_eq!(
            snap.last().unwrap().pr_number,
            Some(DELIVERY_RING_CAP as u64 + 9)
        );
    }

    // ───────────────────────── Azure DevOps webhook (AB#822) ─────────────────────────

    #[test]
    fn verify_azure_token_accepts_correct_bearer() {
        assert!(verify_azure_token("topsecret", "Bearer topsecret"));
    }

    #[test]
    fn verify_azure_token_rejects_wrong_missing_and_malformed() {
        // Wrong token.
        assert!(!verify_azure_token("topsecret", "Bearer other"));
        // Missing `Bearer ` prefix (raw token / wrong scheme).
        assert!(!verify_azure_token("topsecret", "topsecret"));
        assert!(!verify_azure_token("topsecret", "Basic topsecret"));
        // Empty header.
        assert!(!verify_azure_token("topsecret", ""));
        // A token that is a prefix of the secret must NOT pass (length-aware compare).
        assert!(!verify_azure_token("topsecret", "Bearer top"));
        // Empty secret fails closed even with a structurally valid header.
        assert!(!verify_azure_token("", "Bearer "));
    }

    #[test]
    fn verify_azure_token_accepts_bearer_case_insensitively() {
        // RFC 7235 auth-scheme is case-insensitive; a proxy/SDK may send `bearer`/`BEARER`.
        assert!(verify_azure_token("topsecret", "bearer topsecret"));
        assert!(verify_azure_token("topsecret", "BEARER topsecret"));
    }

    #[test]
    fn route_azure_delivery_refreshes_matching_project() {
        let p = azure_pr_payload(serde_json::json!({}));
        // Routes by repo + Azure project to the project id, regardless of labels/status in the
        // payload (those are read by the subsequent az re-discovery).
        assert_eq!(azure_refresh_project(&p, &single_azure_route()), "az");
        match route_azure_delivery(&p, &single_azure_route()) {
            AzureRoute::Refresh { project_id, repo } => {
                assert_eq!(project_id, "az");
                assert_eq!(repo, "myrepo");
            }
            other => panic!("expected Refresh, got {other:?}"),
        }
    }

    #[test]
    fn route_azure_delivery_is_label_agnostic() {
        // The payload's labels (even if present) and status are IGNORED for routing — the whole
        // point of the refresh-signal design (Azure hooks don't carry labels). A payload with
        // NO labels and a non-active status still routes to Refresh.
        let p = azure_pr_payload(serde_json::json!({ "status": "completed" }));
        assert_eq!(azure_refresh_project(&p, &single_azure_route()), "az");
    }

    #[test]
    fn route_azure_delivery_wrong_repo_or_project_mismatch() {
        let routes = single_azure_route();
        // Repo name mismatch → WrongRepo.
        let mut p = azure_pr_payload(serde_json::json!({}));
        p["resource"]["repository"]["name"] = serde_json::json!("otherrepo");
        assert!(matches!(
            route_azure_delivery(&p, &routes),
            AzureRoute::WrongRepo { .. }
        ));
        // Same repo name but DIFFERENT Azure project → WrongRepo (the project guard).
        let mut p2 = azure_pr_payload(serde_json::json!({}));
        p2["resource"]["repository"]["project"]["name"] = serde_json::json!("otherproject");
        assert!(matches!(
            route_azure_delivery(&p2, &routes),
            AzureRoute::WrongRepo { .. }
        ));
    }

    #[test]
    fn route_azure_delivery_malformed_without_resource_repo_or_project() {
        let routes = single_azure_route();
        // No `resource`.
        assert!(matches!(
            route_azure_delivery(
                &serde_json::json!({ "eventType": "git.pullrequest.updated" }),
                &routes
            ),
            AzureRoute::Malformed
        ));
        // `repository.name` absent → Malformed (a real Azure PR hook always carries it).
        let mut p = azure_pr_payload(serde_json::json!({}));
        p["resource"]["repository"]
            .as_object_mut()
            .unwrap()
            .remove("name");
        assert!(matches!(
            route_azure_delivery(&p, &routes),
            AzureRoute::Malformed
        ));
        // `project.name` null → Malformed.
        let mut p2 = azure_pr_payload(serde_json::json!({}));
        p2["resource"]["repository"]["project"]["name"] = serde_json::json!(null);
        assert!(matches!(
            route_azure_delivery(&p2, &routes),
            AzureRoute::Malformed
        ));
        // `repository` object entirely absent → Malformed.
        let mut p3 = azure_pr_payload(serde_json::json!({}));
        p3["resource"].as_object_mut().unwrap().remove("repository");
        assert!(matches!(
            route_azure_delivery(&p3, &routes),
            AzureRoute::Malformed
        ));
    }

    #[test]
    fn route_azure_delivery_matches_repo_and_project_case_insensitively() {
        let routes = single_azure_route(); // repo "myrepo", project "myproject"
        let mut p = azure_pr_payload(serde_json::json!({}));
        p["resource"]["repository"]["name"] = serde_json::json!("MyRepo");
        p["resource"]["repository"]["project"]["name"] = serde_json::json!("MyProject");
        assert_eq!(azure_refresh_project(&p, &routes), "az");
    }

    #[test]
    fn provider_isolation_azure_payload_does_not_match_github_route_and_vice_versa() {
        // An Azure payload must NOT route to a GitHub-source route even if the bare repo name
        // string would `eq_ignore_ascii_case`-match (the source_kind guard rejects it).
        let gh_routes = vec![route("gh", "myrepo", "needs-review", "needs-check")];
        let azure_p = azure_pr_payload(serde_json::json!({}));
        assert!(matches!(
            route_azure_delivery(&azure_p, &gh_routes),
            AzureRoute::WrongRepo { .. }
        ));
        // Symmetrically, a GitHub payload must not route to an Azure-source route.
        let gh_p = pr_payload(&["needs-review"], serde_json::json!({}));
        assert!(matches!(
            parse_delivery(&gh_p, &single_azure_route()),
            ParseResult::WrongRepo { .. }
        ));
    }
}
