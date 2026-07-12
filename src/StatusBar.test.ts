// @vitest-environment happy-dom

import { flushPromises, mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { nextTick } from "vue";
import { DEFAULT_CLI_TOOLS_CONFIG, NEW_PROJECT_DEFAULTS } from "./config/defaults";
import type { AppConfig } from "./config/types";
import { useConfigStore } from "./config/useConfigStore";
import StatusBar from "./StatusBar.vue";

const probes = vi.hoisted(() => ({
  refreshCodexStatus: vi.fn(),
  refreshClaudeStatus: vi.fn(),
  refreshCursorStatus: vi.fn(),
  startCodexServer: vi.fn(),
  stopCodexServer: vi.fn(),
  startCursorServer: vi.fn(),
  stopCursorServer: vi.fn(),
}));

vi.mock("./review/useReviewStore", async () => {
  const { ref } = await import("vue");
  const codex = ref({ available: true, desiredRunning: true, message: "codex ok" });
  const claude = ref({ available: true, message: "claude ok" });
  const cursor = ref({ available: true, desiredRunning: true, message: "cursor ok" });
  return { useReviewStore: () => ({ codex, claude, cursor, ...probes }) };
});

function config(): AppConfig {
  return {
    cliTools: { ...DEFAULT_CLI_TOOLS_CONFIG },
    projects: [
      {
        id: "codex-project",
        name: "Codex",
        ...NEW_PROJECT_DEFAULTS,
        repo: "owner/repo",
        repoRoot: "/repo",
        engineKind: "codex",
      },
      {
        id: "claude-project",
        name: "Claude",
        ...NEW_PROJECT_DEFAULTS,
        repo: "owner/other",
        repoRoot: "/other",
        engineKind: "claude",
      },
      {
        id: "cursor-project",
        name: "Cursor",
        ...NEW_PROJECT_DEFAULTS,
        repo: "owner/cursor",
        repoRoot: "/cursor",
        engineKind: "cursor",
      },
    ],
  } as AppConfig;
}

beforeEach(() => {
  setActivePinia(createPinia());
  Object.values(probes).forEach((mock) => mock.mockReset());
});

describe("StatusBar CLI path invalidation", () => {
  it("re-probes each used engine when its configured path changes", async () => {
    const configStore = useConfigStore();
    configStore.config = config();
    mount(StatusBar);
    await flushPromises();
    expect(probes.refreshCodexStatus).not.toHaveBeenCalled();
    expect(probes.refreshClaudeStatus).not.toHaveBeenCalled();
    expect(probes.refreshCursorStatus).not.toHaveBeenCalled();

    configStore.config.cliTools.codexPath = "/new/codex";
    await nextTick();
    expect(probes.refreshCodexStatus).toHaveBeenCalledOnce();
    expect(probes.stopCodexServer).not.toHaveBeenCalled();

    configStore.config.cliTools.claudePath = "/new/claude";
    await nextTick();
    expect(probes.refreshClaudeStatus).toHaveBeenCalledOnce();

    configStore.config.cliTools.agentPath = "/new/agent";
    await nextTick();
    expect(probes.refreshCursorStatus).toHaveBeenCalledOnce();
    expect(probes.stopCursorServer).not.toHaveBeenCalled();
  });

  it("cursor engine button toggles start/stop via onEngineButton", async () => {
    const { useReviewStore } = await import("./review/useReviewStore");
    const store = useReviewStore() as {
      cursor: { value: { available: boolean; desiredRunning: boolean; message: string } };
      codex: { value: { available: boolean; desiredRunning: boolean; message: string } };
    };
    // Only cursor is running so the sole 「停止」 button is cursor's.
    store.codex.value = {
      available: false,
      desiredRunning: false,
      message: "codex app-server 已停止",
    };
    const configStore = useConfigStore();
    configStore.config = config();
    const wrapper = mount(StatusBar);
    await flushPromises();

    const cursorItem = wrapper
      .findAll(".item")
      .find((el) => el.text().includes("cursor —"));
    expect(cursorItem).toBeTruthy();
    const stopBtn = cursorItem!.find("button");
    expect(stopBtn.text()).toBe("停止");
    await stopBtn.trigger("click");
    expect(probes.stopCursorServer).toHaveBeenCalledOnce();

    store.cursor.value = {
      available: false,
      desiredRunning: false,
      message: "cursor ACP 已停止",
    };
    await nextTick();
    const startBtn = cursorItem!.find("button");
    expect(startBtn.text()).toBe("启动");
    await startBtn.trigger("click");
    expect(probes.startCursorServer).toHaveBeenCalledOnce();
  });
});
