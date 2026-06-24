// useOutboxStore state-machine tests (AB#1066). Drives the store against a mocked `./api`
// module so the assertions stay deterministic — the `outbox:updated` event is simulated by
// invoking the captured `onOutboxUpdated` callback directly rather than through the Tauri
// event bus. Mirrors src/inbox/useInboxStore.test.ts (createPinia/setActivePinia + captured cb).
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import type { OutboxEntry, OutboxEvent } from "../types";

// Captured callback handed to `onOutboxUpdated`, so a test can push an `OutboxEvent` through
// the same path `subscribe()` wires up.
let outboxCb: ((e: OutboxEvent) => void) | null = null;

vi.mock("./api", () => ({
  OUTBOX_UPDATED_EVENT: "outbox:updated",
  outboxList: vi.fn(() => Promise.resolve([])),
  outboxGetRaw: vi.fn(() => Promise.resolve("{}")),
  outboxRetry: vi.fn(() => Promise.resolve()),
  onOutboxUpdated: vi.fn((cb: (e: OutboxEvent) => void) => {
    outboxCb = cb;
    // onOutboxUpdated returns a Promise<UnlistenFn>.
    return Promise.resolve(() => {});
  }),
}));

import * as api from "./api";
import { useOutboxStore } from "./useOutboxStore";

// An outbox entry for `projectId` (default "p1"); only the fields these tests assert on vary.
// NOTE: projectId is at the TOP level (unlike InboxEntry, whose projectId is nested).
const entry = (id: number, projectId = "p1"): OutboxEntry => ({
  id,
  projectId,
  kind: "notification",
  summary: "an action",
  status: "pending",
  attemptCount: 0,
  nextAttemptAt: 0,
  lastError: null,
  createdAt: 0,
  updatedAt: 0,
});

beforeEach(() => {
  setActivePinia(createPinia());
  outboxCb = null;
  vi.clearAllMocks();
  // Restore default resolved behavior wiped by clearAllMocks.
  vi.mocked(api.outboxList).mockResolvedValue([]);
  vi.mocked(api.outboxGetRaw).mockResolvedValue("{}");
  vi.mocked(api.outboxRetry).mockResolvedValue(undefined);
  vi.mocked(api.onOutboxUpdated).mockImplementation((cb) => {
    outboxCb = cb;
    return Promise.resolve(() => {});
  });
});

describe("useOutboxStore upsert()", () => {
  it("inserts a new entry keeping the list id-descending (newest first)", () => {
    const store = useOutboxStore();
    store.upsert(entry(1));
    store.upsert(entry(3));
    store.upsert(entry(2));

    expect(store.entries.map((e) => e.id)).toEqual([3, 2, 1]);
  });

  it("updates an existing entry in place (by id) without duplicating it", () => {
    const store = useOutboxStore();
    store.upsert(entry(1));
    store.upsert(entry(2));

    // A retry flips entry 1's status; the upsert must replace, not append.
    store.upsert({ ...entry(1), status: "done" });

    expect(store.entries.map((e) => e.id)).toEqual([2, 1]);
    const updated = store.entries.find((e) => e.id === 1);
    expect(updated?.status).toBe("done");
  });
});

describe("useOutboxStore subscribe()", () => {
  it("upserts an `updated` event into state", () => {
    const store = useOutboxStore();
    store.subscribe();

    outboxCb?.({ kind: "updated", projectId: "p1", entry: entry(5) });

    expect(store.entries.map((e) => e.id)).toEqual([5]);
  });

  it("ignores an entry for a different project when a projectId filter is set", () => {
    const store = useOutboxStore();
    store.projectId = "p1";
    store.subscribe();

    // Same project → applied.
    outboxCb?.({ kind: "updated", projectId: "p1", entry: entry(1, "p1") });
    // Different project → dropped (the entry's TOP-level projectId is "p2").
    outboxCb?.({ kind: "updated", projectId: "p2", entry: entry(2, "p2") });

    expect(store.entries.map((e) => e.id)).toEqual([1]);
  });

  it("routes an `error` event to the cycleError banner, not entries (AB#1182)", () => {
    const store = useOutboxStore();
    // A project filter must NOT gate a cycle error — it's global, not row-scoped.
    store.projectId = "p1";
    store.subscribe();

    outboxCb?.({ kind: "error", operation: "claim", message: "database is locked" });

    expect(store.cycleError).toBe("claim: database is locked");
    // The row list and the command-path banner are untouched (separate channels).
    expect(store.entries).toEqual([]);
    expect(store.error).toBeNull();
  });
});

