// Review slice → backend adapter. Wraps the codex availability + review session
// commands, and the streamed `review:event` Tauri event.
import { invoke, listen } from "../api";
import type { ReviewEvent } from "../types";
import type { CodexStatus, ReviewSession, StreamItem } from "./types";

// Mirrors `src-tauri/src/events.rs::REVIEW_EVENT` (pinned by a Rust test).
const REVIEW_EVENT = "review:event" as const;

export function getCodexStatus(): Promise<CodexStatus> {
  return invoke<CodexStatus>("get_codex_status");
}

// Explicitly start the resident codex app-server (clears the user-stop flag).
export function startCodex(): Promise<CodexStatus> {
  return invoke<CodexStatus>("start_codex");
}

// Explicitly stop the resident codex app-server (passive probes won't revive it).
export function stopCodex(): Promise<CodexStatus> {
  return invoke<CodexStatus>("stop_codex");
}

// Starts a review for a PR in the given project (#35); resolves with the session
// id (codex threadId). Output streams out-of-band via `onReviewEvent`. `kind` is
// "review" or "check".
export function startReview(
  projectId: string,
  prNumber: number,
  kind: string,
): Promise<string> {
  return invoke<string>("start_review", { projectId, prNumber, kind });
}

export function stopReview(sessionId: string): Promise<void> {
  return invoke<void>("stop_review", { sessionId });
}

export function listReviewSessions(): Promise<ReviewSession[]> {
  return invoke<ReviewSession[]>("list_review_sessions");
}

// A PR's persisted sessions, newest first, from the DURABLE store (#70) — survives an
// app restart (unlike `listReviewSessions`, the in-memory snapshot), so the session
// panel can list a PR's prior sessions and re-open their history.
export function getPrSessions(
  projectId: string,
  prNumber: number,
): Promise<ReviewSession[]> {
  return invoke<ReviewSession[]>("get_pr_sessions", { projectId, prNumber });
}

// A session's persisted history items in stream order (#70) — message/reasoning blocks
// produced before the session was opened. Same shape as the live `StreamItem`, so the
// panel can render stored + live content uniformly.
export function getSessionHistory(threadId: string): Promise<StreamItem[]> {
  return invoke<StreamItem[]>("get_session_history", { threadId });
}

// Subscribe to streamed review events. Returns a Promise<UnlistenFn> for cleanup.
export function onReviewEvent(cb: (e: ReviewEvent) => void) {
  return listen<ReviewEvent>(REVIEW_EVENT, (event) => cb(event.payload));
}
