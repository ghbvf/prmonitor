import { describe, expect, it } from "vitest";
import { CLI_TOOLS } from "../types.generated";
import { DEFAULT_CLI_TOOLS_CONFIG } from "./types.generated";
import type { CliToolProbeStatus } from "./types";
import {
  CLI_RESOLUTION_SOURCE_LABELS,
  CLI_TOOL_META,
  cliProbeSnapshotMatches,
  cliToolsPathErrors,
  cloneCliTools,
  isCliToolsSaveError,
  probeStatusesByTool,
} from "./cliTools";

describe("CLI tool metadata", () => {
  it("covers every generated CLI exactly once with an exhaustive field mapping", () => {
    expect(Object.keys(CLI_TOOL_META).sort()).toEqual([...CLI_TOOLS].sort());
    expect(CLI_TOOLS.map((tool) => CLI_TOOL_META[tool].pathKey)).toEqual([
      "ghPath",
      "azPath",
      "codexPath",
      "claudePath",
      "agentPath",
      "cloudflaredPath",
    ]);
  });

  it("labels every generated resolution source", () => {
    expect(CLI_RESOLUTION_SOURCE_LABELS).toEqual({
      custom: "自定义路径",
      "process-path": "当前环境",
      "login-shell-path": "登录 Shell",
      "platform-fallback": "系统常用位置",
    });
  });

  it("uses the generated empty-path defaults and carries no legacy field", () => {
    expect(DEFAULT_CLI_TOOLS_CONFIG).toEqual({
      ghPath: "",
      azPath: "",
      codexPath: "",
      claudePath: "",
      agentPath: "",
      cloudflaredPath: "",
    });
    expect("cloudflaredBin" in DEFAULT_CLI_TOOLS_CONFIG).toBe(false);
    expect(cloneCliTools(DEFAULT_CLI_TOOLS_CONFIG)).not.toBe(DEFAULT_CLI_TOOLS_CONFIG);
  });
});

describe("CLI path validation", () => {
  it.each(["windows", "linux", "macos"])(
    "keeps empty paths as automatic discovery on %s",
    (platform) => {
      expect(cliToolsPathErrors(DEFAULT_CLI_TOOLS_CONFIG, platform)).toEqual({});
    },
  );

  it.each(["linux", "macos"])("uses Unix absolute-path semantics on %s", (platform) => {
    expect(
      cliToolsPathErrors(
        { ...DEFAULT_CLI_TOOLS_CONFIG, ghPath: "/opt/homebrew/bin/gh" },
        platform,
      ),
    ).toEqual({});
    expect(
      cliToolsPathErrors(
        { ...DEFAULT_CLI_TOOLS_CONFIG, ghPath: "C:\\Program Files\\GitHub CLI\\gh.exe" },
        platform,
      ),
    ).toEqual({ gh: "请输入当前系统的绝对路径；最终以后端校验为准。" });
  });

  it.each([
    "C:\\Program Files\\GitHub CLI\\gh.exe",
    "D:/tools/az.cmd",
    "\\\\server\\share\\codex.exe",
    "//server/share/codex.exe",
  ])("accepts Windows absolute path %j on Windows", (path) => {
    const config = { ...DEFAULT_CLI_TOOLS_CONFIG, ghPath: path };
    expect(cliToolsPathErrors(config, "windows")).toEqual({});
  });

  it("rejects a Unix-rooted path on Windows", () => {
    expect(
      cliToolsPathErrors(
        { ...DEFAULT_CLI_TOOLS_CONFIG, ghPath: "/usr/local/bin/gh" },
        "windows",
      ),
    ).toEqual({ gh: "请输入当前系统的绝对路径；最终以后端校验为准。" });
  });

  it.each(["windows", "linux", "macos"])(
    "rejects relative paths when the platform is known (%s)",
    (platform) => {
      for (const ghPath of ["gh", "bin/gh", "./gh", "../bin/gh", " Program Files/gh.exe"]) {
        expect(cliToolsPathErrors({ ...DEFAULT_CLI_TOOLS_CONFIG, ghPath }, platform)).toEqual({
          gh: "请输入当前系统的绝对路径；最终以后端校验为准。",
        });
      }
    },
  );

  it("does not hard-block saves when a non-Tauri browser build has no platform", () => {
    expect(
      cliToolsPathErrors(
        { ...DEFAULT_CLI_TOOLS_CONFIG, ghPath: "relative/gh" },
        undefined,
      ),
    ).toEqual({});
  });
});

describe("CLI save error routing", () => {
  it.each([
    "CLI 路径必须使用绝对路径",
    "cliTools 配置无效",
    "ghPath 文件名必须为 gh",
    "cloudflaredPath 不是存在且可执行的文件",
  ])("routes %j back to the CLI tools panel", (message) => {
    expect(isCliToolsSaveError(message)).toBe(true);
  });

  it("does not steal errors owned by another settings panel", () => {
    expect(isCliToolsSaveError("webhookPort 必须大于 0")).toBe(false);
  });
});

describe("CLI probe presentation state", () => {
  const status = (
    tool: CliToolProbeStatus["tool"],
    over: Partial<CliToolProbeStatus> = {},
  ): CliToolProbeStatus => ({
    tool,
    configuredPath: "",
    resolvedPath: `/usr/local/bin/${tool}`,
    source: "process-path",
    available: true,
    pendingRestart: false,
    message: "已找到",
    ...over,
  });

  it("indexes success, failure and pending-restart statuses without dropping tools", () => {
    const indexed = probeStatusesByTool([
      status("gh"),
      status("az", { available: false, resolvedPath: null, source: null, message: "未找到" }),
      status("codex", { pendingRestart: true }),
    ]);

    expect(indexed.gh?.available).toBe(true);
    expect(indexed.az).toMatchObject({ available: false, message: "未找到" });
    expect(indexed.codex?.pendingRestart).toBe(true);
    expect(indexed.claude).toBeUndefined();
  });

  it("marks probe data stale as soon as any draft path changes", () => {
    const probed = cloneCliTools(DEFAULT_CLI_TOOLS_CONFIG);
    expect(cliProbeSnapshotMatches(probed, cloneCliTools(probed))).toBe(true);
    expect(cliProbeSnapshotMatches(probed, { ...probed, ghPath: "/usr/local/bin/gh" })).toBe(false);
  });
});
