// useInboxStore state-machine tests (AB#1065). Drives the store against a mocked `./api`
// module so the assertions stay deterministic — the `inbox:updated` event is simulated by
// invoking the captured `onInboxUpdated` callback directly rather than through the Tauri
// event bus. Mirrors src/pr/usePrStore.test.ts (createPinia/setActivePinia + captured cb).
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { inboxDedupeKey, type Event, type InboxEntry, type InboxEvent } from "../types";

// Captured callback handed to `onInboxUpdated`, so a test can push an `InboxEvent` through
// the same path `subscribe()` wires up.
let inboxCb: ((e: InboxEvent) => void) | null = null;

vi.mock("./api", () => ({
  INBOX_UPDATED_EVENT: "inbox:updated",
  inboxList: vi.fn(() => Promise.resolve([])),
  inboxGetRaw: vi.fn(() => Promise.resolve("{}")),
  inboxReplay: vi.fn(() => Promise.resolve()),
  onInboxUpdated: vi.fn((cb: (e: InboxEvent) => void) => {
    inboxCb = cb;
    // onInboxUpdated returns a Promise<UnlistenFn>.
    return Promise.resolve(() => {});
  }),
}));

import * as api from "./api";
import { useInboxStore } from "./useInboxStore";

// A normalized Event for a given project; only the fields these tests assert on vary.
const event = (projectId: string): Event => ({
  dedupeKey: inboxDedupeKey(`${projectId}-key`),
  source: "github",
  projectId,
  repo: "owner/repo",
  payload: {
    kind: "observation",
    eventType: "pullRequest",
    subject: {
      number: 1,
      title: "an event",
      body: "",
      labels: [],
      url: "https://example.test/1",
    },
  },
  receivedAtEpoch: 0,
});

// An inbox entry wrapping an event for `projectId` (default "p1").
const entry = (id: number, projectId = "p1"): InboxEntry => ({
  id,
  event: event(projectId),
  status: "received",
  processedAtEpoch: null,
  error: null,
});

beforeEach(() => {
  setActivePinia(createPinia());
  inboxCb = null;
  vi.clearAllMocks();
  // Restore default resolved behavior wiped by clearAllMocks.
  vi.mocked(api.inboxList).mockResolvedValue([]);
  vi.mocked(api.inboxGetRaw).mockResolvedValue("{}");
  vi.mocked(api.inboxReplay).mockResolvedValue(undefined);
  vi.mocked(api.onInboxUpdated).mockImplementation((cb) => {
    inboxCb = cb;
    return Promise.resolve(() => {});
  });
});

describe("useInboxStore upsert()", () => {
  it("inserts a new entry keeping the list id-descending (newest first)", () => {
    const store = useInboxStore();
    store.upsert(entry(1));
    store.upsert(entry(3));
    store.upsert(entry(2));

    expect(store.entries.map((e) => e.id)).toEqual([3, 2, 1]);
  });

  it("updates an existing entry in place (by id) without duplicating it", () => {
    const store = useInboxStore();
    store.upsert(entry(1));
    store.upsert(entry(2));

    // A replay flips entry 1's status; the upsert must replace, not append.
    store.upsert({ ...entry(1), status: "processed" });

    expect(store.entries.map((e) => e.id)).toEqual([2, 1]);
    const updated = store.entries.find((e) => e.id === 1);
    expect(updated?.status).toBe("processed");
  });
});

