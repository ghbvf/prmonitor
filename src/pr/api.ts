// PR slice → backend adapter. Wraps the polling-scheduler commands and the
// `prs:updated` event subscription.
import { invoke, listen } from "../api";
import type { PrEvent, TrackedPrView } from "../types";
import type { GhStatus, WebhookStatus } from "./types";

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

// Read the scheduler's latest PR snapshot. Used to baseline the list at startup
// so the view is populated regardless of whether the first `prs:updated` event
// raced ahead of the listener registration (#27 F3). The backend now returns the
// retained, tracking-aware list (#38).
export function getPrs(): Promise<TrackedPrView[]> {
  return invoke<TrackedPrView[]>("get_prs");
}

// Archive / unarchive a retained PR (#38). The backend re-emits `prs:updated`
// after the flag flips, so the store refreshes via the existing listener.
export function setPrArchived(
  number: number,
  archived: boolean,
): Promise<void> {
  return invoke<void>("set_pr_archived", { number, archived });
}

export function ghStatus(): Promise<GhStatus> {
  return invoke<GhStatus>("gh_status");
}

// Webhook receiver + Cloudflare tunnel controls (#9). Each returns the current
// WebhookStatus snapshot the WebhookPanel renders; none take arguments.
export function startWebhook(): Promise<WebhookStatus> {
  return invoke<WebhookStatus>("start_webhook");
}

export function stopWebhook(): Promise<WebhookStatus> {
  return invoke<WebhookStatus>("stop_webhook");
}

export function webhookStatus(): Promise<WebhookStatus> {
  return invoke<WebhookStatus>("webhook_status");
}
