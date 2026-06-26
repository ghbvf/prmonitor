// useTerminalStore tests (#1383). Drives the module-singleton factory store against a
// mocked `./api` (mirrors useReviewStore.test.ts). Resets the shared singleton refs +
// the screen sink in beforeEach so each test starts clean.
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { TerminalSession } from "../types";

vi.mock("./api", () => ({
  listTerminalSessions: vi.fn(() => Promise.resolve([])),
  createTerminalSession: vi.fn(() =>
    Promise.resolve({
      sessionId: "new1",
      windowId: "w1",
      tabId: "t1",
      title: "bash",
      isActive: true,
      rows: 24,
      cols: 80,
    }),
  ),
  attachTerminal: vi.fn(() => Promise.resolve()),
  detachTerminal: vi.fn(() => Promise.resolve()),
  sendTerminalInput: vi.fn(() => Promise.resolve()),
  resizeTerminal: vi.fn(() => Promise.resolve()),
  onTerminalEvent: vi.fn(() => Promise.resolve(() => {})),
}));

import * as api from "./api";
import { useTerminalStore } from "./useTerminalStore";

function session(over: Partial<TerminalSession> = {}): TerminalSession {
  return {
    sessionId: "s1",
    windowId: "w1",
    tabId: "t1",
    title: "bash",
    isActive: false,
    rows: 24,
    cols: 80,
    ...over,
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(api.listTerminalSessions).mockResolvedValue([]);
  vi.mocked(api.createTerminalSession).mockResolvedValue(
    session({ sessionId: "new1", isActive: true }),
  );
  vi.mocked(api.attachTerminal).mockResolvedValue();
  vi.mocked(api.detachTerminal).mockResolvedValue();
  vi.mocked(api.sendTerminalInput).mockResolvedValue();
  vi.mocked(api.resizeTerminal).mockResolvedValue();
  vi.mocked(api.onTerminalEvent).mockResolvedValue(() => {});
  // Reset the module-level singleton state so each test starts clean.
  const s = useTerminalStore();
  s.sessions.value = [];
  s.activeSessionId.value = null;
  s.connection.value = "idle";
  s.error.value = null;
  s.listenerReady.value = false;
  s.listenerError.value = null;
  s.unregisterScreenSink();
});

describe("refreshSessions()", () => {
  it("loads the session list from the backend", async () => {
    vi.mocked(api.listTerminalSessions).mockResolvedValueOnce([
      session({ sessionId: "s1" }),
      session({ sessionId: "s2" }),
    ]);
    const store = useTerminalStore();

    await store.refreshSessions();

    expect(api.listTerminalSessions).toHaveBeenCalledOnce();
    expect(store.sessions.value.map((s) => s.sessionId)).toEqual(["s1", "s2"]);
  });

  it("surfaces a rejected list as connection=error without throwing", async () => {
    vi.mocked(api.listTerminalSessions).mockRejectedValueOnce({ message: "boom" });
    const store = useTerminalStore();

    await store.refreshSessions();

    expect(store.error.value).toBe("boom");
    // A reject = daemon precondition failure; flip connection so the error banner shows it.
    expect(store.connection.value).toBe("error");
  });
});

describe("attach()", () => {
  it("focuses the session, marks attaching, and invokes attach_terminal", async () => {
    const store = useTerminalStore();

    await store.attach("s9");

    expect(store.activeSessionId.value).toBe("s9");
    expect(api.attachTerminal).toHaveBeenCalledWith("s9");
  });

  it("on a rejected attach sets connection=error and surfaces the message", async () => {
    vi.mocked(api.attachTerminal).mockRejectedValueOnce({ message: "no session" });
    const store = useTerminalStore();

    await store.attach("s9");

    expect(store.connection.value).toBe("error");
    expect(store.error.value).toBe("no session");
  });

  it("detaches the previously-focused session when switching to a different one", async () => {
    const store = useTerminalStore();

    await store.attach("s1");
    await store.attach("s2");

    // The old session's daemon streamer must be torn down on switch, else it leaks.
    expect(api.detachTerminal).toHaveBeenCalledWith("s1");
    expect(api.attachTerminal).toHaveBeenLastCalledWith("s2");
    expect(store.activeSessionId.value).toBe("s2");
  });

  it("does not detach when re-attaching the same session", async () => {
    const store = useTerminalStore();

    await store.attach("s1");
    await store.attach("s1");

    expect(api.detachTerminal).not.toHaveBeenCalled();
  });

  it("a failed detach of the previous session does not block the new attach", async () => {
    vi.mocked(api.detachTerminal).mockRejectedValueOnce({ message: "detach boom" });
    const store = useTerminalStore();

    await store.attach("s1");
    await store.attach("s2");

    expect(api.detachTerminal).toHaveBeenCalledWith("s1");
    expect(api.attachTerminal).toHaveBeenLastCalledWith("s2");
    expect(store.activeSessionId.value).toBe("s2");
  });
});

