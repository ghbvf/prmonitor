// Review slice → backend adapter. Wraps the codex availability + review session
// commands, and the streamed `review:event` Tauri event.
import { getTransport } from "../transport";
import type { ReviewEvent } from "../types";
import type { ClaudeStatus, CodexStatus, ReviewSession, StreamItem } from "./types";

// Mirrors `src-tauri/src/events.rs::REVIEW_EVENT` (pinned by a Rust test).
const REVIEW_EVENT = "review:event" as const;

export function getCodexStatus(): Promise<CodexStatus> {
  return getTransport().request<CodexStatus>("get_codex_status");
}

// claude availability (one-shot `claude --version`); no start/stop — claude has no
// resident server (unlike codex).
export function getClaudeStatus(): Promise<ClaudeStatus> {
  return getTransport().request<ClaudeStatus>("get_claude_status");
}

// Explicitly start the resident codex app-server (clears the user-stop flag).
export function startCodex(): Promise<CodexStatus> {
  return getTransport().request<CodexStatus>("start_codex");
}

// Explicitly stop the resident codex app-server (passive probes won't revive it).
export function stopCodex(): Promise<CodexStatus> {
  return getTransport().request<CodexStatus>("stop_codex");
}

// Starts a review for a PR in the given project (#35); resolves with the session
// id (codex threadId). Output streams out-of-band via `onReviewEvent`. `kind` is
// "review" or "check".
export function startReview(
  projectId: string,
  prNumber: number,
  kind: string,
): Promise<string> {
  return getTransport().request<string>("start_review", { projectId, prNumber, kind });
}

export function stopReview(sessionId: string): Promise<void> {
  return getTransport().request<void>("stop_review", { sessionId });
}

// Send a follow-up chat message into an existing review thread (#chat). The AI
// reply streams back out-of-band on the SAME session via `onReviewEvent` (so it
// folds into `items` like the initial review). `userItemId` is the optimistic user
// bubble's id — it MUST equal the id used for the local bubble so reopen-history
// dedup (focus() merges history + live by itemId) doesn't double-render it.
export function sendReviewMessage(
  projectId: string,
  threadId: string,
  message: string,
  userItemId: string,
): Promise<void> {
  return getTransport().request<void>("send_review_message", {
    projectId,
    threadId,
    message,
    userItemId,
  });
}

export function listReviewSessions(): Promise<ReviewSession[]> {
  return getTransport().request<ReviewSession[]>("list_review_sessions");
}

// A PR's persisted sessions, newest first, from the DURABLE store (#70) — survives an
// app restart (unlike `listReviewSessions`, the in-memory snapshot), so the session
// panel can list a PR's prior sessions and re-open their history.
export function getPrSessions(
  projectId: string,
  prNumber: number,
): Promise<ReviewSession[]> {
  return getTransport().request<ReviewSession[]>("get_pr_sessions", { projectId, prNumber });
}

// A session's persisted history items in stream order (#70) — message/reasoning blocks
// produced before the session was opened. Same shape as the live `StreamItem`, so the
// panel can render stored + live content uniformly.
export function getSessionHistory(
  projectId: string,
  prNumber: number,
  threadId: string,
): Promise<StreamItem[]> {
  return getTransport().request<StreamItem[]>("get_session_history", {
    projectId,
    prNumber,
    threadId,
  });
}

// Subscribe to streamed review events. Returns a Promise<UnlistenFn> for cleanup.
export function onReviewEvent(cb: (e: ReviewEvent) => void) {
  return getTransport().subscribe<ReviewEvent>(REVIEW_EVENT, cb);
}