describe("useInboxStore subscribe()", () => {
  it("upserts an `updated` event into state", () => {
    const store = useInboxStore();
    store.subscribe();

    inboxCb?.({ kind: "updated", projectId: "p1", entry: entry(5) });

    expect(store.entries.map((e) => e.id)).toEqual([5]);
  });

  it("ignores an entry for a different project when a projectId filter is set", () => {
    const store = useInboxStore();
    store.projectId = "p1";
    store.subscribe();

    // Same project → applied.
    inboxCb?.({ kind: "updated", projectId: "p1", entry: entry(1, "p1") });
    // Different project → dropped (the entry's nested event.projectId is "p2").
    inboxCb?.({ kind: "updated", projectId: "p2", entry: entry(2, "p2") });

    expect(store.entries.map((e) => e.id)).toEqual([1]);
  });

  it("surfaces a typed worker error without inventing an inbox row", () => {
    const store = useInboxStore();
    store.subscribe();

    inboxCb?.({ kind: "error", operation: "retention", message: "active limit exceeded" });

    expect(store.error).toBe("retention: active limit exceeded");
    expect(store.entries).toEqual([]);
  });
});

describe("useInboxStore fetchRaw()", () => {
  it("does not re-fetch an already-cached id", async () => {
    vi.mocked(api.inboxGetRaw).mockResolvedValueOnce('{"payload":1}');
    const store = useInboxStore();

    await store.fetchRaw(7);
    expect(api.inboxGetRaw).toHaveBeenCalledOnce();
    expect(store.rawCache[7]).toBe('{"payload":1}');

    // Second call for the cached id is a no-op — no further backend round-trip.
    await store.fetchRaw(7);
    expect(api.inboxGetRaw).toHaveBeenCalledOnce();
  });

  it("surfaces a rejected raw fetch on the per-entry rawError, not the shared banner", async () => {
    vi.mocked(api.inboxGetRaw).mockRejectedValueOnce({ message: "no raw" });
    const store = useInboxStore();

    await store.fetchRaw(9);

    expect(store.rawError[9]).toBe("no raw");
    expect(store.rawCache[9]).toBeUndefined();
    expect(store.error).toBeNull(); // shared banner untouched
  });
});

describe("useInboxStore replay()", () => {
  it("guards re-entry while a replay is in flight", async () => {
    const store = useInboxStore();
    // First call leaves replayLoading[1] true until it resolves; assert the guard short-
    // circuits a concurrent second call.
    const first = store.replay(1);
    const second = store.replay(1);
    await Promise.all([first, second]);

    expect(api.inboxReplay).toHaveBeenCalledOnce();
    expect(store.replayLoading[1]).toBe(false);
  });

  it("deterministically refreshes after a SUCCESSFUL replay (pr-review F5)", async () => {
    vi.mocked(api.inboxReplay).mockResolvedValueOnce(undefined);
    // The post-replay snapshot reflects the new status (the row flips received → processed).
    vi.mocked(api.inboxList).mockResolvedValueOnce([{ ...entry(3), status: "processed" }]);
    const store = useInboxStore();

    await store.replay(3);

    expect(api.inboxReplay).toHaveBeenCalledWith(3);
    // Reconciles via refresh even without the re-emitted inbox:updated event.
    expect(api.inboxList).toHaveBeenCalledOnce();
    expect(store.entries.find((e) => e.id === 3)?.status).toBe("processed");
    expect(store.error).toBeNull();
  });

  it("refreshes as a fallback when the replay command rejects", async () => {
    vi.mocked(api.inboxReplay).mockRejectedValueOnce({ message: "replay failed" });
    const store = useInboxStore();

    await store.replay(2);

    expect(store.error).toBe("replay failed");
    // The re-emit won't come, so it pulls a fresh snapshot to reconcile.
    expect(api.inboxList).toHaveBeenCalledOnce();
  });
});

describe("useInboxStore init()", () => {
  it("subscribes BEFORE reading the snapshot (race guard)", async () => {
    const order: string[] = [];
    vi.mocked(api.onInboxUpdated).mockImplementation((cb) => {
      order.push("subscribe");
      inboxCb = cb;
      return Promise.resolve(() => {});
    });
    vi.mocked(api.inboxList).mockImplementation(() => {
      order.push("snapshot");
      return Promise.resolve([]);
    });
    const store = useInboxStore();

    const unlisten = await store.init();

    expect(order).toEqual(["subscribe", "snapshot"]);
    expect(typeof (await unlisten)).toBe("function");
  });
});
