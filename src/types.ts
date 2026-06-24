// Shared cross-slice contracts mirroring `src-tauri/src/model.rs` (the contract
// boundary). Slices import from here; they do not import each other's internals.

// Exhaustiveness guard for discriminated unions / string-literal enums: in a
// `default`/`else` branch, `assertNever(x)` only type-checks if `x` has been
// narrowed to `never`, so adding an arm without handling it is a COMPILE error
// (#50 review G8, Medium — `assertNever`穷尽). Throws at runtime as a fail-safe
// for values that bypass the type system (e.g. malformed wire data).
export function assertNever(x: never): never {
  throw new Error(`Unexpected value: ${String(x)}`);
}

export interface PullRequestView {
  number: number;
  title: string;
  labels: string[];
  url: string;
  kind: string; // "review" | "check" — the trigger-label mode
  skipReason: string | null; // null = would dispatch; string = why it is skipped
}

// Tracking presence for a retained PR (#38): "current" = seen in the latest
// discovery; "stale" = previously seen, no longer active. Mirrors the Rust
// `Presence` enum's camelCase wire values.
export type PrPresence = "current" | "stale";

// The retained, tracking-aware row pushed by the backend (#38). Flattened on the
// wire: extends the `PullRequestView` contract with the tracking fields, so the
// Rust↔TS mirror stays a strict superset of `PullRequestView`.
export interface TrackedPrView extends PullRequestView {
  presence: PrPresence;
  archived: boolean;
}

// Discriminator unions mirroring the `SourceKind` / `EngineKind` Rust enums.
// SourceKind widens to Azure DevOps (818) and Bitbucket Server/Data Center (717),
// single-sourced as an `as const` array (mirrors UPDATE_MODES / WEBHOOK_TUNNEL_MODES):
// the type is DERIVED from the array, and fields.ts feeds the same array into the
// sourceKind select `options`, so the type and the UI's option list can never drift.
// 未来 #11: add "gitlab" here.
export const SOURCE_KINDS = ["github", "azure", "bitbucket"] as const;
export type SourceKind = (typeof SOURCE_KINDS)[number];
// #718: review engines, single-sourced as an `as const` array (mirrors SOURCE_KINDS /
// UPDATE_MODES). The type is DERIVED from the array, and fields.ts feeds the same array
// into the engineKind select `options`, so the type and the UI option list can't drift.
// Wire values mirror the Rust `EngineKind` camelCase serde form (locked by a golden test).
export const ENGINE_KINDS = ["codex", "claude"] as const;
export type EngineKind = (typeof ENGINE_KINDS)[number];

// Where a project's trigger labels come from (717) — mirrors the Rust `LabelSource`
// enum's camelCase wire values. native = use the provider's own PR labels; title =
// parse `[..]` bracket tags out of the PR title (e.g. `[pr-status/need-fix]`).
// Default is "native". Bitbucket Server has no native PR labels, so a Bitbucket source
// MUST use "title".
//
// Single-sourced as an `as const` array (mirrors SOURCE_KINDS / UPDATE_MODES): the type
// is DERIVED from the array, and fields.ts feeds the same array into the labelSource
// select `options`, so the type and the UI's option list can never drift. (The Rust↔TS
// mirror remains a separate, golden-locked contract.)
export const LABEL_SOURCES = ["native", "title"] as const;
export type LabelSource = (typeof LABEL_SOURCES)[number];

// Per-project data-update mode (818) — mirrors the Rust `UpdateMode` enum's
// camelCase wire values. webhook-only = default, no CLI polling (push-driven);
// pull-only / hybrid = run the CLI poll loop (may trigger account/API risk control);
// manual = no automatic updates, user pulls on demand.
//
// Single-sourced as an `as const` array (mirrors WEBHOOK_TUNNEL_MODES at
// src/config/types.ts): the type is DERIVED from the array, and fields.ts feeds the
// same array into the select `options`, so the type and the UI's option list can
// never drift. Adding/renaming a mode = edit this one array. (The Rust↔TS mirror
// remains a separate, golden-locked contract.)
export const UPDATE_MODES = ["webhook-only", "pull-only", "hybrid", "manual"] as const;
export type UpdateMode = (typeof UPDATE_MODES)[number];

