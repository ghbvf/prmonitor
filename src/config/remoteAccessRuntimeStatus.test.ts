import { beforeEach, describe, expect, it, vi } from "vitest";
import { nextTick, ref } from "vue";
import type { RemoteAccessRuntimeStatus } from "./types";

const { requestMock } = vi.hoisted(() => ({ requestMock: vi.fn() }));
vi.mock("../transport", () => ({
  getTransport: () => ({
    request: requestMock,
    subscribe: vi.fn(),
    openExternal: vi.fn(),
  }),
}));

import { getRemoteAccessRuntimeStatus, notificationTestSend } from "./api";
import { DEFAULT_NOTIFICATION_CHANNEL } from "./defaults";

describe("getRemoteAccessRuntimeStatus (api wrapper)", () => {
  beforeEach(() => {
    vi.resetAllMocks();
  });

  it("calls request with the correct Tauri command name", async () => {
    const rows: RemoteAccessRuntimeStatus = { entrypoints: [], tunnels: [] };
    requestMock.mockResolvedValue(rows);

    await getRemoteAccessRuntimeStatus();

    expect(requestMock).toHaveBeenCalledWith("get_remote_access_runtime_status");
  });

  it("resolves to the entrypoint and tunnel status returned by request", async () => {
    const rows: RemoteAccessRuntimeStatus = {
      entrypoints: [
        {
          id: "entry-main",
          bound: true,
          boundPort: 8788,
          state: "bound",
          message: "bound on 127.0.0.1:8788",
          routes: [{ id: "local-api", path: "/api", capability: "local-api", enabled: true }],
        },
      ],
      tunnels: [
        {
          id: "lan-main",
          mode: "lan",
          targetEntrypointId: "entry-main",
          state: "running",
          publicUrl: "http://0.0.0.0:18788",
          message: "lan tunnel bound",
          logs: ["bound"],
        },
      ],
    };
    requestMock.mockResolvedValue(rows);

    const result = await getRemoteAccessRuntimeStatus();

    expect(result.entrypoints[0].routes[0].capability).toBe("local-api");
    expect(result.tunnels[0].mode).toBe("lan");
    expect(result.tunnels[0].targetEntrypointId).toBe("entry-main");
  });
});

describe("notificationTestSend (api wrapper)", () => {
  beforeEach(() => {
    vi.resetAllMocks();
  });

  it("calls request with the draft channel payload", async () => {
    requestMock.mockResolvedValue("ok");
    const channel = {
      ...DEFAULT_NOTIFICATION_CHANNEL,
      id: "slack-main",
      name: "Slack",
      kind: "slack" as const,
      enabled: true,
      webhookUrl: "https://example.com/hook",
    };

    await expect(notificationTestSend(channel)).resolves.toBe("ok");

    expect(requestMock).toHaveBeenCalledWith("notification_test_send", { channel });
  });
});

describe("RemoteAccessRuntimeStatus refreshKey watch pattern", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    requestMock.mockResolvedValue({ entrypoints: [], tunnels: [] });
  });

  it("watch with immediate:true fires on setup and on change", async () => {
    const { watch } = await import("vue");
    const refreshKey = ref(0);
    const callLog: number[] = [];

    function mockRefresh() {
      callLog.push(refreshKey.value);
      void getRemoteAccessRuntimeStatus();
    }

    const stop = watch(() => refreshKey.value, mockRefresh, { immediate: true });

    expect(callLog).toEqual([0]);
    expect(requestMock).toHaveBeenCalledTimes(1);

    refreshKey.value = 1;
    await nextTick();
    expect(callLog).toEqual([0, 1]);
    expect(requestMock).toHaveBeenCalledTimes(2);

    stop();
  });

  it("watch with immediate:true does not re-fire when refreshKey stays the same", async () => {
    const { watch } = await import("vue");
    const refreshKey = ref(5);
    const callLog: number[] = [];

    const stop = watch(() => refreshKey.value, () => callLog.push(refreshKey.value), {
      immediate: true,
    });

    refreshKey.value = 5;
    await nextTick();

    expect(callLog).toEqual([5]);
    stop();
  });
});
