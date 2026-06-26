// terminal api adapter tests (#1383). Drives the slice api against a FAKE Transport
// (setTransport) and asserts each fn calls request/subscribe with the EXACT backend
// command name + camelCase args — the wire contract the Rust commands deserialize. A
// renamed command or a snake_cased arg key here would fail the suite.
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Transport } from "../transport";
import { setTransport } from "../transport";
import * as api from "./api";

const request = vi.fn();
const subscribe = vi.fn();
// Register the fake once at module load (vitest isolates module state per test file, so
// this singleton write doesn't leak to other suites); reset call history per test below.
setTransport({ request, subscribe, openExternal: vi.fn() } as unknown as Transport);

beforeEach(() => {
  vi.clearAllMocks();
  request.mockResolvedValue(undefined);
  subscribe.mockResolvedValue(() => {});
});

describe("terminal api commands", () => {
  it("listTerminalSessions invokes list_terminal_sessions (no args)", async () => {
    await api.listTerminalSessions();
    expect(request).toHaveBeenCalledWith("list_terminal_sessions");
  });

  it("createTerminalSession defaults to an empty opts object", async () => {
    await api.createTerminalSession();
    expect(request).toHaveBeenCalledWith("create_terminal_session", { opts: {} });
  });

  it("createTerminalSession forwards the opts (camelCase) when given", async () => {
    await api.createTerminalSession({ windowId: "w1", profile: "Default" });
    expect(request).toHaveBeenCalledWith("create_terminal_session", {
      opts: { windowId: "w1", profile: "Default" },
    });
  });

  it("attachTerminal passes sessionId (camelCase)", async () => {
    await api.attachTerminal("s1");
    expect(request).toHaveBeenCalledWith("attach_terminal", { sessionId: "s1" });
  });

  it("detachTerminal passes sessionId (camelCase)", async () => {
    await api.detachTerminal("s1");
    expect(request).toHaveBeenCalledWith("detach_terminal", { sessionId: "s1" });
  });

  it("sendTerminalInput passes sessionId + data (camelCase)", async () => {
    await api.sendTerminalInput("s1", "ls\r");
    expect(request).toHaveBeenCalledWith("send_terminal_input", {
      sessionId: "s1",
      data: "ls\r",
    });
  });

  it("resizeTerminal passes sessionId + cols + rows (camelCase)", async () => {
    await api.resizeTerminal("s1", 100, 40);
    expect(request).toHaveBeenCalledWith("resize_terminal", {
      sessionId: "s1",
      cols: 100,
      rows: 40,
    });
  });

  it("getTerminalStatus invokes get_terminal_status (no args)", async () => {
    await api.getTerminalStatus();
    expect(request).toHaveBeenCalledWith("get_terminal_status");
  });

  it("stopTerminalDaemon invokes stop_terminal_daemon and returns the daemon status", async () => {
    const status = { available: true, desiredRunning: false, message: "stopped" };
    request.mockResolvedValueOnce(status);
    const out = await api.stopTerminalDaemon();
    expect(request).toHaveBeenCalledWith("stop_terminal_daemon");
    // Rust `stop_terminal_daemon` returns AppResult<TerminalDaemonStatus> — the wrapper must
    // propagate it (previously typed Promise<void>, which dropped the status).
    expect(out).toEqual(status);
  });
});

describe("onTerminalEvent", () => {
  it("subscribes to the terminal:event channel with the handler", async () => {
    const cb = vi.fn();
    await api.onTerminalEvent(cb);
    expect(subscribe).toHaveBeenCalledWith("terminal:event", cb);
  });

  it("returns the unlisten fn the transport yields", async () => {
    const unlisten = vi.fn();
    subscribe.mockResolvedValueOnce(unlisten);
    const out = await api.onTerminalEvent(vi.fn());
    expect(out).toBe(unlisten);
  });
});
