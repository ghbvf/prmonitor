// useConfigStore state-machine tests (#34). Drives the store against a mocked
// `./api` so assertions stay deterministic. Mirrors the pr/review store test
// pattern. Backs the onboarding gate (wizard @done fires only on savedOk) and the
// Settings save path.
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import type { AppConfig } from "./types";
import {
  DEFAULT_CLI_TOOLS_CONFIG,
  DEFAULT_REVIEW_LIFECYCLE_NOTIFICATION_CONFIG,
} from "./defaults";

vi.mock("./api", () => ({
  getConfig: vi.fn(),
  setConfig: vi.fn(() => Promise.resolve()),
}));

import * as api from "./api";
import { useConfigStore } from "./useConfigStore";

// Multi-project AppConfig (#35): one project plus the global webhook fields. Tests
// perturb `projects` / global fields via `over` to exercise the store's load/save.
const cfg = (over: Partial<AppConfig> = {}): AppConfig => ({
  projects: [
    {
      id: "default",
      name: "默认项目",
      enabled: true,
      repo: "ghbvf/gocell",
      repoRoot: "/abs/path",
      pollIntervalSecs: 120,
      authors: [],
      labelSource: "native",
      skillRelPath: ".codex/skills/pr-review/SKILL.md",
      prCooldownSeconds: 1800,
      updateMode: "webhook-only",
      sourceKind: "github",
      azureOrg: "",
      azureProject: "",
      bitbucketHost: "",
      bitbucketProject: "",
      bitbucketToken: "",
      engineKind: "codex",
      codexModel: "",
      claudeModel: "",
      cursorModel: "",
      codexReasoningEffort: "default",
      claudeEffort: "default",
    },
  ],
  activeProjectId: "default",
  webhookEnabled: false,
  webhookPort: 8787,
  webhookSecret: "",
  cliTools: { ...DEFAULT_CLI_TOOLS_CONFIG },
  webhookTunnelMode: "quick",
  webhookTunnelCommand: "",
  webhookPublicUrl: "",
  localApiToken: "",
  outbox: { notificationTtlSecs: 7200 },
  notifications: { channels: [] },
  messaging: { integrations: [] },
  reviewLifecycleNotifications: DEFAULT_REVIEW_LIFECYCLE_NOTIFICATION_CONFIG,
  remoteAccess: { entrypoints: [], tunnels: [] },
  rules: [],
  ...over,
});

beforeEach(() => {
  setActivePinia(createPinia());
  vi.clearAllMocks();
  vi.mocked(api.setConfig).mockResolvedValue(undefined);
});

describe("useConfigStore load()", () => {
  it("populates config and clears error on success", async () => {
    const loaded = cfg();
    vi.mocked(api.getConfig).mockResolvedValueOnce(loaded);
    const store = useConfigStore();

    await store.load();

    expect(api.getConfig).toHaveBeenCalledOnce();
    expect(store.config).toEqual(loaded);
    expect(store.error).toBeNull();
    expect(store.loading).toBe(false);
  });

  it("surfaces a rejected getConfig as error", async () => {
    vi.mocked(api.getConfig).mockRejectedValueOnce({ message: "load boom" });
    const store = useConfigStore();

    await store.load();

    expect(store.error).toBe("load boom");
    expect(store.config).toBeNull();
    expect(store.loading).toBe(false);
  });
});

describe("useConfigStore save()", () => {
  it("on success stores the new config and sets savedOk", async () => {
    const next = cfg({ activeProjectId: "default", webhookPort: 9999 });
    const store = useConfigStore();

    await store.save(next);

    expect(api.setConfig).toHaveBeenCalledWith(next);
    expect(store.config).toEqual(next);
    expect(store.savedOk).toBe(true);
    expect(store.error).toBeNull();
    expect(store.saving).toBe(false);
  });

  it("round-trips all custom CLI paths without dropping the nested structure", async () => {
    const cliTools = {
      ...DEFAULT_CLI_TOOLS_CONFIG,
      ghPath: "/opt/homebrew/bin/gh",
      claudePath: "C:\\Tools\\claude.exe",
    };
    const next = cfg({ cliTools });
    const store = useConfigStore();

    await store.save(next);

    expect(api.setConfig).toHaveBeenCalledWith(expect.objectContaining({ cliTools }));
    expect(store.config?.cliTools).toEqual(cliTools);
    expect("cloudflaredBin" in store.config!).toBe(false);
  });

  it("on a rejected invoke sets error and leaves savedOk false", async () => {
    vi.mocked(api.setConfig).mockRejectedValueOnce({
      message: "repoRoot 必须是存在的绝对目录路径: ",
    });
    const store = useConfigStore();

    await store.save(cfg());

    expect(store.error).toBe("repoRoot 必须是存在的绝对目录路径: ");
    expect(store.savedOk).toBe(false);
    expect(store.saving).toBe(false);
  });
});
