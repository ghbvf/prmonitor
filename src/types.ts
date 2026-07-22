// Shared cross-slice contracts mirroring `src-tauri/src/model.rs` (the contract
// boundary). Slices import from here; they do not import each other's internals.
import { ACTION_STATUSES as GENERATED_ACTION_STATUSES } from "./types.generated";
import type {
  ActionStatus,
  EventEnvelope,
  EventType,
  PullRequestView,
  SourceKind,
  UpdateMode,
} from "./types.generated";

export {
  CLI_RESOLUTION_SOURCES,
  CLI_TOOLS,
  CLAUDE_EFFORTS,
  CODEX_REASONING_EFFORTS,
  ENGINE_KINDS,
  EVENT_TYPES,
  externalRequestId,
  inboxDedupeKey,
  inboxEventId,
  LABEL_SOURCES,
  NOTIFICATION_LEVELS,
  outboxProducerKey,
  reviewActionKey,
  reviewReceiptId,
  SOURCE_KINDS,
  UPDATE_MODES,
} from "./types.generated";
export type {
  ClaudeEffort,
  CliResolutionSource,
  CliTool,
  CodexReasoningEffort,
  EngineKind,
  EventType,
  LabelSource,
  NotificationLevel,
  RuleMatchEntry,
  SendNotificationRequest,
  SendNotificationResponse,
  SourceKind,
  UpdateMode,
  ActionStatus,
  EventEnvelope,
  EventPayload,
  ExternalRequestId,
  ExternalTriggerOrigin,
  InboxDedupeKey,
  InboxEventId,
  OutboxProducerKey,
  PullRequestView,
  ReviewActionKey,
  ReviewReceiptId,
} from "./types.generated";

