// useReviewStore tests. Drives the factory store against a mocked `./api` module
// so the assertions stay deterministic — mirrors the mock style of
// src/pr/usePrStore.test.ts.
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ReviewEvent } from "../types";
import type { ReviewSession } from "./types";

vi.mock("./api", () => ({
  getCodexStatus: vi.fn(() =>
    Promise.resolve({ available: true, desiredRunning: true, message: "ok" }),
  ),
  startCodex: vi.fn(() =>
    Promise.resolve({ available: true, desiredRunning: true, message: "ok" }),
  ),
  stopCodex: vi.fn(() =>
    Promise.resolve({
      available: false,
      desiredRunning: false,
      message: "codex app-server 已停止",
    }),
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
    desiredRunning: true,
    message: "ok",
  });
  vi.mocked(api.startReview).mockResolvedValue("th_1");
  vi.mocked(api.stopReview).mockResolvedValue();
  vi.mocked(api.listReviewSessions).mockResolvedValue([]);
  // Module-level singleton state: reset between tests so each starts clean.
  const s = useReviewStore();
  s.codex.value = null;
  s.sessions.value = [];
  s.items.value = [];
  s.running.value = false;
  s.finalStatus.value = null;
  s.error.value = null;
  s.activeThreadId.value = null;
  s.activePr.value = null;
  s.listenerReady.value = false;
  s.listenerError.value = null;
  s.dispatchError.value = null;
});

// One backend session row; spread an override to vary a field.
function session(over: Partial<ReviewSession> = {}): ReviewSession {
  return {
    threadId: "th_1",
    turnId: "tn_1",
    prNumber: 7,
    kind: "review",
    status: "running",
    ...over,
  };
}

describe("useReviewStore refreshCodexStatus()", () => {
  it("writes codex.value from getCodexStatus on success", async () => {
    const store = useReviewStore();
    expect(store.codex.value).toBeNull();

    await store.refreshCodexStatus();

    expect(api.getCodexStatus).toHaveBeenCalledOnce();
    expect(store.codex.value).toEqual({
      available: true,
      desiredRunning: true,
      message: "ok",
    });
  });

  it("on a rejected invoke sets an unavailable codex status", async () => {
    vi.mocked(api.getCodexStatus).mockRejectedValueOnce({ message: "boom" });
    const store = useReviewStore();

    await store.refreshCodexStatus();

    expect(store.codex.value?.available).toBe(false);
    expect(store.codex.value?.message).toBe("boom");
  });
});

describe("useReviewStore startCodexServer()/stopCodexServer()", () => {
  it("startCodexServer writes a running codex status from startCodex", async () => {
    const store = useReviewStore();

    await store.startCodexServer();

    expect(api.startCodex).toHaveBeenCalledOnce();
    expect(store.codex.value).toEqual({
      available: true,
      desiredRunning: true,
      message: "ok",
    });
  });

  it("startCodexServer on a rejected invoke marks intent-to-run unavailable", async () => {
    vi.mocked(api.startCodex).mockRejectedValueOnce({ message: "boom" });
    const store = useReviewStore();

    await store.startCodexServer();

    expect(store.codex.value?.available).toBe(false);
    expect(store.codex.value?.desiredRunning).toBe(true);
    expect(store.codex.value?.message).toBe("boom");
  });

  it("stopCodexServer writes a stopped codex status from stopCodex", async () => {
    const store = useReviewStore();

    await store.stopCodexServer();

    expect(api.stopCodex).toHaveBeenCalledOnce();
    expect(store.codex.value?.available).toBe(false);
    expect(store.codex.value?.desiredRunning).toBe(false);
  });

  it("stopCodexServer on a rejected invoke still marks it stopped", async () => {
    vi.mocked(api.stopCodex).mockRejectedValueOnce({ message: "gone" });
    const store = useReviewStore();

    await store.stopCodexServer();

    expect(store.codex.value?.available).toBe(false);
    expect(store.codex.value?.desiredRunning).toBe(false);
    expect(store.codex.value?.message).toBe("gone");
  });
});