// Whether a data-update mode runs the CLI poll loop (818). webhook-only / manual =
// push-driven / on-demand (no CLI polling); pull-only / hybrid = the loop runs. Lives
// at the shared `src/` contract root (next to its `assertNever` carrier) because BOTH
// the config slice (fields.ts) and the pr slice (PollControls.vue) gate on it — a
// per-slice copy would violate the vertical-slice boundary (slice-boundary.test.ts).
//
// The `default` arm is `assertNever(mode)` (Medium — `assertNever`穷尽, same carrier as
// above): adding a new UpdateMode without an arm here is a COMPILE error, so the mode
// list and this gate can never silently fall out of sync.
export function pollingEnabledForMode(mode: UpdateMode): boolean {
  switch (mode) {
    case "webhook-only":
      return false;
    case "pull-only":
    case "hybrid":
      return true;
    case "manual":
      return false;
    default:
      return assertNever(mode);
  }
}

// The source-side CLI/REST tool a project surfaces, keyed by `SourceKind`:
// GitHub→`gh`, Azure→`az`, Bitbucket→its REST API (labelled `bitbucket`, no CLI).
// Adding a source (e.g. `gitlab`, 未来 #11) is a COMPILE error in the assertNever
// switches below until its tool is mapped — the StatusBar can't silently miss it.
export type SourceTool = "gh" | "az" | "bitbucket";

// Which source tool the StatusBar should surface for this project, or `null` when the
// mode runs NO discovery for that source. GitHub/Bitbucket discover only via the poll
// loop OR a manual one-shot pull, so webhook-only drops out (GitHub's webhook path
// classifies from the payload — no `gh`; Bitbucket has no webhook handler). AZURE is the
// exception: its webhook path is a refresh signal that RE-RUNS `az` discovery
// (src-tauri/src/pr/webhook.rs → SchedulerSet::discover_once → `az repos pr list`), so
// `az` is invoked in EVERY azure mode — always surface it. assertNever 穷尽 (Medium): a
// new SourceKind must be classified here.
export function statusBarSourceTool(
  sourceKind: SourceKind,
  mode: UpdateMode,
): SourceTool | null {
  const discovers = pollingEnabledForMode(mode) || manualPullAllowedForMode(mode);
  switch (sourceKind) {
    case "github":
      return discovers ? "gh" : null;
    case "azure":
      return "az"; // poll/manual call `az`; webhook re-runs `az` discovery — always used
    case "bitbucket":
      return discovers ? "bitbucket" : null;
    default:
      return assertNever(sourceKind);
  }
}

// Which source CLI the active project's AUTOMATIC review pipeline depends on (a failed
// auth pauses auto review), or `null`. GitHub auto-discovers via `gh` only in the poll
// loop (pull-only/hybrid) — its webhook path classifies from the payload, and manual is
// on-demand. AZURE re-runs `az` in BOTH the poll loop AND the webhook refresh signal, so
// every auto-updating azure mode (webhook-only/pull-only/hybrid) needs `az`; manual azure
// is on-demand only, so it's excluded to avoid a false "paused" when no webhook is set up.
// Bitbucket uses a REST token (no live-blockable CLI). Drives the App.vue "自动 review 已
// 暂停" banner. assertNever 穷尽 (Medium).
export function autoReviewSourceCli(
  sourceKind: SourceKind,
  mode: UpdateMode,
): "gh" | "az" | null {
  switch (sourceKind) {
    case "github":
      return pollingEnabledForMode(mode) ? "gh" : null;
    case "azure":
      return mode === "manual" ? null : "az";
    case "bitbucket":
      return null;
    default:
      return assertNever(sourceKind);
  }
}

