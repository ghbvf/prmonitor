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

afterEach(() => {
  vi.clearAllMocks();
  vi.unstubAllGlobals();
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

  // --- HttpTransport.subscribe lifecycle (AB#1375): a FakeEventSource whose open/error/message
  //     we drive by hand. subscribe() now resolves on `onopen` (parity with Tauri listen()'s
  //     "registered" semantics), so each test fires `onopen` before awaiting. ------------------
  class FakeEventSource {
    static instances: FakeEventSource[] = [];
    url: string;
    onmessage: ((e: { data: string }) => void) | null = null;
    onopen: (() => void) | null = null;
    onerror: ((e: unknown) => void) | null = null;
    closed = false;
    constructor(url: string) {
      this.url = url;
      FakeEventSource.instances.push(this);
    }
    close() {
      this.closed = true;
    }
  }
  function stubEventSource(): FakeEventSource[] {
    FakeEventSource.instances = [];
    vi.stubGlobal("EventSource", FakeEventSource);
    return FakeEventSource.instances;
  }

  it("subscribe resolves only AFTER onopen, delivers parsed payloads, and unlisten closes it", async () => {
    const instances = stubEventSource();
    const t = createHttpTransport("http://x");
    const received: unknown[] = [];
    const subP = t.subscribe<{ kind: string }>("prs:updated", (p) => received.push(p));

    // The EventSource is created synchronously, but subscribe must NOT resolve until the stream
    // is open — otherwise a consumer's snapshot read could beat the subscription (#27 F3).
    expect(instances).toHaveLength(1);
    expect(instances[0].url).toContain("prs%3Aupdated");
    let resolved = false;
    void subP.then(() => {
      resolved = true;
    });
    await Promise.resolve();
    expect(resolved).toBe(false);

    instances[0].onopen?.(); // the server accepted the SSE stream
    const off = await subP;
    expect(resolved).toBe(true);

    instances[0].onmessage?.({ data: JSON.stringify({ kind: "updated" }) });
    expect(received).toEqual([{ kind: "updated" }]);

    off();
    expect(instances[0].closed).toBe(true);
  });

  it("subscribe rejects and closes on an initial (pre-open) stream error", async () => {
    const instances = stubEventSource();
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const t = createHttpTransport("http://x");
    const subP = t.subscribe("prs:updated", () => {});

    instances[0].onerror?.({}); // the connection failed before ever opening
    await expect(subP).rejects.toThrow(/failed to open/);
    // The dead stream is closed (no half-open EventSource leak); a pre-open error rejects rather
    // than logging as a recoverable blip.
    expect(instances[0].closed).toBe(true);
    errSpy.mockRestore();
  });

  it("a stream error AFTER open is logged, not fatal (EventSource auto-reconnects)", async () => {
    const instances = stubEventSource();
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const t = createHttpTransport("http://x");
    const subP = t.subscribe("prs:updated", () => {});

    instances[0].onopen?.();
    const off = await subP;
    instances[0].onerror?.({}); // a post-open blip
    expect(errSpy).toHaveBeenCalled();
    expect(instances[0].closed).toBe(false); // NOT torn down by the error
    off();
    expect(instances[0].closed).toBe(true);
    errSpy.mockRestore();
  });

  it("subscribe swallows a malformed SSE frame (parse error → logged, handler not called)", async () => {
    const instances = stubEventSource();
    const errSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const t = createHttpTransport("http://x");
    const received: unknown[] = [];
    const subP = t.subscribe("prs:updated", (p) => received.push(p));
    instances[0].onopen?.();
    await subP;

    // A non-JSON frame must NOT throw out of the native callback nor reach the handler.
    expect(() => instances[0].onmessage?.({ data: "<html>not json</html>" })).not.toThrow();
    expect(received).toEqual([]);
    expect(errSpy).toHaveBeenCalled();
    errSpy.mockRestore();
  });

  it("a handler that throws on a VALID frame is NOT swallowed as a malformed frame", async () => {
    const instances = stubEventSource();
    const t = createHttpTransport("http://x");
    const subP = t.subscribe("prs:updated", () => {
      throw new Error("handler boom");
    });
    instances[0].onopen?.();
    await subP;

    // The frame parses fine; the handler's own exception must surface (not be misattributed as a
    // dropped malformed frame and swallowed), so an `assertNever`-style runtime guard stays loud.
    expect(() => instances[0].onmessage?.({ data: JSON.stringify({ kind: "x" }) })).toThrow(
      "handler boom",
    );
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
