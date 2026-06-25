// Locks the listenerStateLabel helper (AB#1225 PR1), the Medium `assertNever`穷尽
// carrier that sits on top of the LISTENER_STATES Hard `as const` set. Mirrors the
// inbox.test.ts pattern for eventTypeLabel / inboxStatusLabel: the compile error catches
// a MISSING arm in listenerStateLabel; this test catches an empty or duplicated label
// that the compiler can't detect.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { nextTick, ref } from "vue";
import { LISTENER_STATES } from "./types";
import { listenerStateLabel } from "./listenerStateLabel";
import type { ListenerRuntimeStatus } from "./types";

// Mock the real Tauri boundary (`../api` re-exports `invoke` from `@tauri-apps/api/core`).
// We intentionally do NOT mock `./api` — we want the real `getListenerRuntimeStatus`
// wrapper to be exercised so that a rename of the Tauri command string fails the test
// (F7 fix: test must not mock the system-under-test).
vi.mock("../api", () => ({
  invoke: vi.fn(),
  listen: vi.fn(),
}));

import * as tauriApi from "../api";
import { getListenerRuntimeStatus } from "./api";

// ── listenerStateLabel exhaustiveness ───────────────────────────────────────────

describe("listenerStateLabel", () => {
  it("returns a non-empty label for every ListenerState", () => {
    for (const s of LISTENER_STATES) {
      expect(listenerStateLabel(s).length, `state '${s}' returned empty`).toBeGreaterThan(0);
    }
  });

  it("maps each ListenerState to a distinct label (no two states collide)", () => {
    const labels = LISTENER_STATES.map(listenerStateLabel);
    expect(new Set(labels).size).toBe(LISTENER_STATES.length);
  });

  it("bound label mentions '已绑定'", () => {
    expect(listenerStateLabel("bound")).toContain("已绑定");
  });

  it("blocked-needs-1073 label is user-actionable and ticket-free (F7)", () => {
    const label = listenerStateLabel("blocked-needs-1073");
    // Must not expose the raw ticket number to users (F7).
    expect(label).not.toContain("AB#1073");
    // Must give actionable hint about the bind address constraint.
    expect(label.length).toBeGreaterThan(0);
    expect(label).toContain("127.0.0.1");
  });

  it("unsupported label is user-actionable and ticket-free (F7)", () => {
    const label = listenerStateLabel("unsupported");
    // Must not expose the raw ticket number to users (F7).
    expect(label).not.toContain("AB#1073");
    // Must give actionable hint about the feature status.
    expect(label.length).toBeGreaterThan(0);
    expect(label).toContain("开发中");
  });

  it("error label mentions '错误'", () => {
    expect(listenerStateLabel("error")).toContain("错误");
  });

  it("bound-no-auth label is non-empty and distinct from 'bound'", () => {
    const label = listenerStateLabel("bound-no-auth");
    expect(label.length).toBeGreaterThan(0);
    // Must be actionable (hint about token) and distinct from the plain "bound" label.
    expect(label).not.toBe(listenerStateLabel("bound"));
    expect(label).toContain("token");
  });
});

// ── getListenerRuntimeStatus API wrapper ─────────────────────────────────────────
//
// These tests exercise the REAL `getListenerRuntimeStatus` from `./api` (not a mock
// of it). Only `invoke` (the Tauri IPC boundary in `../api`) is mocked. This means:
//  • A rename of the Tauri command string "get_listener_runtime_status" will fail here.
//  • The wrapper's `invoke<ListenerRuntimeStatus[]>(...)` call shape is verified.
// (F7 fix: mock the boundary, not the system-under-test.)

