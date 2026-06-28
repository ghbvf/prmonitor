import { getTransport } from "../transport";
import type { MessagingEventEntry } from "../types.generated";

export function messagingEventsList(integrationId?: string): Promise<MessagingEventEntry[]> {
  return getTransport().request<MessagingEventEntry[]>("messaging_events_list", { integrationId });
}

export function messagingEventRaw(id: number): Promise<string> {
  return getTransport().request<string>("messaging_event_raw", { id });
}

export function messagingEventReplay(id: number): Promise<void> {
  return getTransport().request<void>("messaging_event_replay", { id });
}