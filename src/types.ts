// Shared cross-slice contracts mirroring `src-tauri/src/model.rs` (the contract
// boundary). Slices import from here; they do not import each other's internals.

export interface PullRequestView {
  number: number;
  title: string;
  labels: string[];
  url: string;
}

// Discriminator unions mirroring the `SourceKind` / `EngineKind` Rust enums.
// Single-arm today; widening tracked by #11.
export type SourceKind = "github"; // 未来 #11: | "gitlab" | "bitbucket"
export type EngineKind = "codex"; // 未来 #11: | "claude"

export type ReviewEvent =
  | { kind: "messageDelta"; threadId: string; itemId: string; text: string }
  | { kind: "reasoningDelta"; threadId: string; itemId: string; text: string }
  | { kind: "turnCompleted"; threadId: string; status: string }
  | { kind: "error"; threadId: string; message: string };
