// useReviewStore tests. Drives the factory store against a mocked `./api` module
// so the assertions stay deterministic — mirrors the mock style of
// src/pr/usePrStore.test.ts.
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ReviewEvent } from "../types";

vi.mock("./api", () => ({
  getCodexStatus: vi.fn(() =>
    Promise.resolve({ available: true, message: "ok" }),
  ),
  startReview: vi.fn(() => Promise.resolve("th_1")),
  stopReview: vi.fn(() => Promise.resolve()),
  listReviewSessions: vi.fn(() => Promise.resolve([])),
  onReviewEvent: vi.fn(() => Promise.resolve(() => {})),
}));

import * as api from "./api";
import { useReviewStore } from "./useReviewStore";

beforeEach(() => {
  vi.clearAllMocks();
  // Restore default resolved behavior wiped by clearAllMocks.
  vi.mocked(api.getCodexStatus).mockResolvedValue({
    available: true,
    message: "ok",
  });
  vi.mocked(api.startReview).mockResolvedValue("th_1");
  vi.mocked(api.stopReview).mockResolvedValue();
  // Module-level singleton state: reset between tests so each starts clean.
  const s = useReviewStore();
  s.codex.value = null;
  s.items.value = [];
  s.running.value = false;
  s.finalStatus.value = null;
  s.error.value = null;
  s.activeThreadId.value = null;
  s.activePr.value = null;
});

describe("useReviewStore refreshCodexStatus()", () => {
  it("writes codex.value from getCodexStatus on success", async () => {
    const store = useReviewStore();
    expect(store.codex.value).toBeNull();

    await store.refreshCodexStatus();

    expect(api.getCodexStatus).toHaveBeenCalledOnce();
    expect(store.codex.value).toEqual({ available: true, message: "ok" });
  });

  it("on a rejected invoke sets an unavailable codex status", async () => {
    vi.mocked(api.getCodexStatus).mockRejectedValueOnce({ message: "boom" });
    const store = useReviewStore();

    await store.refreshCodexStatus();

    expect(store.codex.value?.available).toBe(false);
    expect(store.codex.value?.message).toBe("boom");
  });
});

describe("useReviewStore applyEvent()", () => {
  const md = (itemId: string, text: string): ReviewEvent => ({
    kind: "messageDelta",
    threadId: "th_1",
    itemId,
    text,
  });

  it("concatenates message deltas sharing an itemId", () => {
    const store = useReviewStore();
    store.applyEvent(md("i1", "Hel"));
    store.applyEvent(md("i1", "lo"));

    expect(store.items.value).toEqual([
      { itemId: "i1", kind: "message", text: "Hello" },
    ]);
  });

  it("keeps distinct itemIds as separate ordered items", () => {
    const store = useReviewStore();
    store.applyEvent(md("i1", "a"));
    store.applyEvent({
      kind: "reasoningDelta",
      threadId: "th_1",
      itemId: "r1",
      text: "why",
    });

    expect(store.items.value).toEqual([
      { itemId: "i1", kind: "message", text: "a" },
      { itemId: "r1", kind: "reasoning", text: "why" },
    ]);
  });

  it("turnCompleted clears running and records the status", () => {
    const store = useReviewStore();
    store.running.value = true;
    store.applyEvent({
      kind: "turnCompleted",
      threadId: "th_1",
      status: "interrupted",
    });

    expect(store.running.value).toBe(false);
    expect(store.finalStatus.value).toBe("interrupted");
  });

  it("error event surfaces the message and clears running", () => {
    const store = useReviewStore();
    store.running.value = true;
    store.applyEvent({ kind: "error", threadId: "th_1", message: "boom" });

    expect(store.error.value).toBe("boom");
    expect(store.running.value).toBe(false);
  });

  it("ignores events from a different session once the active id is known", () => {
    const store = useReviewStore();
    store.activeThreadId.value = "th_1";
    store.applyEvent({
      kind: "messageDelta",
      threadId: "other",
      itemId: "x",
      text: "nope",
    });
    expect(store.items.value).toEqual([]);

    store.applyEvent(md("i1", "yes"));
    expect(store.items.value).toHaveLength(1);
  });
});

describe("useReviewStore start()/stop()", () => {
  it("start resets, invokes startReview, and records the session id", async () => {
    const store = useReviewStore();
    store.items.value = [{ itemId: "stale", kind: "message", text: "old" }];

    await store.start(7, "review");

    expect(api.startReview).toHaveBeenCalledWith(7, "review");
    expect(store.activeThreadId.value).toBe("th_1");
    expect(store.activePr.value).toBe(7);
    expect(store.running.value).toBe(true);
    expect(store.items.value).toEqual([]); // reset for the new session.
  });

  it("start on a rejected invoke clears running and surfaces the error", async () => {
    vi.mocked(api.startReview).mockRejectedValueOnce({ message: "nope" });
    const store = useReviewStore();

    await store.start(7, "review");

    expect(store.running.value).toBe(false);
    expect(store.error.value).toBe("nope");
  });

  it("stop interrupts the active session by id", async () => {
    const store = useReviewStore();
    store.activeThreadId.value = "th_1";

    await store.stop();

    expect(api.stopReview).toHaveBeenCalledWith("th_1");
  });

  it("stop is a no-op when there is no active session", async () => {
    const store = useReviewStore();
    await store.stop();
    expect(api.stopReview).not.toHaveBeenCalled();
  });
});
