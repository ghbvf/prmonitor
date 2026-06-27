import { afterEach, describe, expect, it, vi } from "vitest";

// --- TauriTransport: delegates request→invoke, subscribe→listen (payload-unwrapped),
//     openExternal→openUrl. We mock the three @tauri-apps entry points (the only place
//     they are imported) and assert the adapter forwards to them. --------------------
const invokeMock = vi.fn();
const listenMock = vi.fn();
const openUrlMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: (...args: unknown[]) => listenMock(...args),
}));
vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: (...args: unknown[]) => openUrlMock(...args),
}));

import { createTauriTransport } from "./tauri";
import { createHttpTransport } from "./http";
import type { Transport } from "./index";

const sessionItems = new Map<string, string>();
vi.stubGlobal("sessionStorage", {
  getItem: (key: string) => sessionItems.get(key) ?? null,
  setItem: (key: string, value: string) => sessionItems.set(key, value),
  removeItem: (key: string) => sessionItems.delete(key),
  clear: () => sessionItems.clear(),
});

afterEach(() => {
  vi.clearAllMocks();
  vi.unstubAllGlobals();
  vi.stubGlobal("sessionStorage", {
    getItem: (key: string) => sessionItems.get(key) ?? null,
    setItem: (key: string, value: string) => sessionItems.set(key, value),
    removeItem: (key: string) => sessionItems.delete(key),
    clear: () => sessionItems.clear(),
  });
  sessionItems.clear();
});

describe("TauriTransport", () => {
  it("request delegates to invoke(command, args) and returns its result", async () => {
    invokeMock.mockResolvedValue(["pr1"]);
    const t = createTauriTransport();
    const out = await t.request<string[]>("get_prs", { projectId: "p1" });
    expect(invokeMock).toHaveBeenCalledWith("get_prs", { projectId: "p1" });
    expect(out).toEqual(["pr1"]);
  });

  it("subscribe delegates to listen and unwraps Event.payload before the handler", async () => {
    const unlisten = vi.fn();
    let registered: ((e: { payload: unknown }) => void) | undefined;
    listenMock.mockImplementation((_event: string, cb: (e: { payload: unknown }) => void) => {
      registered = cb;
      return Promise.resolve(unlisten);
    });
    const t = createTauriTransport();
    const received: unknown[] = [];
    const off = await t.subscribe<{ kind: string }>("prs:updated", (p) => received.push(p));

    expect(listenMock).toHaveBeenCalledWith("prs:updated", expect.any(Function));
    // Backend pushes a Tauri Event envelope; the handler must see only the payload.
    registered?.({ payload: { kind: "updated" } });
    expect(received).toEqual([{ kind: "updated" }]);

    off();
    expect(unlisten).toHaveBeenCalledOnce();
  });

  it("openExternal delegates to openUrl", async () => {
    openUrlMock.mockResolvedValue(undefined);
    const t = createTauriTransport();
    await t.openExternal("https://example.test/pr/1");
    expect(openUrlMock).toHaveBeenCalledWith("https://example.test/pr/1");
  });
});