// Whether a mode supports the manual one-shot "立即拉取" (poll-now) trigger (818, F7).
// This is a DIFFERENT capability from the periodic poll loop (pollingEnabledForMode):
// the backend `poll_now` Manual branch runs a one-shot `discover_once`, so manual mode
// DOES support an on-demand pull — only webhook-only (purely push-driven) does not.
// Mirrors the backend `poll_now` gate. Exhaustive over UpdateMode via the `assertNever`
// default (Medium — `assertNever`穷尽), so a new mode forces a decision here.
export function manualPullAllowedForMode(mode: UpdateMode): boolean {
  switch (mode) {
    case "webhook-only":
      return false;
    case "pull-only":
    case "hybrid":
    case "manual":
      return true;
    default:
      return assertNever(mode);
  }
}

// Whether a project's PERIODIC poll loop should be running: the project is enabled AND
// its mode runs the loop. Mirrors the backend `periodic_polling` gate
// (src-tauri/src/pr/scheduler.rs: `p.enabled && (PullOnly || Hybrid)`), so the frontend's
// optimistic "running" flag matches what the scheduler actually starts — a disabled or
// webhook-only/manual project gets no loop, so it must read as not-running rather than
// the old blanket optimistic true (#150 F1b). `enabled` is the dimension the mode-only
// `pollingEnabledForMode` gate omitted.
export function periodicPollEligible(enabled: boolean, mode: UpdateMode): boolean {
  return enabled && pollingEnabledForMode(mode);
}

// Whether the manual one-shot "立即拉取" should be offered: the project is enabled AND its
// mode supports an on-demand pull. A disabled project is fully off (the backend skips it
// in validation/scheduling/webhook routing), so the UI offers it no poll actions either
// (#150 F1b) — `enabled` is the dimension the mode-only `manualPullAllowedForMode` omitted.
export function manualPullEligible(enabled: boolean, mode: UpdateMode): boolean {
  return enabled && manualPullAllowedForMode(mode);
}

// Every arm carries `projectId` (#35): events fan out per monitored project, so the
// frontend routes each payload to the project it belongs to. Discriminant stays `kind`.
export type ReviewEvent =
  | { kind: "messageDelta"; projectId: string; threadId: string; itemId: string; text: string }
  | { kind: "reasoningDelta"; projectId: string; threadId: string; itemId: string; text: string }
  // `commentUrl` (AB#1042): the resolved pr-review comment URL, present on a `completed`
  // turn when the source kind can resolve one (GitHub: exact comment URL; Azure: PR URL;
  // Bitbucket: absent), else omitted/undefined. Mirrors `events.rs::TurnCompleted`'s
  // optional `comment_url` (serde camelCase; locked by the events.rs golden test).
  | { kind: "turnCompleted"; projectId: string; threadId: string; status: string; commentUrl?: string }
  | { kind: "error"; projectId: string; threadId: string; message: string }
  // Session-less auto-trigger (#8) notice — no threadId (mirrors
  // `events.rs::ReviewEvent::DispatchError`; locked by a serde golden test).
  | { kind: "dispatchError"; projectId: string; message: string };

// Mirrors `events.rs::PrEvent` (tagged `kind`, camelCase) — the funnel's
// downstream end for the `prs:updated` Tauri event payload. `projectId` (#35)
// routes each update to its monitored project.
export type PrEvent =
  | { kind: "updated"; projectId: string; prs: TrackedPrView[] }
  | { kind: "error"; projectId: string; message: string };

// ── Event pipeline contracts (AB#1079, epic AB#1078) ──────────────────────────────
// The normalized inbound-event envelope shared by the inbox (1065) / rule engine (1068) /
// outbox (1066). Mirrors `model.rs::Event` + `EventType` (the event-pipeline keystone).

// The class of a normalized event — mirrors the Rust `EventType` enum's camelCase wire
// values (locked by the model.rs golden test). Single-sourced as an `as const` array
// (mirrors SOURCE_KINDS / ENGINE_KINDS): the type is DERIVED from the array. Default is
// "pullRequest" (the only class the current webhook path emits).
export const EVENT_TYPES = ["pullRequest", "issue", "comment", "label", "generic"] as const;
export type EventType = (typeof EVENT_TYPES)[number];

