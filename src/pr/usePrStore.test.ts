// usePrStore state-machine tests (#27 F4). Drives the store against a mocked
// `./api` module so the assertions stay deterministic — the `prs:updated` event
// is simulated by invoking the captured `onPrsUpdated` callback directly rather
// than through the Tauri event bus.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import type { PrEvent, TrackedPrView } from "../types";

// Captured callback handed to `onPrsUpdated`, so a test can push a `PrEvent`
// through the same path `subscribe()` wires up.
let prsCb: ((e: PrEvent) => void) | null = null;

vi.mock("./api", () => ({
  pollNow: vi.fn(() => Promise.resolve()),
  startPolling: vi.fn(() => Promise.resolve()),
  stopPolling: vi.fn(() => Promise.resolve()),
  ghStatus: vi.fn(() => Promise.resolve({ authenticated: true, message: "" })),
  getPrs: vi.fn(() => Promise.resolve([])),
  setPrArchived: vi.fn(() => Promise.resolve()),
  onPrsUpdated: vi.fn((cb: (e: PrEvent) => void) => {
    prsCb = cb;
    // onPrsUpdated returns a Promise<UnlistenFn>.
    return Promise.resolve(() => {});
  }),
}));

import * as api from "./api";
import { usePrStore } from "./usePrStore";

// `track` overrides the tracking fields so a test can mint stale / archived rows;
// the defaults (current + not archived) keep every existing single-arg call valid.
const view = (
  number: number,
  track: Partial<Pick<TrackedPrView, "presence" | "archived">> = {},
): TrackedPrView => ({
  number,
  title: `PR #${number}`,
  labels: [],
  url: `https://example.test/${number}`,
  kind: "review",
  skipReason: null,
  presence: "current",
  archived: false,
  ...track,
});

beforeEach(() => {
  setActivePinia(createPinia());
  prsCb = null;
  vi.clearAllMocks();
  // Restore default resolved behavior wiped by clearAllMocks.
  vi.mocked(api.pollNow).mockResolvedValue(undefined);
  vi.mocked(api.startPolling).mockResolvedValue(undefined);
  vi.mocked(api.stopPolling).mockResolvedValue(undefined);
  vi.mocked(api.getPrs).mockResolvedValue([]);
  vi.mocked(api.setPrArchived).mockResolvedValue(undefined);
  vi.mocked(api.onPrsUpdated).mockImplementation((cb) => {
    prsCb = cb;
    return Promise.resolve(() => {});
  });
});

describe("usePrStore subscribe()", () => {
  it("applies an `updated` event: sets prs, lastPulledAt, clears error + loading", () => {
    const store = usePrStore();
    store.subscribe();
    store.error = "stale";
    store.loading = true;

    const prs = [view(1), view(2)];
    prsCb?.({ kind: "updated", prs });

    expect(store.prs).toEqual(prs);
    expect(store.lastPulledAt).not.toBeNull();
    expect(store.error).toBeNull();
    expect(store.loading).toBe(false);
  });

  it("applies an `error` event: sets error, clears loading", () => {
    const store = usePrStore();
    store.subscribe();
    store.loading = true;

    prsCb?.({ kind: "error", message: "gh exploded" });

    expect(store.error).toBe("gh exploded");
    expect(store.loading).toBe(false);
  });
});

describe("usePrStore pollNow()", () => {
  it("on success leaves loading=true (event clears it later)", async () => {
    const store = usePrStore();
    await store.pollNow();

    expect(api.pollNow).toHaveBeenCalledOnce();
    expect(store.loading).toBe(true);
    expect(store.error).toBeNull();
  });

  it("on a rejected invoke sets error and clears loading", async () => {
    vi.mocked(api.pollNow).mockRejectedValueOnce({ message: "boom" });
    const store = usePrStore();
    await store.pollNow();

    expect(store.error).toBe("boom");
    expect(store.loading).toBe(false);
  });
});

describe("usePrStore toggle()", () => {
  it("polling -> paused calls stopPolling and flips polling=false", async () => {
    const store = usePrStore();
    expect(store.polling).toBe(true);

    await store.toggle();

    expect(api.stopPolling).toHaveBeenCalledOnce();
    expect(store.polling).toBe(false);
    expect(store.error).toBeNull();
  });

  it("on a rejected command sets error and does NOT flip polling", async () => {
    vi.mocked(api.stopPolling).mockRejectedValueOnce({ message: "stop failed" });
    const store = usePrStore();
    expect(store.polling).toBe(true);

    await store.toggle();

    // catch runs before the flip, so polling stays at its prior value.
    expect(store.error).toBe("stop failed");
    expect(store.polling).toBe(true);
  });
});

