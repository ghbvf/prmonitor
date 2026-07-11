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
  startCodexServer: vi.fn(),
  stopCodexServer: vi.fn(),
}));

vi.mock("./review/useReviewStore", async () => {
  const { ref } = await import("vue");
  const codex = ref({ available: true, desiredRunning: true, message: "codex ok" });
  const claude = ref({ available: true, message: "claude ok" });
  return { useReviewStore: () => ({ codex, claude, ...probes }) };
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

    configStore.config.cliTools.codexPath = "/new/codex";
    await nextTick();
    expect(probes.refreshCodexStatus).toHaveBeenCalledOnce();
    expect(probes.stopCodexServer).not.toHaveBeenCalled();

    configStore.config.cliTools.claudePath = "/new/claude";
    await nextTick();
    expect(probes.refreshClaudeStatus).toHaveBeenCalledOnce();
  });
});
