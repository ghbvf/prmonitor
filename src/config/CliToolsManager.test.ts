// @vitest-environment happy-dom

import { flushPromises, mount } from "@vue/test-utils";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DEFAULT_CLI_TOOLS_CONFIG } from "./types.generated";
import type { AppConfig, CliToolProbeStatus } from "./types";
import CliToolsManager from "./CliToolsManager.vue";

const { probeCliTools } = vi.hoisted(() => ({ probeCliTools: vi.fn() }));
vi.mock("./api", () => ({ probeCliTools }));

function draft(): AppConfig {
  return { cliTools: { ...DEFAULT_CLI_TOOLS_CONFIG } } as AppConfig;
}

function status(
  tool: CliToolProbeStatus["tool"],
  resolvedPath = `/usr/local/bin/${tool}`,
  pendingRestart = false,
): CliToolProbeStatus {
  return {
    tool,
    configuredPath: "",
    resolvedPath,
    source: "process-path",
    available: true,
    pendingRestart,
    message: "已找到",
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((onResolve, onReject) => {
    resolve = onResolve;
    reject = onReject;
  });
  return { promise, resolve, reject };
}

beforeEach(() => {
  probeCliTools.mockReset();
  vi.stubEnv("TAURI_ENV_PLATFORM", "macos");
});

afterEach(() => vi.unstubAllEnvs());

describe("CliToolsManager", () => {
  it("edits only Codex/Claude config directories and passes them to probing", async () => {
    probeCliTools.mockResolvedValue([]);
    const config = draft();
    const wrapper = mount(CliToolsManager, { props: { draft: config } });
    await flushPromises();
    expect(wrapper.find("#cli-config-dir-agent").exists()).toBe(false);
    await wrapper.get("#cli-config-dir-codex").setValue("/accounts/工作 codex");
    await wrapper.get("#cli-config-dir-claude").setValue("/accounts/claude");
    expect(wrapper.text()).toContain("当前探测结果已过期");
    await wrapper.get("button").trigger("click");
    expect(probeCliTools).toHaveBeenLastCalledWith({
      ...DEFAULT_CLI_TOOLS_CONFIG,
      codexHome: "/accounts/工作 codex",
      claudeConfigDir: "/accounts/claude",
    }, true);
    expect(wrapper.emitted("edit")).toHaveLength(2);
  });

  it("keeps config directory errors local and invalidates obsolete row failures", async () => {
    probeCliTools.mockResolvedValue([{ ...status("codex"), available: false, message: "CLI 配置目录 CODEX_HOME 必须为已存在的目录绝对路径" }]);
    const config = draft();
    config.cliTools.codexHome = "/missing";
    const wrapper = mount(CliToolsManager, { props: { draft: config } });
    await flushPromises();
    expect(wrapper.get("#cli-config-dir-codex").attributes("aria-invalid")).toBe("true");
    await wrapper.get("#cli-config-dir-codex").setValue("/accounts/valid");
    expect(wrapper.get("#cli-config-dir-codex").attributes("aria-invalid")).toBe("false");
    await wrapper.get("#cli-config-dir-claude").setValue("relative");
    expect(wrapper.text()).toContain("配置目录请输入当前系统的绝对路径");
    await wrapper.get("button").trigger("click");
    expect(probeCliTools).toHaveBeenLastCalledWith(expect.objectContaining({ codexHome: "/accounts/valid", claudeConfigDir: "relative" }), true);
  });

  it("keeps a relative-path error local while probing the other tools", async () => {
    probeCliTools.mockResolvedValue([]);
    const config = draft();
    const wrapper = mount(CliToolsManager, { props: { draft: config } });
    await flushPromises();

    expect(wrapper.findAll(".tool-row")).toHaveLength(6);
    await wrapper.get("#cli-path-gh").setValue("bin/gh");
    await wrapper.get("button").trigger("click");

    expect(config.cliTools.ghPath).toBe("bin/gh");
    expect(wrapper.text()).toContain("请输入当前系统的绝对路径");
    expect(probeCliTools).toHaveBeenCalledTimes(2);
    expect(probeCliTools).toHaveBeenLastCalledWith(
      { ...DEFAULT_CLI_TOOLS_CONFIG, ghPath: "" },
      true,
    );
    expect(wrapper.emitted("edit")).toHaveLength(1);
  });

  it("shows pending restart and marks results stale after an absolute-path edit", async () => {
    probeCliTools.mockResolvedValue([status("codex", "/opt/bin/codex", true)]);
    const wrapper = mount(CliToolsManager, { props: { draft: draft() } });
    await flushPromises();

    expect(wrapper.text()).toContain("实际路径：/opt/bin/codex");
    expect(wrapper.text()).toContain("新路径或配置目录将在相关进程重启后生效");
    await wrapper.get("#cli-path-codex").setValue("/custom/bin/codex");
    expect(wrapper.text()).toContain("当前探测结果已过期");
  });

  it("keeps the latest probe result when responses arrive out of order", async () => {
    const first = deferred<CliToolProbeStatus[]>();
    const second = deferred<CliToolProbeStatus[]>();
    probeCliTools.mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise);
    const wrapper = mount(CliToolsManager, { props: { draft: draft() } });

    await wrapper.get("button").trigger("click");
    second.resolve([status("gh", "/new/bin/gh")]);
    await flushPromises();
    expect(wrapper.text()).toContain("实际路径：/new/bin/gh");

    first.resolve([status("gh", "/old/bin/gh")]);
    await flushPromises();
    expect(wrapper.text()).toContain("实际路径：/new/bin/gh");
    expect(wrapper.text()).not.toContain("/old/bin/gh");
  });

  it("surfaces an unavailable probe result", async () => {
    probeCliTools.mockResolvedValue([
      {
        ...status("claude"),
        resolvedPath: null,
        source: null,
        available: false,
        message: "探测失败",
      },
    ]);
    const wrapper = mount(CliToolsManager, { props: { draft: draft() } });
    await flushPromises();

    expect(wrapper.text()).toContain("探测失败");
  });

  it("marks an unavailable custom path invalid without showing restart-success text", async () => {
    const config = draft();
    config.cliTools.codexPath = "/missing/codex";
    probeCliTools.mockResolvedValue([
      {
        ...status("codex"),
        configuredPath: "/missing/codex",
        resolvedPath: null,
        source: null,
        available: false,
        pendingRestart: true,
        message: "路径不可用；旧常驻进程仍在运行，重启后将无法启动",
      },
    ]);
    const wrapper = mount(CliToolsManager, { props: { draft: config } });
    await flushPromises();

    expect(wrapper.get("#cli-path-codex").attributes("aria-invalid")).toBe("true");
    expect(wrapper.text()).toContain("旧常驻进程仍在运行");
    expect(wrapper.text()).not.toContain("新路径或配置目录将在相关进程重启后生效");

    await wrapper.get("#cli-path-gh").setValue("/custom/gh");
    expect(
      wrapper.get("#cli-path-codex").attributes("aria-invalid"),
      "editing a sibling row must not clear this row's known invalid state",
    ).toBe("true");
  });
});
