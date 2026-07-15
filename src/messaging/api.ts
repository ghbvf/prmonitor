import { getTransport } from "../transport";
import type { OutboxEntry } from "../types";
import type {
  FeishuConnectionStatus,
  MessagingEventEntry,
  MessagingIntegrationOption,
  SendMessagingRequest,
  SendMessagingResponse,
} from "../types.generated";

export function messagingEventsList(integrationId?: string): Promise<MessagingEventEntry[]> {
  return getTransport().request<MessagingEventEntry[]>("messaging_events_list", { integrationId });
}

export function messagingEventRaw(id: number): Promise<string> {
  return getTransport().request<string>("messaging_event_raw", { id });
}

export function messagingEventReplay(id: number): Promise<void> {
  return getTransport().request<void>("messaging_event_replay", { id });
}

export function messagingSend(request: SendMessagingRequest): Promise<SendMessagingResponse> {
  return getTransport().request<SendMessagingResponse>("messaging_send", { request });
}

export function messagingSendsList(integrationId?: string): Promise<OutboxEntry[]> {
  return getTransport().request<OutboxEntry[]>("messaging_sends_list", { integrationId });
}

export function messagingIntegrationsList(): Promise<MessagingIntegrationOption[]> {
  return getTransport().request<MessagingIntegrationOption[]>("messaging_integrations_list");
}

export function messagingConnectionStatusesList(): Promise<FeishuConnectionStatus[]> {
  return getTransport().request<FeishuConnectionStatus[]>("messaging_connection_statuses_list");
}