describe("HttpTransport", () => {
  it("request POSTs JSON to <baseUrl>/invoke/<command> and returns parsed body", async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValue({ ok: true, text: async () => JSON.stringify(["pr1"]) });
    vi.stubGlobal("fetch", fetchMock);

    const t = createHttpTransport("http://127.0.0.1:8787");
    const out = await t.request<string[]>("get_prs", { projectId: "p1" });

    expect(fetchMock).toHaveBeenCalledWith(
      "http://127.0.0.1:8787/invoke/get_prs",
      expect.objectContaining({
        method: "POST",
        headers: expect.objectContaining({ "content-type": "application/json" }),
        body: JSON.stringify({ projectId: "p1" }),
      }),
    );
    expect(out).toEqual(["pr1"]);
  });

  it("request sends {} body when args are omitted (undefined keys dropped, parity with invoke)", async () => {
    const fetchMock = vi.fn().mockResolvedValue({ ok: true, text: async () => "" });
    vi.stubGlobal("fetch", fetchMock);
    const t = createHttpTransport("http://x");
    await t.request<void>("start_polling");
    expect(fetchMock.mock.calls[0][1].body).toBe(JSON.stringify({}));
  });

  it("request forwards keepalive for pagehide cleanup calls", async () => {
    const fetchMock = vi.fn().mockResolvedValue({ ok: true, text: async () => "" });
    vi.stubGlobal("fetch", fetchMock);
    const t = createHttpTransport("http://x");

    await t.request<void>("detach_terminal", { sessionId: "s1" }, { keepalive: true });

    expect(fetchMock.mock.calls[0][1].keepalive).toBe(true);
  });

  it("request resolves undefined for a void command (empty/204 body, no JSON.parse throw)", async () => {
    // A void command (startPolling/stopReview/...) returns no body; res.json() would throw
    // on empty input, so request reads text first and returns undefined when it is empty.
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: true, text: async () => "" }));
    const t = createHttpTransport("http://x");
    await expect(t.request<void>("stop_polling")).resolves.toBeUndefined();
  });

  it("request rejects on a non-ok response", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: false, status: 500, statusText: "Internal Error" }),
    );
    const t = createHttpTransport("http://x");
    await expect(t.request("get_prs")).rejects.toThrow(/500/);
  });

  it("request surfaces backend JSON error messages", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 403,
        statusText: "Forbidden",
        text: async () => JSON.stringify({ message: "权限不足" }),
      }),
    );
    const t = createHttpTransport("http://x");
    await expect(t.request("send_terminal_input")).rejects.toThrow(/权限不足/);
  });

  it("request sends the session bearer token when present", async () => {
    sessionStorage.setItem("prmonitor.remoteBearerToken", "tok-123");
    const fetchMock = vi.fn().mockResolvedValue({ ok: true, text: async () => "" });
    vi.stubGlobal("fetch", fetchMock);
    const t = createHttpTransport("http://x");

    await t.request<void>("list_terminal_sessions");

    expect(fetchMock).toHaveBeenCalledWith(
      "http://x/invoke/list_terminal_sessions",
      expect.objectContaining({
        headers: expect.objectContaining({ authorization: "Bearer tok-123" }),
      }),
    );
  });

  it("request retries once with a refreshed bearer token after 401", async () => {
    sessionStorage.setItem("prmonitor.remoteBearerToken", "bad-token");
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce({
        ok: false,
        status: 401,
        statusText: "Unauthorized",
        text: async () => JSON.stringify({ message: "未授权" }),
      })
      .mockResolvedValueOnce({ ok: true, text: async () => JSON.stringify(["s1"]) });
    vi.stubGlobal("fetch", fetchMock);
    const t = createHttpTransport("http://x", {
      onAuthRejected: () => {
        sessionStorage.setItem("prmonitor.remoteBearerToken", "good-token");
        return true;
      },
    });

    await expect(t.request<string[]>("list_terminal_sessions")).resolves.toEqual(["s1"]);

    expect(fetchMock.mock.calls[0][1].headers.authorization).toBe("Bearer bad-token");
    expect(fetchMock.mock.calls[1][1].headers.authorization).toBe("Bearer good-token");
  });

  // --- HttpTransport.subscribe lifecycle: fetch-based SSE so Authorization can be sent. --------
  function sseResponse(lines: string[], close = false) {
    let cancelled = false;
    const encoder = new TextEncoder();
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        for (const line of lines) controller.enqueue(encoder.encode(line));
        if (close) controller.close();
      },
      cancel() {
        cancelled = true;
      },
    });
    return {
      ok: true,
      body: stream,
      cancelled: () => cancelled,
    };
  }

  it("subscribe fetches an SSE stream with bearer auth, delivers parsed payloads, and unlisten cancels it", async () => {
    sessionStorage.setItem("prmonitor.remoteBearerToken", "tok-123");
    const res = sseResponse(['data: {"kind":"updated"}\n\n']);
    const fetchMock = vi.fn().mockResolvedValue(res);
    vi.stubGlobal("fetch", fetchMock);
    const t = createHttpTransport("http://x");
    const received: unknown[] = [];

    const off = await t.subscribe<{ kind: string }>("prs:updated", (p) => received.push(p));
    await Promise.resolve();

    expect(fetchMock).toHaveBeenCalledWith(
      "http://x/events?topic=prs%3Aupdated",
      expect.objectContaining({
        headers: expect.objectContaining({
          accept: "text/event-stream",
          authorization: "Bearer tok-123",
        }),
      }),
    );
    expect(received).toEqual([{ kind: "updated" }]);

    off();
    await Promise.resolve();
    expect(res.cancelled()).toBe(true);
  });

  it("subscribe rejects on an initial stream error", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 401,
        statusText: "Unauthorized",
        text: async () => JSON.stringify({ message: "未授权" }),
      }),
    );
    const t = createHttpTransport("http://x");
    await expect(t.subscribe("prs:updated", () => {})).rejects.toThrow(/未授权/);
  });

  it("subscribe retries once with a refreshed bearer token after 401", async () => {
    sessionStorage.setItem("prmonitor.remoteBearerToken", "bad-token");
    const res = sseResponse(['data: {"kind":"updated"}\n\n']);
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce({
        ok: false,
        status: 401,
        statusText: "Unauthorized",
        text: async () => JSON.stringify({ message: "未授权" }),
      })
      .mockResolvedValueOnce(res);
    vi.stubGlobal("fetch", fetchMock);
    const t = createHttpTransport("http://x", {
      onAuthRejected: () => {
        sessionStorage.setItem("prmonitor.remoteBearerToken", "good-token");
        return true;
      },
    });

    const received: unknown[] = [];
    await t.subscribe("terminal:event", (p) => received.push(p));
    await Promise.resolve();

    expect(fetchMock.mock.calls[0][1].headers.authorization).toBe("Bearer bad-token");
    expect(fetchMock.mock.calls[1][1].headers.authorization).toBe("Bearer good-token");
    expect(received).toEqual([{ kind: "updated" }]);
  });

  it("subscribe swallows a malformed SSE frame (parse error → logged, handler not called)", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(sseResponse(["data: <html>not json</html>\n\n"])));
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const t = createHttpTransport("http://x");
    const received: unknown[] = [];
    await t.subscribe("prs:updated", (p) => received.push(p));
    await Promise.resolve();

    expect(received).toEqual([]);
    expect(errSpy).toHaveBeenCalled();
    errSpy.mockRestore();
  });

  it("a handler that throws on a VALID frame is NOT swallowed as a malformed frame", async () => {
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const thrown: unknown[] = [];
    const timeoutSpy = vi
      .spyOn(globalThis, "setTimeout")
      .mockImplementation(((cb: () => void) => {
        try {
          cb();
        } catch (err) {
          thrown.push(err);
        }
        return 0 as unknown as ReturnType<typeof setTimeout>;
      }) as typeof setTimeout);
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(sseResponse(['data: {"kind":"x"}\n\n'])));
    const t = createHttpTransport("http://x");
    await t.subscribe("prs:updated", () => {
      throw new Error("handler boom");
    });
    await Promise.resolve();
    expect(errSpy).not.toHaveBeenCalled();
    expect((thrown[0] as Error).message).toBe("handler boom");
    timeoutSpy.mockRestore();
    errSpy.mockRestore();
  });

  it("subscribe reports a clean EOF through onClosed", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(sseResponse(['data: {"kind":"x"}\n\n'], true)));
    const t = createHttpTransport("http://x");
    const closed: string[] = [];

    await t.subscribe("terminal:event", () => {}, { onClosed: (msg) => closed.push(msg) });
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(closed[0]).toContain("stream closed");
  });

  it("openExternal opens a new window with noopener,noreferrer", async () => {
    const openMock = vi.fn();
    vi.stubGlobal("window", { open: openMock });
    const t = createHttpTransport("http://x");
    await t.openExternal("https://example.test");
    expect(openMock).toHaveBeenCalledWith("https://example.test", "_blank", "noopener,noreferrer");
  });
});

describe("transport singleton (index.ts)", () => {
  it("getTransport() throws before setTransport() is called (fail-fast, not silent no-op)", async () => {
    vi.resetModules(); // fresh module → `current` is null again
    const mod = await import("./index");
    expect(() => mod.getTransport()).toThrow(/not initialized/);
  });

  it("getTransport() returns the instance registered by setTransport()", async () => {
    vi.resetModules();
    const mod = await import("./index");
    const fake = {
      request: vi.fn(),
      subscribe: vi.fn(),
      openExternal: vi.fn(),
    } as unknown as Transport;
    mod.setTransport(fake);
    expect(mod.getTransport()).toBe(fake);
  });
});