describe("createAndAttach()", () => {
  it("creates a session, lists it, and attaches to it", async () => {
    const store = useTerminalStore();

    await store.createAndAttach();

    expect(api.createTerminalSession).toHaveBeenCalledOnce();
    expect(store.sessions.value.some((s) => s.sessionId === "new1")).toBe(true);
    expect(store.activeSessionId.value).toBe("new1");
    expect(api.attachTerminal).toHaveBeenCalledWith("new1");
  });

  it("on a rejected create sets connection=error and surfaces the message", async () => {
    vi.mocked(api.createTerminalSession).mockRejectedValueOnce({ message: "daemon down" });
    const store = useTerminalStore();

    await store.createAndAttach();

    expect(store.connection.value).toBe("error");
    expect(store.error.value).toBe("daemon down");
    expect(api.attachTerminal).not.toHaveBeenCalled();
  });
});

describe("sendInput() guard", () => {
  it("sends when a session is active AND attached", async () => {
    const store = useTerminalStore();
    store.activeSessionId.value = "s1";
    store.connection.value = "attached";

    await store.sendInput("ls\r");

    expect(api.sendTerminalInput).toHaveBeenCalledWith("s1", "ls\r");
  });

  it("drops input when no session is active", async () => {
    const store = useTerminalStore();
    store.connection.value = "attached";

    await store.sendInput("x");

    expect(api.sendTerminalInput).not.toHaveBeenCalled();
  });

  it("drops input when the connection is not attached", async () => {
    const store = useTerminalStore();
    store.activeSessionId.value = "s1";
    store.connection.value = "attaching";

    await store.sendInput("x");

    expect(api.sendTerminalInput).not.toHaveBeenCalled();
  });

  it("on a rejected send sets connection=error so the banner surfaces it", async () => {
    vi.mocked(api.sendTerminalInput).mockRejectedValueOnce({ message: "send boom" });
    const store = useTerminalStore();
    store.activeSessionId.value = "s1";
    store.connection.value = "attached";

    await store.sendInput("x");

    // The error banner is gated on connection==='error' — setting only `error` is invisible.
    expect(store.connection.value).toBe("error");
    expect(store.error.value).toBe("send boom");
  });
});

describe("resize()", () => {
  it("invokes resize_terminal for the active session", async () => {
    const store = useTerminalStore();
    store.activeSessionId.value = "s1";

    await store.resize(100, 40);

    expect(api.resizeTerminal).toHaveBeenCalledWith("s1", 100, 40);
  });

  it("is a no-op with no active session", async () => {
    const store = useTerminalStore();

    await store.resize(100, 40);

    expect(api.resizeTerminal).not.toHaveBeenCalled();
  });

  it("on a rejected resize sets connection=error so the banner surfaces it", async () => {
    vi.mocked(api.resizeTerminal).mockRejectedValueOnce({ message: "resize boom" });
    const store = useTerminalStore();
    store.activeSessionId.value = "s1";

    await store.resize(100, 40);

    expect(store.connection.value).toBe("error");
    expect(store.error.value).toBe("resize boom");
  });
});

describe("detach()", () => {
  it("detaches the active session and resets to idle", async () => {
    const store = useTerminalStore();
    store.activeSessionId.value = "s1";
    store.connection.value = "attached";

    await store.detach();

    expect(api.detachTerminal).toHaveBeenCalledWith("s1");
    expect(store.activeSessionId.value).toBeNull();
    expect(store.connection.value).toBe("idle");
  });

  it("is a no-op with no active session", async () => {
    const store = useTerminalStore();

    await store.detach();

    expect(api.detachTerminal).not.toHaveBeenCalled();
  });

  it("still resets to idle when detach_terminal rejects (finally guarantees it)", async () => {
    vi.mocked(api.detachTerminal).mockRejectedValueOnce({ message: "detach boom" });
    const store = useTerminalStore();
    store.activeSessionId.value = "s1";
    store.connection.value = "attached";

    await store.detach();

    expect(api.detachTerminal).toHaveBeenCalledWith("s1");
    expect(store.activeSessionId.value).toBeNull();
    expect(store.connection.value).toBe("idle");
  });
});