describe("useOutboxStore fetchRaw()", () => {
  it("does not re-fetch an already-cached id", async () => {
    vi.mocked(api.outboxGetRaw).mockResolvedValueOnce('{"payload":1}');
    const store = useOutboxStore();

    await store.fetchRaw(7);
    expect(api.outboxGetRaw).toHaveBeenCalledOnce();
    expect(store.rawCache[7]).toBe('{"payload":1}');

    // Second call for the cached id is a no-op — no further backend round-trip.
    await store.fetchRaw(7);
    expect(api.outboxGetRaw).toHaveBeenCalledOnce();
  });

  it("surfaces a rejected raw fetch on the per-entry rawError, not the shared banner", async () => {
    vi.mocked(api.outboxGetRaw).mockRejectedValueOnce({ message: "no raw" });
    const store = useOutboxStore();

    await store.fetchRaw(9);

    expect(store.rawError[9]).toBe("no raw");
    expect(store.rawCache[9]).toBeUndefined();
    expect(store.error).toBeNull(); // shared banner untouched
  });
});

describe("useOutboxStore retry()", () => {
  it("guards re-entry while a retry is in flight", async () => {
    const store = useOutboxStore();
    // First call leaves retryLoading[1] true until it resolves; assert the guard short-
    // circuits a concurrent second call.
    const first = store.retry(1);
    const second = store.retry(1);
    await Promise.all([first, second]);

    expect(api.outboxRetry).toHaveBeenCalledOnce();
    expect(store.retryLoading[1]).toBe(false);
  });

  it("deterministically refreshes after a SUCCESSFUL retry (pr-review F5)", async () => {
    vi.mocked(api.outboxRetry).mockResolvedValueOnce(undefined);
    // The post-retry snapshot reflects the new status (the row flips dead → pending).
    vi.mocked(api.outboxList).mockResolvedValueOnce([{ ...entry(3), status: "pending" }]);
    const store = useOutboxStore();

    await store.retry(3);

    expect(api.outboxRetry).toHaveBeenCalledWith(3);
    // Reconciles via refresh even without the re-emitted outbox:updated event.
    expect(api.outboxList).toHaveBeenCalledOnce();
    expect(store.entries.find((e) => e.id === 3)?.status).toBe("pending");
    expect(store.error).toBeNull();
  });

  it("refreshes as a fallback when the retry command rejects", async () => {
    vi.mocked(api.outboxRetry).mockRejectedValueOnce({ message: "retry failed" });
    const store = useOutboxStore();

    await store.retry(2);

    expect(store.error).toBe("retry failed");
    // The re-emit won't come, so it pulls a fresh snapshot to reconcile.
    expect(api.outboxList).toHaveBeenCalledOnce();
  });
});

describe("useOutboxStore init()", () => {
  it("subscribes BEFORE reading the snapshot (race guard)", async () => {
    const order: string[] = [];
    vi.mocked(api.onOutboxUpdated).mockImplementation((cb) => {
      order.push("subscribe");
      outboxCb = cb;
      return Promise.resolve(() => {});
    });
    vi.mocked(api.outboxList).mockImplementation(() => {
      order.push("snapshot");
      return Promise.resolve([]);
    });
    const store = useOutboxStore();

    const unlisten = await store.init();

    expect(order).toEqual(["subscribe", "snapshot"]);
    expect(typeof (await unlisten)).toBe("function");
  });
});
