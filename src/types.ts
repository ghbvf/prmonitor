// Shared cross-slice contracts mirroring `src-tauri/src/model.rs` (the contract
// boundary). Slices import from here; they do not import each other's internals.

export interface PullRequestView {
  number: number;
  title: string;
  labels: string[];
  url: string;
  kind: string; // "review" | "check" — the trigger-label mode
  skipReason: string | null; // null = would dispatch; string = why it is skipped
}

// Discriminator unions mirroring the `SourceKind` / `EngineKind` Rust enums.
// Single-arm today; widening tracked by #11.
export type SourceKind = "github"; // 未来 #11: | "gitlab" | "bitbucket"
export type EngineKind = "codex"; // 未来 #11: | "claude"

export type ReviewEvent =
  | { kind: "messageDelta"; threadId: string; itemId: string; text: string }
  | { kind: "reasoningDelta"; threadId: string; itemId: string; text: string }
  | { kind: "turnCompleted"; threadId: string; status: string }
  | { kind: "error"; threadId: string; message: string }
  // Session-less auto-trigger (#8) notice — no threadId (mirrors
  // `events.rs::ReviewEvent::DispatchError`; locked by a serde golden test).
  | { kind: "dispatchError"; message: string };

// Mirrors `events.rs::PrEvent` (tagged `kind`, camelCase) — the funnel's
// downstream end for the `prs:updated` Tauri event payload.
export type PrEvent =
  | { kind: "updated"; prs: PullRequestView[] }
  | { kind: "error"; message: string };
