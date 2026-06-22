// Review 切片私有 wire 型，镜像 src-tauri 的 CodexStatus（不进 src/types.ts —— 对标 src/pr/types.ts 的 GhStatus）。
export interface CodexStatus {
  available: boolean;
  // 用户意图运行态：false=用户已显式停止（StatusBar 显示「启动」按钮 + idle 点），true=意图运行（available 反映实际连通）。
  desiredRunning: boolean;
  message: string;
}

// Review 切片私有 wire 型，镜像 src-tauri 的 ClaudeStatus（不进 src/types.ts —— 对标 CodexStatus）。
// claude 是一次性 `claude -p`，无常驻进程，故无 desiredRunning / 无启动停止：只报「已安装可用」。
export interface ClaudeStatus {
  available: boolean;
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
// `projectId` (#35) attributes each session to its monitored project so the
// ReviewSessions list can filter to the active project.
export interface ReviewSession {
  projectId: string;
  threadId: string;
  turnId: string;
  prNumber: number;
  kind: string;
  status: SessionStatus;
  // Wall-clock epoch seconds the session was created (#70, review F10): the newest-first
  // sort key for the session list. Mirrors `SessionInfo.created_at_epoch` (Rust) — a UUID
  // `threadId` has no time, so the list is ordered by this instead.
  createdAtEpoch: number;
  // The resolved pr-review comment URL (AB#1042), present once a session reached a
  // `completed` terminal and the source kind resolved one (GitHub: exact comment URL;
  // Azure: PR URL; else absent). Mirrors `SessionInfo.comment_url` (Rust, omitted when None).
  commentUrl?: string;
}

// A view-only aggregate of streamed deltas, keyed by codex `itemId`. Message and
// reasoning items render differently (reasoning is collapsed); one `itemId` only
// ever carries one kind.
export interface StreamItem {
  itemId: string;
  kind: "message" | "reasoning";
  text: string;
}