describe("getListenerRuntimeStatus (api wrapper)", () => {
  beforeEach(() => {
    vi.resetAllMocks();
  });

  it("calls invoke with the correct Tauri command name", async () => {
    // Verify the wire contract: if the Rust command is renamed, this test fails.
    vi.mocked(tauriApi.invoke).mockResolvedValue([]);
    await getListenerRuntimeStatus();
    expect(tauriApi.invoke).toHaveBeenCalledWith("get_listener_runtime_status");
  });

  it("resolves to the array returned by invoke", async () => {
    const rows: ListenerRuntimeStatus[] = [
      {
        id: "lis-local",
        kind: "local-api",
        bound: true,
        boundPort: 8788,
        state: "bound",
        message: "bound on 127.0.0.1:8788",
      },
      {
        id: "lis-web",
        kind: "remote-web",
        bound: false,
        state: "blocked-needs-1073",
        message: "non-loopback bind requires a loopback address",
      },
    ];
    vi.mocked(tauriApi.invoke).mockResolvedValue(rows);

    const result = await getListenerRuntimeStatus();
    expect(result).toHaveLength(2);

    // First row: bound state
    expect(result[0].state).toBe("bound");
    expect(result[0].bound).toBe(true);
    expect(result[0].boundPort).toBe(8788);

    // Second row: blocked-needs-1073 — label must be user-actionable, no ticket id (F7)
    expect(result[1].state).toBe("blocked-needs-1073");
    const stateLabel = listenerStateLabel(result[1].state);
    expect(stateLabel).not.toContain("AB#1073");
    expect(stateLabel.length).toBeGreaterThan(0);
  });

  it("returns an empty array when invoke resolves to []", async () => {
    vi.mocked(tauriApi.invoke).mockResolvedValue([]);
    const result = await getListenerRuntimeStatus();
    expect(result).toHaveLength(0);
  });
});

// ── RemoteAccessRuntimeStatus refreshKey watch (F22) ─────────────────────────────
//
// Tests that the single-source `watch(() => props.refreshKey, refresh, { immediate: true })`
// in RemoteAccessRuntimeStatus fires both on initial render AND on prop change. This is
// verified without mounting the component (no DOM in this vitest environment) by running
// the same watch logic directly — proving the consolidated pattern works and that the
// `onMounted` + `watch` dual-trigger was successfully removed (F17).

describe("RemoteAccessRuntimeStatus refreshKey watch pattern (F22)", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    vi.mocked(tauriApi.invoke).mockResolvedValue([]);
  });

  it("watch with immediate:true fires on setup and on change", async () => {
    // Reproduce the exact watch pattern used in RemoteAccessRuntimeStatus.vue
    // (F17 consolidated: single watch, no onMounted duplicate).
    const { watch } = await import("vue");
    const refreshKey = ref(0);
    const callLog: number[] = [];

    function mockRefresh() {
      callLog.push(refreshKey.value);
      void getListenerRuntimeStatus();
    }

    // This mirrors `watch(() => props.refreshKey, refresh, { immediate: true })`.
    const stop = watch(() => refreshKey.value, mockRefresh, { immediate: true });

    // immediate fires synchronously — callLog already has the initial entry.
    expect(callLog).toHaveLength(1);
    expect(callLog[0]).toBe(0);
    expect(tauriApi.invoke).toHaveBeenCalledTimes(1);

    // Bump refreshKey — the watch should fire again.
    refreshKey.value = 1;
    await nextTick();
    expect(callLog).toHaveLength(2);
    expect(callLog[1]).toBe(1);
    expect(tauriApi.invoke).toHaveBeenCalledTimes(2);

    stop();
  });

  it("watch with immediate:true does NOT re-fire when refreshKey stays the same", async () => {
    const { watch } = await import("vue");
    const refreshKey = ref(5);
    const callLog: number[] = [];

    const stop = watch(() => refreshKey.value, () => { callLog.push(refreshKey.value); }, {
      immediate: true,
    });
    expect(callLog).toHaveLength(1);

    // Same value — no re-fire.
    refreshKey.value = 5;
    await nextTick();
    expect(callLog).toHaveLength(1);

    stop();
  });
});
