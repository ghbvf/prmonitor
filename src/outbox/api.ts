// Outbox slice → backend adapter. Wraps the outbox commands and the `outbox:updated`
// event subscription. Mirrors src/inbox/api.ts: the ONLY `@tauri-apps/api` access goes
// through the shared `../api` root, and the wire types come from the `../types` root —
// the outbox slice never imports a sibling slice (slice-boundary.test.ts).
import { invoke, listen } from "../api";
import type { OutboxEntry, OutboxEvent } from "../types";

// Mirrors the backend's `outbox:updated` Tauri event name (this TS side is the open
// downstream end of the event-name funnel — keep in lockstep with the emitter).
export const OUTBOX_UPDATED_EVENT = "outbox:updated" as const;

// Read the outbox entries, newest first (AB#1066). An optional `projectId` scopes the
// list to one monitored project; omitting it returns every project's entries. The JS
// `projectId` key maps to the Rust `project_id` snake_case arg (same convention as the
// inbox slice's inboxList). `undefined` is dropped from the serialized args, so the
// backend sees an absent argument and returns the unscoped list.
export function outboxList(projectId?: string): Promise<OutboxEntry[]> {
  return invoke<OutboxEntry[]>("outbox_list", { projectId });
}

// Fetch one entry's raw action payload (AB#1066) — the serialized action the entry was
// enqueued from, for the "查看原始" disclosure. The payload is NOT embedded in OutboxEntry
// (unlike the inbox's nested event), so this is the only way to surface it.
export function outboxGetRaw(id: number): Promise<string> {
  return invoke<string>("outbox_get_raw", { id });
}

// Re-run a dead-lettered entry (AB#1066). The backend re-emits `outbox:updated` for the
// entry after re-queuing it, so the store refreshes via the existing listener — no return
// value to apply here.
export function outboxRetry(id: number): Promise<void> {
  return invoke<void>("outbox_retry", { id });
}

// Subscribe to backend-pushed outbox updates. Returns the `Promise<UnlistenFn>` so the
// caller can await it for cleanup on unmount (mirrors onInboxUpdated).
export function onOutboxUpdated(cb: (e: OutboxEvent) => void) {
  return listen<OutboxEvent>(OUTBOX_UPDATED_EVENT, (event) => cb(event.payload));
}
