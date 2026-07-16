import { beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import type { OutboxEntry } from "../types";
import type { MessagingEventEntry, MessagingIntegrationOption } from "../types.generated";

vi.mock("./api", () => ({
  messagingEventsList: vi.fn(() => Promise.resolve([])),
  messagingEventRaw: vi.fn(() => Promise.resolve("{}")),
  messagingEventReplay: vi.fn(() => Promise.resolve()),
  messagingIntegrationsList: vi.fn(() => Promise.resolve([])),
  messagingConnectionStatusesList: vi.fn(() => Promise.resolve([])),
  messagingSend: vi.fn(() => Promise.resolve({ outboxId: 9 })),
  messagingSendsList: vi.fn(() => Promise.resolve([])),
}));

import * as api from "./api";
import { useMessagingStore } from "./useMessagingStore";

const entry = (id: number): MessagingEventEntry => ({
  id,
  event: {
    provider: "feishu",
    integrationId: "feishu-main",
    eventId: `event-${id}`,
    conversationId: "oc_123",
    threadId: "om_123",
    senderId: "ou_123",
    text: "/help",
    mentionedBot: true,
    rawPayload: "",
    receivedAtEpoch: 1,
  },
  status: "received",
  processedAtEpoch: null,
  error: null,
  reply: null,
});

const sendEntry = (id: number): OutboxEntry => ({
  id,
  projectId: "feishu-main",
  kind: "messagingSend",
  summary: "Messaging send feishu -> oc_123",
  status: "pending",
  attemptCount: 0,
  nextAttemptAt: 1,
  lastError: null,
  createdAt: 1,
  updatedAt: 1,
});

const integrationOption = (): MessagingIntegrationOption => ({
  id: "wx-main",
  name: "企业微信",
  kind: "weChatWork",
  allowedConversationIds: ["room-1"],
});

beforeEach(() => {
  setActivePinia(createPinia());
  vi.clearAllMocks();
  vi.mocked(api.messagingEventsList).mockResolvedValue([]);
  vi.mocked(api.messagingEventRaw).mockResolvedValue("{}");
  vi.mocked(api.messagingEventReplay).mockResolvedValue(undefined);
  vi.mocked(api.messagingIntegrationsList).mockResolvedValue([]);
  vi.mocked(api.messagingConnectionStatusesList).mockResolvedValue([]);
  vi.mocked(api.messagingSend).mockResolvedValue({ outboxId: 9 });
  vi.mocked(api.messagingSendsList).mockResolvedValue([]);
});

describe("useMessagingStore refreshIntegrations()", () => {
  it("loads secret-free active send integration options", async () => {
    vi.mocked(api.messagingIntegrationsList).mockResolvedValueOnce([integrationOption()]);
    const store = useMessagingStore();

    await store.refreshIntegrations();

    expect(api.messagingIntegrationsList).toHaveBeenCalledOnce();
    expect(store.integrations).toEqual([integrationOption()]);
    expect(store.integrationsLoading).toBe(false);
    expect(store.error).toBeNull();
  });
});

describe("useMessagingStore refreshConnectionStatuses()", () => {
  it("loads Feishu long-connection health independently from message events", async () => {
    const statuses = [{
      provider: "feishu" as const,
      integrationId: "feishu-main",
      status: "reconnecting" as const,
      lastConnectedAtEpoch: 1_700_000_000,
      lastEventAtEpoch: 1_700_000_100,
      lastError: "socket closed",
      reconnectCount: 3,
    }];
    vi.mocked(api.messagingConnectionStatusesList).mockResolvedValueOnce(statuses);
    const store = useMessagingStore();

    await store.refreshConnectionStatuses();

    expect(api.messagingConnectionStatusesList).toHaveBeenCalledOnce();
    expect(store.connectionStatuses).toEqual(statuses);
    expect(store.connectionStatusesStale).toBe(false);
    expect(store.connectionStatusesLastUpdatedAt).not.toBeNull();
    expect(store.connectionStatusesLoading).toBe(false);
    expect(store.connectionStatusesError).toBeNull();
  });

  it("does not replace the last known health snapshot on refresh failure", async () => {
    vi.mocked(api.messagingConnectionStatusesList).mockRejectedValueOnce(new Error("offline"));
    const store = useMessagingStore();
    store.connectionStatusesLastUpdatedAt = 123;
    store.connectionStatuses = [{
      provider: "feishu" as const,
      integrationId: "feishu-main",
      status: "connected",
      lastConnectedAtEpoch: 1,
      lastEventAtEpoch: null,
      lastError: null,
      reconnectCount: 0,
    }];

    await store.refreshConnectionStatuses();

    expect(store.connectionStatuses[0]?.status).toBe("connected");
    expect(store.connectionStatusesError).toBe("offline");
    expect(store.connectionStatusesStale).toBe(true);
    expect(store.connectionStatusesLastUpdatedAt).toBe(123);
    expect(store.connectionStatusesLoading).toBe(false);
  });
});

describe("useMessagingStore refresh()", () => {
  it("loads entries and clears the panel error", async () => {
    vi.mocked(api.messagingEventsList).mockResolvedValueOnce([entry(1)]);
    const store = useMessagingStore();
    store.error = "stale";

    await store.refresh();

    expect(api.messagingEventsList).toHaveBeenCalledWith();
    expect(store.entries.map((item) => item.id)).toEqual([1]);
    expect(store.error).toBeNull();
    expect(store.loading).toBe(false);
  });

  it("surfaces list failures without clearing existing entries", async () => {
    vi.mocked(api.messagingEventsList).mockRejectedValueOnce({ message: "boom" });
    const store = useMessagingStore();
    store.entries = [entry(1)];

    await store.refresh();

    expect(store.entries.map((item) => item.id)).toEqual([1]);
    expect(store.error).toBe("boom");
    expect(store.loading).toBe(false);
  });
});

describe("useMessagingStore send logs", () => {
  it("loads messaging send/reply outbox rows", async () => {
    vi.mocked(api.messagingSendsList).mockResolvedValueOnce([sendEntry(9)]);
    const store = useMessagingStore();

    await store.refreshSends();

    expect(api.messagingSendsList).toHaveBeenCalledWith();
    expect(store.sends.map((item) => item.id)).toEqual([9]);
    expect(store.sendsLoading).toBe(false);
  });

  it("enqueues a send and refreshes send logs", async () => {
    vi.mocked(api.messagingSendsList).mockResolvedValueOnce([sendEntry(9)]);
    const store = useMessagingStore();

    await store.send({
      integrationId: "feishu-main",
      conversationId: "oc_123",
      content: { kind: "text", text: "hello" },
      requestId: "req-1",
    });

    expect(api.messagingSend).toHaveBeenCalledWith({
      integrationId: "feishu-main",
      conversationId: "oc_123",
      content: { kind: "text", text: "hello" },
      requestId: "req-1",
    });
    expect(store.sends.map((item) => item.id)).toEqual([9]);
    expect(store.sendError).toBeNull();
    expect(store.sendLoading).toBe(false);
  });

  it("keeps send failures scoped to the send form", async () => {
    vi.mocked(api.messagingSend).mockRejectedValueOnce({ message: "send failed" });
    const store = useMessagingStore();

    await store.send({
      integrationId: "feishu-main",
      conversationId: "oc_123",
      content: { kind: "text", text: "hello" },
      requestId: "req-1",
    });

    expect(store.sendError).toBe("send failed");
    expect(store.error).toBeNull();
    expect(api.messagingSendsList).not.toHaveBeenCalled();
  });
});

describe("useMessagingStore fetchRaw()", () => {
  it("caches raw summaries and skips duplicate fetches", async () => {
    vi.mocked(api.messagingEventRaw).mockResolvedValueOnce("{redacted}");
    const store = useMessagingStore();

    await store.fetchRaw(7);
    await store.fetchRaw(7);

    expect(api.messagingEventRaw).toHaveBeenCalledOnce();
    expect(store.rawCache[7]).toBe("{redacted}");
    expect(store.rawError[7]).toBeNull();
  });

  it("keeps raw fetch errors scoped to the row", async () => {
    vi.mocked(api.messagingEventRaw).mockRejectedValueOnce({ message: "missing" });
    const store = useMessagingStore();

    await store.fetchRaw(7);

    expect(store.rawError[7]).toBe("missing");
    expect(store.rawLoading[7]).toBe(false);
    expect(store.error).toBeNull();
  });
});

describe("useMessagingStore replay()", () => {
  it("replays the entry then refreshes the audit list", async () => {
    vi.mocked(api.messagingEventsList).mockResolvedValueOnce([entry(2)]);
    const store = useMessagingStore();

    await store.replay(2);

    expect(api.messagingEventReplay).toHaveBeenCalledWith(2);
    expect(api.messagingEventsList).toHaveBeenCalledOnce();
    expect(store.entries.map((item) => item.id)).toEqual([2]);
    expect(store.replayLoading[2]).toBe(false);
  });

  it("surfaces replay failures on the panel banner", async () => {
    vi.mocked(api.messagingEventReplay).mockRejectedValueOnce({ message: "replay failed" });
    const store = useMessagingStore();

    await store.replay(2);

    expect(store.error).toBe("replay failed");
    expect(store.replayLoading[2]).toBe(false);
    expect(api.messagingEventsList).not.toHaveBeenCalled();
  });
});
