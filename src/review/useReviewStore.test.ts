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
  getClaudeStatus: vi.fn(() =>
    Promise.resolve({ available: true, message: "ok" }),
  ),
  getCursorStatus: vi.fn(() =>
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
  startCursor: vi.fn(() =>
    Promise.resolve({ available: true, desiredRunning: true, message: "ok" }),
  ),
  stopCursor: vi.fn(() =>
    Promise.resolve({
      available: false,
      desiredRunning: false,
      message: "cursor ACP 已停止",
    }),
  ),
  startReview: vi.fn(() => Promise.resolve("th_1")),
  stopReview: vi.fn(() => Promise.resolve()),
  listReviewSessions: vi.fn(() => Promise.resolve([])),
  getPrSessions: vi.fn(() => Promise.resolve([])),
  getSessionHistory: vi.fn(() => Promise.resolve([])),
  onReviewEvent: vi.fn(() => Promise.resolve(() => {})),
}));

import * as api from "./api";
import { useReviewStore } from "./useReviewStore";
import { useProjects } from "../projects";

beforeEach(() => {
  vi.clearAllMocks();
  // Restore default resolved behavior wiped by clearAllMocks.
  vi.mocked(api.getCodexStatus).mockResolvedValue({
    available: true,
    desiredRunning: true,
    message: "ok",
  });
  vi.mocked(api.getClaudeStatus).mockResolvedValue({
    available: true,
    message: "ok",
  });
  vi.mocked(api.getCursorStatus).mockResolvedValue({
    available: true,
    desiredRunning: true,
    message: "ok",
  });
  vi.mocked(api.startReview).mockResolvedValue("th_1");
  vi.mocked(api.stopReview).mockResolvedValue();
  vi.mocked(api.listReviewSessions).mockResolvedValue([]);
  vi.mocked(api.getPrSessions).mockResolvedValue([]);
  vi.mocked(api.getSessionHistory).mockResolvedValue([]);
  // Module-level singleton state: reset between tests so each starts clean.
  const s = useReviewStore();
  s.codex.value = null;
  s.claude.value = null;
  s.cursor.value = null;
  s.sessions.value = [];
  s.items.value = [];
  s.running.value = false;
  s.finalStatus.value = null;
  s.error.value = null;
  s.activeThreadId.value = null;
  s.activePr.value = null;
  s.listenerReady.value = false;
  s.listenerError.value = null;
  s.dispatchError.value = {};
  // hydrateActiveSession (#35) reattaches only within the active project; reset the
  // shared singleton so each test starts from a known active selection.
  useProjects().activeProjectId.value = "";
});

