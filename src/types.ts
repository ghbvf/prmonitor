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
export type EngineKind = "codex"; // 未来 #11: | "claude"

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

// Every arm carries `projectId` (#35): events fan out per monitored project, so the
// frontend routes each payload to the project it belongs to. Discriminant stays `kind`.
export type ReviewEvent =
  | { kind: "messageDelta"; projectId: string; threadId: string; itemId: string; text: string }
  | { kind: "reasoningDelta"; projectId: string; threadId: string; itemId: string; text: string }
  | { kind: "turnCompleted"; projectId: string; threadId: string; status: string }
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
