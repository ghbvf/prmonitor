// PR slice → backend adapter. Wraps the polling-scheduler commands and the
// `prs:updated` event subscription.
import { invoke, listen } from "../api";
import type { PrEvent, TrackedPrView } from "../types";
import type {
  GhStatus,
  PollStatus,
  WebhookDelivery,
  WebhookStatus,
} from "./types";

// Mirrors src-tauri/src/events.rs::PRS_UPDATED_EVENT (this TS side is the
// open downstream end of the event-name funnel — keep in lockstep).
const PRS_UPDATED_EVENT = "prs:updated" as const;

// Wake ONE project's poll loop (#35). The JS `projectId` key maps to the Rust
// `project_id` snake_case arg.
export function pollNow(projectId: string): Promise<void> {
  return invoke<void>("poll_now", { projectId });
}

// Global scheduler controls (#35): reconcile-all / stop-all — they take NO
// projectId; the backend fans the loop out across every enabled project.
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

// Read one project's latest PR snapshot (#35). Used to baseline the list at
// startup / on project switch so the view is populated regardless of whether the
// first `prs:updated` event raced ahead of the listener registration (#27 F3).
// The backend returns the retained, tracking-aware list (#38).
export function getPrs(projectId: string): Promise<TrackedPrView[]> {
  return invoke<TrackedPrView[]>("get_prs", { projectId });
}

// Archive / unarchive a retained PR within a project (#38, #35). The backend
// re-emits `prs:updated` for that project after the flag flips, so the store
// refreshes via the existing listener.
export function setPrArchived(
  projectId: string,
  number: number,
  archived: boolean,
): Promise<void> {
  return invoke<void>("set_pr_archived", { projectId, number, archived });
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

// Webhook delivery diagnostics ring (#62): the most recent received deliveries the
// receiver classified, oldest→newest (the WebhookPanel reverses for display). No
// arguments — the ring is process-global.
export function webhookDeliveries(): Promise<WebhookDelivery[]> {
  return invoke<WebhookDelivery[]>("webhook_deliveries");
}

// One project's poll-loop status snapshot (#62). The JS `projectId` key maps to the
// Rust `project_id` snake_case arg (same convention as pollNow/getPrs above).
export function pollStatus(projectId: string): Promise<PollStatus> {
  return invoke<PollStatus>("poll_status", { projectId });
}