// Exhaustiveness guard for discriminated unions / string-literal enums: in a
// `default`/`else` branch, `assertNever(x)` only type-checks if `x` has been
// narrowed to `never`, so adding an arm without handling it is a COMPILE error
// (#50 review G8, Medium — `assertNever`穷尽). Throws at runtime as a fail-safe
// for values that bypass the type system (e.g. malformed wire data).
export function assertNever(x: never): never {
  throw new Error(`Unexpected value: ${String(x)}`);
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

// Mirrors Rust `SkillInvocation::migrate_legacy_skill_key` (load-only legacy wire).
function migrateLegacySkillKey(skillKey: string): string {
  if (skillKey === "review") return "pr-review\0";
  if (skillKey === "check") return "pr-review\0--check";
  return skillKey;
}

// Human-readable skill identity (`pr-review` or `pr-review --check`), mirrors
// Rust `SkillInvocation::display_label`.
export function skillKeyLabel(skillKey: string): string {
  const key = migrateLegacySkillKey(skillKey);
  const nul = key.indexOf("\0");
  if (nul < 0) return key || "—";
  const name = key.slice(0, nul);
  const extra = key.slice(nul + 1);
  if (!name && !extra) return "—";
  return extra ? `${name} ${extra}` : name;
}

// Extra args segment of a skill key (after `\0`), for manual start / remote review.
export function extraArgsFromSkillKey(skillKey: string): string {
  const key = migrateLegacySkillKey(skillKey);
  const nul = key.indexOf("\0");
  return nul >= 0 ? key.slice(nul + 1) : "";
}

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

// Rust's private-field `EventEnvelope` is the wire source. Its payload union keeps observations
// and explicit review requests distinct, so configured observation rules cannot consume a command.
export type Event = EventEnvelope;

export interface EventPresentation {
  eventType: EventType;
  number: number | null;
  title: string;
}

export function presentEvent(event: EventEnvelope): EventPresentation {
  switch (event.payload.kind) {
    case "observation":
      return {
        eventType: event.payload.eventType,
        number: event.payload.subject.number,
        title: event.payload.subject.title,
      };
    case "reviewRequest":
      return {
        eventType: "pullRequest",
        number: event.payload.prNumber,
        title: event.payload.extraArgs.trim()
          ? `${event.payload.skillName} ${event.payload.extraArgs.trim()} request`
          : `${event.payload.skillName} request`,
      };
    default:
      return assertNever(event.payload);
  }
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
export type InboxEvent =
  | {
      kind: "updated";
      projectId: string;
      entry: InboxEntry;
    }
  | {
      kind: "error";
      operation: "retention" | "worker";
      message: string;
    };

// ── Action outbox contracts (AB#1066, epic AB#1078) ───────────────────────────────
// The outbox is the symmetric counterpart to the inbox: it retains each outbound ACTION the
// rule engine enqueued, with a retry lifecycle (attempt count / next-attempt time / last
// error) and a dead-letter terminal. It mirrors the inbox's `as const` + `assertNever` carrier
// shape so a new status/kind can't drift past the rendered labels.

// The retry lifecycle status of an outbox entry — mirrors the Rust `ActionStatus` enum's
// camelCase serde form (locked by the model.rs golden test). Single-sourced as an `as const`
// array (mirrors INBOX_STATUSES / EVENT_TYPES): the type is DERIVED from the array, so the
// literal set is Hard — a value outside `["pending","done","dead"]` is un-expressible. The
// `outboxStatusLabel` switch below is the Medium `assertNever`穷尽 carrier on top of it.
export const OUTBOX_STATUSES = GENERATED_ACTION_STATUSES;
export type OutboxStatus = ActionStatus;

// A human-readable label for an OutboxStatus, rendered as the outbox row's status badge.
// `default: assertNever(s)` (Medium — `assertNever`穷尽, same carrier class as
// `inboxStatusLabel`): adding an OutboxStatus without a label arm here is a COMPILE error, so
// the status set and the rendered badges can't drift.
export function outboxStatusLabel(s: OutboxStatus): string {
  switch (s) {
    case "pending":
      return "待执行 / Pending";
    case "blocked":
      return "等待恢复 / Blocked";
    case "done":
      return "已完成 / Done";
    case "dead":
      return "最终失败 / Dead-letter";
    default:
      return assertNever(s);
  }
}

// The kind of action an outbox entry performs — mirrors the Rust `ActionKind` enum's camelCase
// serde form (locked by the model.rs golden test). Single-sourced as an `as const` array
// (mirrors OUTBOX_STATUSES): the type is DERIVED from the array. `notification` (AB#1066) +
// `runSkill`/`stopReview` (skill actions — reuse the review funnel) +
// `messagingReply` (#1559 bot reply executor) + `messagingSend` active messaging sends.
// Email/IM notification channels are still
// `NotificationKind` variants under `notification`.
export const ACTION_KINDS = [
  "notification",
  "runSkill",
  "stopReview",
  "messagingReply",
  "messagingSend",
] as const;
export type ActionKind = (typeof ACTION_KINDS)[number];

// A human-readable label for an ActionKind, rendered as the outbox row's kind tag.
// `default: assertNever(k)` (Medium — `assertNever`穷尽, same carrier class as
// `outboxStatusLabel`): adding an ActionKind without a label arm here is a COMPILE error.
export function outboxKindLabel(k: ActionKind): string {
  switch (k) {
    case "notification":
      return "通知 / Notification";
    case "runSkill":
      return "运行 Skill / Run Skill";
    case "stopReview":
      return "停止评审 / Stop Review";
    case "messagingReply":
      return "消息回复 / Messaging Reply";
    case "messagingSend":
      return "消息发送 / Messaging Send";
    default:
      return assertNever(k);
  }
}

// A retained outbox entry (AB#1066) — mirrors `model.rs::OutboxEntry` (Serialize-only, serde
// camelCase; locked by the model.rs golden test). Unlike `InboxEntry`, the raw action payload
// is NOT embedded: it's fetched on demand via `outboxGetRaw` (the "查看原始" disclosure). The
// retry fields drive the row's "重试中 (N)" badge / next-attempt readout. `projectId` is at the
// TOP level (unlike InboxEntry, whose projectId is nested under `.event.projectId`). `lastError`
// is non-null only for an entry that has failed at least once.
export interface OutboxEntry {
  id: number;
  projectId: string;
  kind: ActionKind;
  summary: string;
  status: OutboxStatus;
  attemptCount: number;
  nextAttemptAt: number;
  lastError: string | null;
  createdAt: number;
  updatedAt: number;
}

// Mirrors the `outbox:updated` Tauri event payload (AB#1066) — the downstream end of the
// event-name funnel for the outbox push stream (the upstream `OUTBOX_UPDATED_EVENT` name lives
// in `outbox/api.ts`). A discriminated union (discriminant `kind`) mirroring `InboxEvent` /
// `PrEvent`. The `updated` arm carries one row's upsert (`projectId` (#35) routes it to its
// monitored project); the `error` arm (AB#1182) is a worker-CYCLE-level failure NOT tied to a row
// or project (so it has NO `projectId` — the panel shows it as a global banner). `operation` names
// the failing site (`claim`/`record`/`announce`). Keep this 2-arm shape in lockstep with the Rust
// `events.rs::OutboxEvent` (the open end of the funnel); `useOutboxStore.subscribe`'s `assertNever`
// switch is the Medium exhaustiveness carrier.
export type OutboxEvent =
  | { kind: "updated"; projectId: string; entry: OutboxEntry }
  | { kind: "error"; operation: string; message: string };

// ── Workflow / saga contracts (#1370) ────────────────────────────────────────────
export const WORKFLOW_TYPES = ["reviewNotify"] as const;
export type WorkflowType = (typeof WORKFLOW_TYPES)[number];

export const WORKFLOW_STATUSES = ["pending", "running", "waiting", "done", "failed"] as const;
export type WorkflowStatus = (typeof WORKFLOW_STATUSES)[number];

export const WORKFLOW_STEPS = ["startReview", "waitReview", "enqueueNotify", "done"] as const;
export type WorkflowStep = (typeof WORKFLOW_STEPS)[number];

export function workflowStatusLabel(s: WorkflowStatus): string {
  switch (s) {
    case "pending":
      return "待启动 / Pending";
    case "running":
      return "运行中 / Running";
    case "waiting":
      return "等待中 / Waiting";
    case "done":
      return "完成 / Done";
    case "failed":
      return "失败 / Failed";
    default:
      return assertNever(s);
  }
}

export function workflowStepLabel(s: WorkflowStep): string {
  switch (s) {
    case "startReview":
      return "启动 review / Start review";
    case "waitReview":
      return "等待完成 / Wait review";
    case "enqueueNotify":
      return "入队通知 / Enqueue notify";
    case "done":
      return "完成 / Done";
    default:
      return assertNever(s);
  }
}

export interface ReviewNotifyInput {
  reference: string;
  prNumber: number;
  skillKey: string;
}

export interface ReviewNotifyState {
  reviewThreadId?: string;
  reviewWireStatus?: string;
  commentUrl?: string | null;
  notificationOutboxIds?: number[];
}

interface WorkflowInstanceBase<T extends WorkflowType, I, S> {
  id: number;
  projectId: string;
  type: T;
  status: WorkflowStatus;
  currentStep: WorkflowStep;
  input: I;
  state: S;
  attemptCount: number;
  nextWakeAt: number;
  lastError: string | null;
  createdAt: number;
  updatedAt: number;
}

export type ReviewNotifyWorkflowInstance = WorkflowInstanceBase<
  "reviewNotify",
  ReviewNotifyInput,
  ReviewNotifyState
>;

export type WorkflowInstance = ReviewNotifyWorkflowInstance;

export type WorkflowEvent =
  | { kind: "updated"; projectId: string; instance: WorkflowInstance }
  | { kind: "error"; operation: string; message: string };

// ── Remote terminal contracts (#1383, #1372) ──────────────────────────────────────
// Mirror `src-tauri/src/model.rs` (TerminalSession / CreateSessionOpts) + `events.rs`
// (TerminalEvent), serde camelCase, locked by the model.rs / events.rs golden tests
// (the open downstream end of those funnels; a Rust-side rename surfaces in those goldens,
// and this mirror must be synced in lockstep — future Hard path = codegen this from Rust).

// Which terminal backend drives a session (#1372): `iterm` = the AppleScript/iTerm daemon
// (full screen-snapshot frames); `webPty` = a real PTY shell (raw incremental byte stream).
// Single-sourced as an `as const` array (mirrors SOURCE_KINDS / ENGINE_KINDS): the type is
// DERIVED from the array, so the literal set is Hard — a backend value outside this list is
// un-expressible. `terminalBackendLabel` below is the Medium `assertNever`穷尽 carrier on top.
// Wire values mirror the Rust `TerminalBackend` camelCase serde form (locked by the model.rs
// golden test). NOTE the lowercase-i `"iterm"` and camelCase `"webPty"`.
export const TERMINAL_BACKENDS = ["iterm", "webPty"] as const;
export type TerminalBackend = (typeof TERMINAL_BACKENDS)[number];

// A human-readable label for a TerminalBackend, rendered as the active session's header badge.
// `default: assertNever(b)` (Medium — `assertNever`穷尽, same carrier class as `eventTypeLabel`):
// adding a TerminalBackend without a label arm here is a COMPILE error, so the backend set and
// the rendered badge can't drift past the `as const` array.
export function terminalBackendLabel(b: TerminalBackend): string {
  switch (b) {
    case "iterm":
      return "iTerm";
    case "webPty":
      return "Shell";
    default:
      return assertNever(b);
  }
}

// One addressable terminal session — the leaf the xterm panel attaches to. `sessionId` is the
// session GUID (the attach key); `windowId`/`tabId` group it in the picker (the slice flattens
// this list then re-groups window → tab → session client-side; a PTY shell groups under the
// backend-supplied synthetic `"webpty"` window/tab); `rows`/`cols` are the current grid.
// `backend` (#1372) is ALWAYS present on the wire — it tags which backend owns the session so the
// UI can label it and gate backend-specific controls (PTY-only "stop process"). Mirrors
// `model.rs::TerminalSession`.
export interface TerminalSession {
  sessionId: string;
  windowId: string;
  tabId: string;
  title: string;
  isActive: boolean;
  rows: number;
  cols: number;
  backend: TerminalBackend;
}

// Options for `create_terminal_session`. All optional (omitted = daemon picks defaults). `backend`
// (#1372) selects the backend; OMITTED ⇒ the backend defaults to iterm (serde `skip_serializing_if`
// → an absent key, not null — the daemon applies its own default). Mirrors `model.rs::CreateSessionOpts`.
export interface CreateSessionOpts {
  windowId?: string;
  profile?: string;
  backend?: TerminalBackend;
}

// One streamed unit on the `terminal:event` Tauri channel. Tagged `kind` (mirrors
// ReviewEvent / PrEvent), camelCase. Two distinct render paths by backend:
//   • `screenUpdate` (iTerm) carries a FULL visible-screen snapshot — the panel renders each
//     frame with `term.reset()` + `term.write(contents)`, then repositions the cursor when
//     `cursorRow`/`cursorCol` are present.
//   • `output` (#1372, PTY) carries `data` = base64 of raw incremental PTY bytes — the panel
//     decodes to bytes and does an INCREMENTAL `term.write(bytes)` (no reset). At most one of
//     the two arms is emitted for a given session (the backend picks the model).
// Keep this in lockstep with `events.rs::TerminalEvent`; the store's `applyEvent` `assertNever`
// default is the Medium exhaustiveness carrier.
export type TerminalEvent =
  | { kind: "attached"; sessionId: string; cols: number; rows: number }
  | {
      kind: "screenUpdate";
      sessionId: string;
      cols: number;
      rows: number;
      contents: string;
      cursorRow?: number;
      cursorCol?: number;
    }
  | { kind: "output"; sessionId: string; data: string }
  | { kind: "sessionEnded"; sessionId: string; reason: string }
  // `sessionId` omitted for a connection-level error not tied to one session.
  | { kind: "error"; sessionId?: string; message: string };
