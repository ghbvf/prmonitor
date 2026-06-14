// Shared cross-slice contracts mirroring `src-tauri/src/model.rs` (the contract
// boundary). Slices import from here; they do not import each other's internals.

export interface PullRequestView {
  number: number;
  title: string;
  labels: string[];
  url: string;
}

export type ReviewEvent =
  | { kind: "messageDelta"; threadId: string; itemId: string; text: string }
  | { kind: "reasoningDelta"; threadId: string; itemId: string; text: string }
  | { kind: "turnCompleted"; threadId: string; status: string }
  | { kind: "error"; threadId: string; message: string };