describe("usePrStore loadSnapshot()", () => {
  it("sets prs from getPrs", async () => {
    const snapshot = [view(7)];
    vi.mocked(api.getPrs).mockResolvedValueOnce(snapshot);
    const store = usePrStore();

    await store.loadSnapshot();

    expect(api.getPrs).toHaveBeenCalledOnce();
    expect(store.prs).toEqual(snapshot);
  });

  it("surfaces a rejected getPrs as error without mutating prs", async () => {
    vi.mocked(api.getPrs).mockRejectedValueOnce({ message: "no snapshot" });
    const store = usePrStore();

    await store.loadSnapshot();

    expect(store.error).toBe("no snapshot");
    expect(store.prs).toEqual([]);
  });
});

describe("usePrStore init()", () => {
  it("subscribes before reading the snapshot baseline", async () => {
    const snapshot = [view(3)];
    vi.mocked(api.getPrs).mockResolvedValueOnce(snapshot);
    const store = usePrStore();

    const unlisten = await store.init();

    // subscribe() wired the listener and loadSnapshot() baselined the list.
    expect(api.onPrsUpdated).toHaveBeenCalledOnce();
    expect(api.getPrs).toHaveBeenCalledOnce();
    expect(store.prs).toEqual(snapshot);
    expect(typeof (await unlisten)).toBe("function");
  });

  it("registers the listener BEFORE reading the snapshot (#27 F3 race guard)", async () => {
    // Capture the actual call order: a snapshot read that landed first would let a
    // `prs:updated` event fired in the gap go unheard. The guard is the await on the
    // listener registration inside init() — assert it observably precedes getPrs.
    const order: string[] = [];
    vi.mocked(api.onPrsUpdated).mockImplementation((cb) => {
      order.push("subscribe");
      prsCb = cb;
      return Promise.resolve(() => {});
    });
    vi.mocked(api.getPrs).mockImplementation(() => {
      order.push("snapshot");
      return Promise.resolve([]);
    });
    const store = usePrStore();

    await store.init();

    expect(order).toEqual(["subscribe", "snapshot"]);
  });
});

describe("usePrStore tracking getters (#38)", () => {
  // One PR of each tracking class, fed through the same `subscribe` path the
  // backend's `prs:updated` push uses, so the getters partition real store state.
  it("partition the retained list into current / stale / archived", () => {
    const store = usePrStore();
    store.subscribe();

    const current = view(1, { presence: "current", archived: false });
    const stale = view(2, { presence: "stale", archived: false });
    const archivedCurrent = view(3, { presence: "current", archived: true });
    const archivedStale = view(4, { presence: "stale", archived: true });
    prsCb?.({
      kind: "updated",
      prs: [current, stale, archivedCurrent, archivedStale],
    });

    // current = not archived AND presence "current".
    expect(store.currentPrs).toEqual([current]);
    // stale = not archived AND presence "stale".
    expect(store.stalePrs).toEqual([stale]);
    // archived = archived regardless of presence (both #3 and #4).
    expect(store.archivedPrs).toEqual([archivedCurrent, archivedStale]);
  });

  it("keep an archived+current PR out of currentPrs (archived wins)", () => {
    const store = usePrStore();
    store.subscribe();

    const archivedCurrent = view(5, { presence: "current", archived: true });
    prsCb?.({ kind: "updated", prs: [archivedCurrent] });

    expect(store.currentPrs).toEqual([]);
    expect(store.archivedPrs).toEqual([archivedCurrent]);
  });
});

describe("usePrStore setArchived()", () => {
  it("invokes setPrArchived with {number, archived}", async () => {
    const store = usePrStore();

    await store.setArchived(42, true);

    expect(api.setPrArchived).toHaveBeenCalledWith(42, true);
    expect(store.error).toBeNull();
  });

  it("surfaces a rejected invoke as error", async () => {
    vi.mocked(api.setPrArchived).mockRejectedValueOnce({ message: "archive failed" });
    const store = usePrStore();

    await store.setArchived(7, false);

    expect(store.error).toBe("archive failed");
  });
});
