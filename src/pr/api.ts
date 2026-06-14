// PR slice → backend adapter. Wraps the polling-scheduler commands and the
// `prs:updated` event subscription.
import { invoke, listen } from "../api";
import type { PrEvent } from "../types";
import type { GhStatus } from "./types";

// Mirrors src-tauri/src/events.rs::PRS_UPDATED_EVENT (this TS side is the
// open downstream end of the event-name funnel — keep in lockstep).
const PRS_UPDATED_EVENT = "prs:updated" as const;

export function pollNow(): Promise<void> {
  return invoke<void>("poll_now");
}

export function startPolling(): Promise<void> {
  return invoke<void>("start_polling");
}

export function stopPolling(): Promise<void> {
  return invoke<void>("stop_polling");
}

export function reschedule(): Promise<void> {
  return invoke<void>("reschedule");
}

// Subscribe to backend-pushed PR updates. Returns the `Promise<UnlistenFn>` so
// the caller can await it for cleanup on unmount.
export function onPrsUpdated(cb: (e: PrEvent) => void) {
  return listen<PrEvent>(PRS_UPDATED_EVENT, (event) => cb(event.payload));
}

export function ghStatus(): Promise<GhStatus> {
  return invoke<GhStatus>("gh_status");
}