describe("useReviewStore applyEvent()", () => {
  const md = (itemId: string, text: string): ReviewEvent => ({
    kind: "messageDelta",
    threadId: "th_1",
    itemId,
    text,
  });

  // The stream only renders the FOCUSED session (see applyEvent's drop-when-unfocused
  // guard), so these tests focus `th_1` — the threadId `md`/the terminal events use —
  // before asserting on stream state.
  beforeEach(() => {
    useReviewStore().activeThreadId.value = "th_1";
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

  it("drops stream deltas when no session is focused (concurrent auto-sessions don't mix)", () => {
    const store = useReviewStore();
    // Override the describe's focus: nothing focused and no manual start in flight,
    // so an auto-started session's deltas must NOT pile into the (unfocused) panel.
    store.activeThreadId.value = null;
    store.applyEvent({
      kind: "messageDelta",
      threadId: "auto_1",
      itemId: "i1",
      text: "x",
    });
    expect(store.items.value).toEqual([]);
  });

  it("dispatchError sets the session-less notice without touching the stream", () => {
    const store = useReviewStore();
    store.applyEvent(md("i1", "hi")); // an existing session item
    store.applyEvent({ kind: "dispatchError", message: "配置无效" });

    expect(store.dispatchError.value).toBe("配置无效");
    // Session-less: must not be folded into the stream or the per-session error.
    expect(store.items.value).toHaveLength(1);
    expect(store.error.value).toBeNull();
  });

  it("dispatchError is surfaced even while a session is focused (not threadId-filtered)", () => {
    const store = useReviewStore();
    store.activeThreadId.value = "th_1"; // a focused session would drop foreign events
    store.applyEvent({ kind: "dispatchError", message: "ledger 落账失败" });

    expect(store.dispatchError.value).toBe("ledger 落账失败");
  });

  it("clearDispatchError dismisses the notice", () => {
    const store = useReviewStore();
    store.applyEvent({ kind: "dispatchError", message: "boom" });
    expect(store.dispatchError.value).toBe("boom");

    store.clearDispatchError();
    expect(store.dispatchError.value).toBeNull();
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

  it("stop on a rejected invoke clears running and surfaces the error", async () => {
    vi.mocked(api.stopReview).mockRejectedValueOnce({ message: "gone" });
    const store = useReviewStore();
    store.activeThreadId.value = "th_1";
    store.running.value = true;

    await store.stop();

    expect(store.running.value).toBe(false); // not stuck.
    expect(store.error.value).toBe("gone");
  });
});

describe("useReviewStore init()", () => {
  it("registers the review-event listener and returns an unlisten fn", async () => {
    const store = useReviewStore();
    const unlisten = await store.init();
    expect(api.onReviewEvent).toHaveBeenCalledOnce();
    expect(typeof unlisten).toBe("function");
  });

  it("marks listenerReady once the listener attaches (gates start)", async () => {
    const store = useReviewStore();
    expect(store.listenerReady.value).toBe(false);

    await store.init();

    expect(store.listenerReady.value).toBe(true);
    expect(store.listenerError.value).toBeNull();
  });

  it("surfaces a listener registration failure and returns a noop unlisten", async () => {
    vi.mocked(api.onReviewEvent).mockRejectedValueOnce(new Error("listen failed"));
    const store = useReviewStore();

    const unlisten = await store.init();

    expect(store.listenerReady.value).toBe(false);
    expect(store.listenerError.value).toBe("listen failed");
    expect(typeof unlisten).toBe("function");
  });

  it("reattaches to a still-active backend session", async () => {
    vi.mocked(api.listReviewSessions).mockResolvedValueOnce([
      {
        threadId: "th_live",
        turnId: "tn",
        prNumber: 42,
        kind: "review",
        status: "running",
      },
    ]);
    const store = useReviewStore();

    await store.init();

    expect(store.activeThreadId.value).toBe("th_live");
    expect(store.activePr.value).toBe(42);
    expect(store.running.value).toBe(true);
  });

  it("leaves state clean when no backend session is active", async () => {
    vi.mocked(api.listReviewSessions).mockResolvedValueOnce([
      {
        threadId: "th_done",
        turnId: "tn",
        prNumber: 1,
        kind: "review",
        status: "done",
      },
    ]);
    const store = useReviewStore();

    await store.init();

    expect(store.activeThreadId.value).toBeNull();
    expect(store.running.value).toBe(false);
  });
});

describe("useReviewStore start() in-flight buffering", () => {
  it("buffers events while the id is unknown and drops foreign sessions on replay", async () => {
    // Hold startReview open so we can inject events into the id-unknown window.
    let release!: (id: string) => void;
    vi.mocked(api.startReview).mockReturnValueOnce(
      new Promise<string>((resolve) => {
        release = resolve;
      }),
    );
    const store = useReviewStore();
    const startPromise = store.start(7, "review");

    // Events arriving before the id resolves: one foreign, one for our session.
    store.applyEvent({
      kind: "messageDelta",
      threadId: "foreign",
      itemId: "x",
      text: "NOPE",
    });
    store.applyEvent({
      kind: "messageDelta",
      threadId: "th_1",
      itemId: "i1",
      text: "yes",
    });
    // Buffered, not yet applied.
    expect(store.items.value).toEqual([]);

    release("th_1");
    await startPromise;

    // Only our own session's event survives the filtered replay.
    expect(store.items.value).toEqual([
      { itemId: "i1", kind: "message", text: "yes" },
    ]);
  });
});

describe("useReviewStore refreshSessions()", () => {
  it("populates sessions from listReviewSessions, sorted by threadId", async () => {
    vi.mocked(api.listReviewSessions).mockResolvedValueOnce([
      session({ threadId: "th_b", prNumber: 2 }),
      session({ threadId: "th_a", prNumber: 1 }),
    ]);
    const store = useReviewStore();

    await store.refreshSessions();

    expect(api.listReviewSessions).toHaveBeenCalledOnce();
    expect(store.sessions.value.map((s) => s.threadId)).toEqual([
      "th_a",
      "th_b",
    ]);
  });

  it("keeps the prior value when the command rejects", async () => {
    const store = useReviewStore();
    store.sessions.value = [session({ threadId: "th_prior" })];
    vi.mocked(api.listReviewSessions).mockRejectedValueOnce({ message: "boom" });

    await store.refreshSessions();

    expect(store.sessions.value.map((s) => s.threadId)).toEqual(["th_prior"]);
  });
});

describe("useReviewStore applyEvent() sessions refresh", () => {
  // The refresh is fire-and-forget (`void refreshSessions()`); a microtask flush
  // lets the awaited list assignment settle before assertions.
  const flush = () => Promise.resolve();

  it("refreshes when an event arrives for an unseen threadId", async () => {
    vi.mocked(api.listReviewSessions).mockResolvedValue([
      session({ threadId: "th_new", prNumber: 9 }),
    ]);
    const store = useReviewStore();
    expect(store.sessions.value).toEqual([]); // th_new not yet listed.

    store.applyEvent({
      kind: "messageDelta",
      threadId: "th_new",
      itemId: "i1",
      text: "hi",
    });
    await flush();

    expect(api.listReviewSessions).toHaveBeenCalledOnce();
    expect(store.sessions.value.map((s) => s.threadId)).toEqual(["th_new"]);
  });

  it("does not refresh for an event whose threadId is already listed", async () => {
    const store = useReviewStore();
    store.sessions.value = [session({ threadId: "th_1" })];

    store.applyEvent({
      kind: "messageDelta",
      threadId: "th_1",
      itemId: "i1",
      text: "hi",
    });
    await flush();

    expect(api.listReviewSessions).not.toHaveBeenCalled();
  });

  it("refreshes on a terminal turnCompleted event", async () => {
    vi.mocked(api.listReviewSessions).mockResolvedValue([
      session({ threadId: "th_1", status: "done" }),
    ]);
    const store = useReviewStore();
    store.sessions.value = [session({ threadId: "th_1", status: "running" })];

    store.applyEvent({
      kind: "turnCompleted",
      threadId: "th_1",
      status: "completed",
    });
    await flush();

    expect(api.listReviewSessions).toHaveBeenCalledOnce();
    expect(store.sessions.value[0]?.status).toBe("done");
  });

  it("refreshes on a terminal error event", async () => {
    vi.mocked(api.listReviewSessions).mockResolvedValue([
      session({ threadId: "th_1", status: "failed" }),
    ]);
    const store = useReviewStore();
    store.sessions.value = [session({ threadId: "th_1", status: "running" })];

    store.applyEvent({ kind: "error", threadId: "th_1", message: "boom" });
    await flush();

    expect(api.listReviewSessions).toHaveBeenCalledOnce();
    expect(store.sessions.value[0]?.status).toBe("failed");
  });
});

describe("useReviewStore focus()", () => {
  it("points the focused stream at a session and clears prior stream state", () => {
    const store = useReviewStore();
    store.items.value = [{ itemId: "stale", kind: "message", text: "old" }];
    store.error.value = "old error";
    store.finalStatus.value = "completed";

    store.focus("th_pick", 42, "running");

    expect(store.activeThreadId.value).toBe("th_pick");
    expect(store.activePr.value).toBe(42);
    expect(store.running.value).toBe(true);
    expect(store.items.value).toEqual([]);
    expect(store.error.value).toBeNull();
    expect(store.finalStatus.value).toBeNull();
  });

  it("marks a terminal session as not running", () => {
    const store = useReviewStore();

    store.focus("th_done", 7, "done");

    expect(store.running.value).toBe(false);
  });

  it("treats a starting session as running", () => {
    const store = useReviewStore();

    store.focus("th_starting", 7, "starting");

    expect(store.running.value).toBe(true);
  });

  it("treats an interrupting session as running", () => {
    const store = useReviewStore();

    store.focus("th_interrupting", 7, "interrupting");

    expect(store.running.value).toBe(true);
  });

  it("renders a focused done session as ended, not 未开始", () => {
    const store = useReviewStore();

    store.focus("th_done", 7, "done");

    // A terminal status must surface so ReviewPanel shows "已结束", not "未开始".
    expect(store.running.value).toBe(false);
    expect(store.finalStatus.value).toBe("completed");
  });

  it("renders a focused failed session with a failed terminal status", () => {
    const store = useReviewStore();

    store.focus("th_failed", 7, "failed");

    expect(store.running.value).toBe(false);
    expect(store.finalStatus.value).toBe("failed");
  });
});
