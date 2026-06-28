import { beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import type { MessagingEventEntry } from "../types.generated";

vi.mock("./api", () => ({
  messagingEventsList: vi.fn(() => Promise.resolve([])),
  messagingEventRaw: vi.fn(() => Promise.resolve("{}")),
  messagingEventReplay: vi.fn(() => Promise.resolve()),
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

beforeEach(() => {
  setActivePinia(createPinia());
  vi.clearAllMocks();
  vi.mocked(api.messagingEventsList).mockResolvedValue([]);
  vi.mocked(api.messagingEventRaw).mockResolvedValue("{}");
  vi.mocked(api.messagingEventReplay).mockResolvedValue(undefined);
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
