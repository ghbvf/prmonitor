// Review slice → backend adapter. Wraps the codex availability + review session
// commands, and the streamed `review:event` Tauri event.
import { invoke, listen } from "../api";
import type { ReviewEvent } from "../types";
import type { CodexStatus, ReviewSession } from "./types";

// Mirrors `src-tauri/src/events.rs::REVIEW_EVENT` (pinned by a Rust test).
const REVIEW_EVENT = "review:event" as const;

export function getCodexStatus(): Promise<CodexStatus> {
  return invoke<CodexStatus>("get_codex_status");
}

// Starts a review; resolves with the session id (codex threadId). Output streams
// out-of-band via `onReviewEvent`. `kind` is "review" or "check".
export function startReview(prNumber: number, kind: string): Promise<string> {
  return invoke<string>("start_review", { prNumber, kind });
}

export function stopReview(sessionId: string): Promise<void> {
  return invoke<void>("stop_review", { sessionId });
}

export function listReviewSessions(): Promise<ReviewSession[]> {
  return invoke<ReviewSession[]>("list_review_sessions");
}

// Subscribe to streamed review events. Returns a Promise<UnlistenFn> for cleanup.
export function onReviewEvent(cb: (e: ReviewEvent) => void) {
  return listen<ReviewEvent>(REVIEW_EVENT, (event) => cb(event.payload));
}
