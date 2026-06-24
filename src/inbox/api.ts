// Inbox slice → backend adapter. Wraps the inbox commands and the `inbox:updated`
// event subscription. Mirrors src/pr/api.ts: the ONLY `@tauri-apps/api` access goes
// through the shared `../api` root, and the wire types come from the `../types` root —
// the inbox slice never imports a sibling slice (slice-boundary.test.ts).
import { invoke, listen } from "../api";
import type { InboxEntry, InboxEvent } from "../types";

// Mirrors the backend's `inbox:updated` Tauri event name (this TS side is the open
// downstream end of the event-name funnel — keep in lockstep with the emitter).
export const INBOX_UPDATED_EVENT = "inbox:updated" as const;

// Read the inbox entries, newest first (AB#1065). An optional `projectId` scopes the
// list to one monitored project; omitting it returns every project's entries. The JS
// `projectId` key maps to the Rust `project_id` snake_case arg (same convention as the
// pr slice's pollNow/getPrs). `undefined` is dropped from the serialized args, so the
// backend sees an absent argument and returns the unscoped list.
export function inboxList(projectId?: string): Promise<InboxEntry[]> {
  return invoke<InboxEntry[]>("inbox_list", { projectId });
}

// Fetch one entry's raw inbound payload (AB#1065) — the original webhook body the
// normalized `Event` was derived from, for the "View raw" disclosure.
export function inboxGetRaw(id: number): Promise<string> {
  return invoke<string>("inbox_get_raw", { id });
}

// Re-run processing for one entry (AB#1065). The backend re-emits `inbox:updated` for
// the entry after replay, so the store refreshes via the existing listener — no return
// value to apply here.
export function inboxReplay(id: number): Promise<void> {
  return invoke<void>("inbox_replay", { id });
}

// Subscribe to backend-pushed inbox updates. Returns the `Promise<UnlistenFn>` so the
// caller can await it for cleanup on unmount (mirrors onPrsUpdated).
export function onInboxUpdated(cb: (e: InboxEvent) => void) {
  return listen<InboxEvent>(INBOX_UPDATED_EVENT, (event) => cb(event.payload));
}
