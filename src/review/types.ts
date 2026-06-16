// Review 切片私有 wire 型，镜像 src-tauri 的 CodexStatus（不进 src/types.ts —— 对标 src/pr/types.ts 的 GhStatus）。
export interface CodexStatus {
  available: boolean;
  // 用户意图运行态：false=用户已显式停止（StatusBar 显示「启动」按钮 + idle 点），true=意图运行（available 反映实际连通）。
  desiredRunning: boolean;
  message: string;
}

// Mirrors `session.rs::SessionStatus` (serde camelCase). Slice-private.
export type SessionStatus =
  | "starting"
  | "running"
  | "interrupting"
  | "done"
  | "failed";

// Mirrors `session.rs::SessionInfo` — one review session (list_review_sessions).
export interface ReviewSession {
  threadId: string;
  turnId: string;
  prNumber: number;
  kind: string;
  status: SessionStatus;
}

// A view-only aggregate of streamed deltas, keyed by codex `itemId`. Message and
// reasoning items render differently (reasoning is collapsed); one `itemId` only
// ever carries one kind.
export interface StreamItem {
  itemId: string;
  kind: "message" | "reasoning";
  text: string;
}
