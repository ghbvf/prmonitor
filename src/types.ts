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
// Single-arm today; widening tracked by #11.
export type SourceKind = "github"; // 未来 #11: | "gitlab" | "bitbucket"
export type EngineKind = "codex"; // 未来 #11: | "claude"

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