// A normalized inbound event (AB#1079) — mirrors `model.rs::Event` (serde camelCase; locked
// by the model.rs golden test). Generalizes the webhook event with a cross-source identity
// (`source` / `eventType`) and the `dedupeKey` the inbox dedups on. `number` is null for an
// event class with no PR/issue number (a generic webhook), mirroring the Rust `Option<u64>`.
export interface Event {
  dedupeKey: string;
  source: SourceKind;
  eventType: EventType;
  projectId: string;
  repo: string;
  number: number | null;
  title: string;
  body: string;
  labels: string[];
  url: string;
  receivedAtEpoch: number;
}

// A human-readable label for an EventType, rendered as the inbox row's type tag (AB#1065).
// THIS is the FIRST frontend consumer of `event.eventType`: before it, the EVENT_TYPES `as
// const` set (Hard — the literal set can't drift) had no downstream switch, so adding an
// EventType arm was un-enforced past the array (Soft, eyeball-only). This switch closes that
// gap — its `default` arm is `assertNever(t)` (Medium — `assertNever`穷尽, same carrier as
// `pollingEnabledForMode` above): adding a new EventType without a label arm here is a COMPILE
// error, so the wire enum and the inbox's rendered labels can never silently fall out of sync.
export function eventTypeLabel(t: EventType): string {
  switch (t) {
    case "pullRequest":
      return "拉取请求 / Pull Request";
    case "issue":
      return "议题 / Issue";
    case "comment":
      return "评论 / Comment";
    case "label":
      return "标签 / Label";
    case "generic":
      return "通用 / Generic";
    default:
      return assertNever(t);
  }
}

// ── Event inbox contracts (AB#1065, epic AB#1078) ─────────────────────────────────
// The inbox is the first persistence layer for normalized events: it retains each inbound
// `Event` with a processing `status`, the (optional) processed timestamp, and any error.

// The lifecycle status of an inbox entry — mirrors the Rust `InboxStatus` enum's camelCase
// serde form (locked by the model.rs golden test). Single-sourced as an `as const` array
// (mirrors EVENT_TYPES / SOURCE_KINDS): the type is DERIVED from the array, so the literal
// set is Hard — a value outside `["received","processed","failed"]` is un-expressible. The
// `inboxStatusLabel` switch below is the Medium `assertNever`穷尽 carrier on top of it.
export const INBOX_STATUSES = ["received", "processed", "failed"] as const;
export type InboxStatus = (typeof INBOX_STATUSES)[number];

// A human-readable label for an InboxStatus, rendered as the inbox row's status badge.
// `default: assertNever(s)` (Medium — `assertNever`穷尽, second carrier next to
// `eventTypeLabel`): adding an InboxStatus without a label arm here is a COMPILE error, so the
// status set and the rendered badges can't drift.
export function inboxStatusLabel(s: InboxStatus): string {
  switch (s) {
    case "received":
      return "已接收 / Received";
    case "processed":
      return "已处理 / Processed";
    case "failed":
      return "失败 / Failed";
    default:
      return assertNever(s);
  }
}

// A retained inbox entry (AB#1065) — mirrors `model.rs::InboxEntry` (serde camelCase; locked
// by the model.rs golden test). The inbound `event` is NESTED (not flattened): the inbox wraps
// the normalized `Event` contract with its own lifecycle fields. `processedAtEpoch` is null
// until the entry leaves the `received` state; `error` is non-null only for a `failed` entry.
export interface InboxEntry {
  id: number;
  event: Event;
  status: InboxStatus;
  processedAtEpoch: number | null;
  error: string | null;
}

// Mirrors the `inbox:updated` Tauri event payload (AB#1065) — the downstream end of the
// event-name funnel for the inbox push stream (the upstream `INBOX_UPDATED_EVENT` name lives
// in `inbox/api.ts`). A single-arm tagged union (discriminant `kind`) mirroring the `PrEvent`
// / `ReviewEvent` shape, so it can widen later without churning the call sites. `projectId`
// (#35) routes each upsert to its monitored project.
export type InboxEvent = {
  kind: "updated";
  projectId: string;
  entry: InboxEntry;
};