// One backend session row; spread an override to vary a field.
function session(over: Partial<ReviewSession> = {}): ReviewSession {
  return {
    projectId: "p1",
    threadId: "th_1",
    turnId: "tn_1",
    prNumber: 7,
    kind: "review",
    engineKind: "codex",
    status: "running",
    createdAtEpoch: 0,
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

describe("useReviewStore refreshClaudeStatus()", () => {
  it("writes claude.value from getClaudeStatus on success", async () => {
    const store = useReviewStore();
    expect(store.claude.value).toBeNull();

    await store.refreshClaudeStatus();

    expect(api.getClaudeStatus).toHaveBeenCalledOnce();
    expect(store.claude.value).toEqual({ available: true, message: "ok" });
  });

  it("on a rejected invoke sets an unavailable claude status", async () => {
    vi.mocked(api.getClaudeStatus).mockRejectedValueOnce({ message: "boom" });
    const store = useReviewStore();

    await store.refreshClaudeStatus();

    expect(store.claude.value?.available).toBe(false);
    expect(store.claude.value?.message).toBe("boom");
  });
});

describe("useReviewStore refreshCursorStatus()", () => {
  it("writes cursor.value from getCursorStatus on success", async () => {
    const store = useReviewStore();
    expect(store.cursor.value).toBeNull();

    await store.refreshCursorStatus();

    expect(api.getCursorStatus).toHaveBeenCalledOnce();
    expect(store.cursor.value).toEqual({
      available: true,
      desiredRunning: true,
      message: "ok",
    });
  });

  it("on a rejected invoke sets an unavailable cursor status", async () => {
    vi.mocked(api.getCursorStatus).mockRejectedValueOnce({ message: "boom" });
    const store = useReviewStore();

    await store.refreshCursorStatus();

    expect(store.cursor.value?.available).toBe(false);
    expect(store.cursor.value?.desiredRunning).toBe(true);
    expect(store.cursor.value?.message).toBe("boom");
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

describe("useReviewStore startCursorServer()/stopCursorServer()", () => {
  it("startCursorServer writes a running cursor status from startCursor", async () => {
    const store = useReviewStore();

    await store.startCursorServer();

    expect(api.startCursor).toHaveBeenCalledOnce();
    expect(store.cursor.value).toEqual({
      available: true,
      desiredRunning: true,
      message: "ok",
    });
  });

  it("startCursorServer on a rejected invoke marks intent-to-run unavailable", async () => {
    vi.mocked(api.startCursor).mockRejectedValueOnce({ message: "boom" });
    const store = useReviewStore();

    await store.startCursorServer();

    expect(store.cursor.value?.available).toBe(false);
    expect(store.cursor.value?.desiredRunning).toBe(true);
    expect(store.cursor.value?.message).toBe("boom");
  });

  it("stopCursorServer writes a stopped cursor status from stopCursor", async () => {
    const store = useReviewStore();

    await store.stopCursorServer();

    expect(api.stopCursor).toHaveBeenCalledOnce();
    expect(store.cursor.value?.available).toBe(false);
    expect(store.cursor.value?.desiredRunning).toBe(false);
  });

  it("stopCursorServer on a rejected invoke still marks it stopped", async () => {
    vi.mocked(api.stopCursor).mockRejectedValueOnce({ message: "gone" });
    const store = useReviewStore();

    await store.stopCursorServer();

    expect(store.cursor.value?.available).toBe(false);
    expect(store.cursor.value?.desiredRunning).toBe(false);
    expect(store.cursor.value?.message).toBe("gone");
  });
});

describe("useReviewStore applyEvent()", () => {
  const md = (itemId: string, text: string): ReviewEvent => ({
    kind: "messageDelta",
    projectId: "p1",
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
      projectId: "p1",
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
      projectId: "p1",
      threadId: "th_1",
      status: "interrupted",
    });

    expect(store.running.value).toBe(false);
    expect(store.finalStatus.value).toBe("interrupted");
  });

  it("error event surfaces the message and clears running", () => {
    const store = useReviewStore();
    store.running.value = true;
    store.applyEvent({
      kind: "error",
      projectId: "p1",
      threadId: "th_1",
      message: "boom",
    });

    expect(store.error.value).toBe("boom");
    expect(store.running.value).toBe(false);
  });

  it("ignores events from a different session once the active id is known", () => {
    const store = useReviewStore();
    store.activeThreadId.value = "th_1";
    store.applyEvent({
      kind: "messageDelta",
      projectId: "p1",
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
      projectId: "p1",
      threadId: "auto_1",
      itemId: "i1",
      text: "x",
    });
    expect(store.items.value).toEqual([]);
  });

  it("dispatchError sets the per-project notice without touching the stream", () => {
    const store = useReviewStore();
    store.applyEvent(md("i1", "hi")); // an existing session item
    store.applyEvent({ kind: "dispatchError", projectId: "p1", message: "配置无效" });

    expect(store.dispatchError.value.p1).toBe("配置无效");
    // Session-less: must not be folded into the stream or the per-session error.
    expect(store.items.value).toHaveLength(1);
    expect(store.error.value).toBeNull();
  });

  it("dispatchError is keyed per project (#35): p2's notice doesn't touch p1", () => {
    const store = useReviewStore();
    store.applyEvent({ kind: "dispatchError", projectId: "p2", message: "配置无效" });

    expect(store.dispatchError.value.p2).toBe("配置无效");
    expect(store.dispatchError.value.p1).toBeUndefined();
  });

  it("dispatchError is surfaced even while a session is focused (not threadId-filtered)", () => {
    const store = useReviewStore();
    store.activeThreadId.value = "th_1"; // a focused session would drop foreign events
    store.applyEvent({
      kind: "dispatchError",
      projectId: "p1",
      message: "ledger 落账失败",
    });

    expect(store.dispatchError.value.p1).toBe("ledger 落账失败");
  });

  it("clearDispatchError dismisses one project's notice", () => {
    const store = useReviewStore();
    store.applyEvent({ kind: "dispatchError", projectId: "p1", message: "boom" });
    expect(store.dispatchError.value.p1).toBe("boom");

    store.clearDispatchError("p1");
    expect(store.dispatchError.value.p1).toBeNull();
  });
});

describe("useReviewStore start()/stop()", () => {
  it("start resets, invokes startReview, and records the session id", async () => {
    const store = useReviewStore();
    store.items.value = [{ itemId: "stale", kind: "message", text: "old" }];

    await store.start("p1", 7, "review");

    expect(api.startReview).toHaveBeenCalledWith("p1", 7, "review");
    expect(store.activeThreadId.value).toBe("th_1");
    expect(store.activePr.value).toBe(7);
    expect(store.running.value).toBe(true);
    expect(store.items.value).toEqual([]); // reset for the new session.
  });

  it("start on a rejected invoke clears running and surfaces the error", async () => {
    vi.mocked(api.startReview).mockRejectedValueOnce({ message: "nope" });
    const store = useReviewStore();

    await store.start("p1", 7, "review");

    expect(store.running.value).toBe(false);
    expect(store.error.value).toBe("nope");
  });

  it("start on a cursor project refreshes cursor status after success", async () => {
    useProjects().projects.value = [
      {
        id: "p1",
        name: "Cursor",
        engineKind: "cursor",
      } as never,
    ];
    const store = useReviewStore();
    await store.start("p1", 7, "review");
    expect(api.getCursorStatus).toHaveBeenCalled();
  });

  it("start failure on a cursor project still refreshes cursor status", async () => {
    useProjects().projects.value = [
      {
        id: "p1",
        name: "Cursor",
        engineKind: "cursor",
      } as never,
    ];
    vi.mocked(api.startReview).mockRejectedValueOnce({ message: "nope" });
    const store = useReviewStore();
    await store.start("p1", 7, "review");
    expect(store.error.value).toBe("nope");
    expect(api.getCursorStatus).toHaveBeenCalled();
  });

  it("start on a non-cursor project does not refresh cursor status", async () => {
    useProjects().projects.value = [
      {
        id: "p1",
        name: "Codex",
        engineKind: "codex",
      } as never,
    ];
    const store = useReviewStore();
    await store.start("p1", 7, "review");
    expect(api.getCursorStatus).not.toHaveBeenCalled();
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
    // init() calls listReviewSessions twice (hydrateActiveSession + refreshSessions);
    // a persistent mock so BOTH the reattach pick and the session-list seed see it.
    vi.mocked(api.listReviewSessions).mockResolvedValue([
      {
        projectId: "p1",
        threadId: "th_live",
        turnId: "tn",
        prNumber: 42,
        kind: "review",
        engineKind: "codex",
        status: "running",
        createdAtEpoch: 0,
      },
    ]);
    const store = useReviewStore();
    // hydrateActiveSession filters by the active project (#35) — point it at p1.
    useProjects().activeProjectId.value = "p1";

    await store.init();

    expect(store.activeThreadId.value).toBe("th_live");
    expect(store.activePr.value).toBe(42);
    expect(store.running.value).toBe(true);
    // init()'s refreshSessions ran: the concurrent-session list is seeded too.
    expect(store.sessions.value.length).toBeGreaterThan(0);
    expect(store.sessions.value[0]?.threadId).toBe("th_live");
  });

  it("leaves state clean when no backend session is active", async () => {
    vi.mocked(api.listReviewSessions).mockResolvedValueOnce([
      {
        projectId: "p1",
        threadId: "th_done",
        turnId: "tn",
        prNumber: 1,
        kind: "review",
        engineKind: "codex",
        status: "done",
        createdAtEpoch: 0,
      },
    ]);
    const store = useReviewStore();
    // Active project matches the session's, so the terminal STATUS is what blocks the
    // reattach here (not the #35 project filter).
    useProjects().activeProjectId.value = "p1";

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
    const startPromise = store.start("p1", 7, "review");

    // Events arriving before the id resolves: one foreign, one for our session.
    store.applyEvent({
      kind: "messageDelta",
      projectId: "p1",
      threadId: "foreign",
      itemId: "x",
      text: "NOPE",
    });
    store.applyEvent({
      kind: "messageDelta",
      projectId: "p1",
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
      projectId: "p1",
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
      projectId: "p1",
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
      projectId: "p1",
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

    store.applyEvent({
      kind: "error",
      projectId: "p1",
      threadId: "th_1",
      message: "boom",
    });
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

    store.focus("p1", "th_pick", 42, "running");

    expect(store.activeThreadId.value).toBe("th_pick");
    expect(store.activePr.value).toBe(42);
    expect(store.running.value).toBe(true);
    expect(store.items.value).toEqual([]);
    expect(store.error.value).toBeNull();
    expect(store.finalStatus.value).toBeNull();
  });

  it("marks a terminal session as not running", () => {
    const store = useReviewStore();

    store.focus("p1", "th_done", 7, "done");

    expect(store.running.value).toBe(false);
  });

  it("hydrates the panel with the session's persisted history (#70)", async () => {
    const store = useReviewStore();
    vi.mocked(api.getSessionHistory).mockResolvedValueOnce([
      { itemId: "i1", kind: "reasoning", text: "planned" },
      { itemId: "i2", kind: "message", text: "done" },
    ]);

    await store.focus("p1", "th_hist", 7, "done");

    expect(api.getSessionHistory).toHaveBeenCalledWith("p1", 7, "th_hist");
    expect(store.items.value).toEqual([
      { itemId: "i1", kind: "reasoning", text: "planned" },
      { itemId: "i2", kind: "message", text: "done" },
    ]);
  });

  it("surfaces a history load failure to the user (pr-review F4)", async () => {
    const store = useReviewStore();
    vi.mocked(api.getSessionHistory).mockRejectedValueOnce(new Error("disk gone"));

    await store.focus("p1", "th_err", 7, "done");

    // A rejected getSessionHistory used to only console.error → blank panel, no signal.
    expect(store.error.value).toBe("disk gone");
  });

  it("does not apply stale history after focus switched away mid-load", async () => {
    const store = useReviewStore();
    // First focus's history load is slow; a second focus lands before it resolves.
    let resolveFirst!: (v: { itemId: string; kind: "message"; text: string }[]) => void;
    vi.mocked(api.getSessionHistory)
      .mockImplementationOnce(
        () => new Promise((res) => (resolveFirst = res)),
      )
      .mockResolvedValueOnce([]);

    const first = store.focus("p1", "th_a", 1, "done");
    await store.focus("p1", "th_b", 2, "done"); // switches focus to th_b
    resolveFirst([{ itemId: "x", kind: "message", text: "late" }]);
    await first;

    // th_a's late history must NOT clobber th_b's (now-focused) empty panel.
    expect(store.activeThreadId.value).toBe("th_b");
    expect(store.items.value).toEqual([]);
  });

  it("treats a starting session as running", () => {
    const store = useReviewStore();

    store.focus("p1", "th_starting", 7, "starting");

    expect(store.running.value).toBe(true);
  });

  it("treats an interrupting session as running", () => {
    const store = useReviewStore();

    store.focus("p1", "th_interrupting", 7, "interrupting");

    expect(store.running.value).toBe(true);
  });

  it("renders a focused done session as ended, not 未开始", () => {
    const store = useReviewStore();

    store.focus("p1", "th_done", 7, "done");

    // A terminal status must surface so ReviewPanel shows "已结束", not "未开始".
    expect(store.running.value).toBe(false);
    expect(store.finalStatus.value).toBe("completed");
  });

  it("renders a focused failed session with a failed terminal status", () => {
    const store = useReviewStore();

    store.focus("p1", "th_failed", 7, "failed");

    expect(store.running.value).toBe(false);
    expect(store.finalStatus.value).toBe("failed");
  });

  it("clearFocus() drops the focused session (#35 F5: project switch)", () => {
    const store = useReviewStore();
    // A focused, running session from the project the user is about to leave.
    store.focus("p1", "th_other_project", 7, "running");
    store.items.value = [{ itemId: "i1", kind: "message", text: "hi" }];

    store.clearFocus();

    // Nothing focused → ReviewPanel renders the empty "Select a PR" state for the new
    // project instead of the previous project's session (which its 停止 button would
    // otherwise still target).
    expect(store.activeThreadId.value).toBeNull();
    expect(store.activePr.value).toBeNull();
    expect(store.running.value).toBe(false);
    expect(store.finalStatus.value).toBeNull();
    expect(store.error.value).toBeNull();
    expect(store.items.value).toEqual([]);
  });
});