describe("applyEvent()", () => {
  it("attached → connection=attached", () => {
    const store = useTerminalStore();
    store.activeSessionId.value = "s1";

    store.applyEvent({ kind: "attached", sessionId: "s1", cols: 80, rows: 24 });

    expect(store.connection.value).toBe("attached");
  });

  it("screenUpdate updates latestScreen and feeds the screen sink", () => {
    const store = useTerminalStore();
    store.activeSessionId.value = "s1";
    const frames: { data: string; cursorRow?: number; cursorCol?: number }[] = [];
    store.registerScreenSink((f) => frames.push(f));

    store.applyEvent({
      kind: "screenUpdate",
      sessionId: "s1",
      cols: 80,
      rows: 24,
      contents: "hello",
      cursorRow: 1,
      cursorCol: 2,
    });

    expect(frames).toEqual([{ data: "hello", cursorRow: 1, cursorCol: 2 }]);
  });

  it("replays the last screen to a freshly-registered sink (remount)", () => {
    const store = useTerminalStore();
    store.activeSessionId.value = "s1";
    store.registerScreenSink(() => {});
    store.applyEvent({
      kind: "screenUpdate",
      sessionId: "s1",
      cols: 80,
      rows: 24,
      contents: "snap",
    });

    // A new pane mounts: its sink must immediately receive the last frame.
    const replayed: { data: string }[] = [];
    store.registerScreenSink((f) => replayed.push(f));

    expect(replayed).toEqual([{ data: "snap", cursorRow: undefined, cursorCol: undefined }]);
  });

  it("drops an event for a non-active session", () => {
    const store = useTerminalStore();
    store.activeSessionId.value = "s1";
    const frames: unknown[] = [];
    store.registerScreenSink((f) => frames.push(f));
    frames.length = 0; // ignore any replay of a leaked prior frame

    store.applyEvent({
      kind: "screenUpdate",
      sessionId: "other",
      cols: 80,
      rows: 24,
      contents: "NOPE",
    });

    expect(frames).toEqual([]);
    expect(store.connection.value).toBe("idle"); // untouched by the foreign event
  });

  it("sessionEnded → closed, clears the active session, and refreshes the picker", () => {
    const store = useTerminalStore();
    store.activeSessionId.value = "s1";
    store.connection.value = "attached";

    store.applyEvent({ kind: "sessionEnded", sessionId: "s1", reason: "exit" });

    // Keep connection=closed so the "ended" banner shows...
    expect(store.connection.value).toBe("closed");
    // ...but drop the dead session so the frozen pane unmounts and the picker stops
    // highlighting it, and re-list to remove the gone session from the picker.
    expect(store.activeSessionId.value).toBeNull();
    expect(api.listTerminalSessions).toHaveBeenCalledOnce();
  });

  it("session-scoped error → connection=error + message", () => {
    const store = useTerminalStore();
    store.activeSessionId.value = "s1";

    store.applyEvent({ kind: "error", sessionId: "s1", message: "boom" });

    expect(store.connection.value).toBe("error");
    expect(store.error.value).toBe("boom");
  });

  it("connection-level error (no sessionId) surfaces regardless of focus", () => {
    const store = useTerminalStore();
    store.activeSessionId.value = "s1";

    store.applyEvent({ kind: "error", message: "daemon crashed" });

    expect(store.connection.value).toBe("error");
    expect(store.error.value).toBe("daemon crashed");
  });
});

describe("init()", () => {
  it("subscribes BEFORE reading the snapshot and returns an unlisten fn", async () => {
    const order: string[] = [];
    vi.mocked(api.onTerminalEvent).mockImplementationOnce(() => {
      order.push("subscribe");
      return Promise.resolve(() => {});
    });
    vi.mocked(api.listTerminalSessions).mockImplementationOnce(() => {
      order.push("snapshot");
      return Promise.resolve([]);
    });
    const store = useTerminalStore();

    const unlisten = await store.init();

    expect(order).toEqual(["subscribe", "snapshot"]);
    expect(store.listenerReady.value).toBe(true);
    expect(typeof unlisten).toBe("function");
  });

  it("surfaces a listener registration failure and returns a noop unlisten", async () => {
    vi.mocked(api.onTerminalEvent).mockRejectedValueOnce(new Error("listen failed"));
    const store = useTerminalStore();

    const unlisten = await store.init();

    expect(store.listenerReady.value).toBe(false);
    expect(store.listenerError.value).toBe("listen failed");
    expect(typeof unlisten).toBe("function");
    // The snapshot read is skipped when the listener never attached.
    expect(api.listTerminalSessions).not.toHaveBeenCalled();
  });
});
