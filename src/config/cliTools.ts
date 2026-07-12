import { CLI_TOOLS } from "../types.generated";
import type { CliResolutionSource, CliTool } from "../types.generated";
import type { CliToolProbeStatus, CliToolsConfig } from "./types";

interface CliToolMeta {
  label: string;
  pathKey: keyof CliToolsConfig;
}

export const CLI_TOOL_META: Record<CliTool, CliToolMeta> = {
  gh: { label: "GitHub CLI (gh)", pathKey: "ghPath" },
  az: { label: "Azure CLI (az)", pathKey: "azPath" },
  codex: { label: "Codex CLI", pathKey: "codexPath" },
  claude: { label: "Claude CLI", pathKey: "claudePath" },
  agent: { label: "Cursor Agent (agent)", pathKey: "agentPath" },
  cloudflared: { label: "Cloudflare Tunnel (cloudflared)", pathKey: "cloudflaredPath" },
};

export const CLI_RESOLUTION_SOURCE_LABELS: Record<CliResolutionSource, string> = {
  custom: "自定义路径",
  "process-path": "当前环境",
  "login-shell-path": "登录 Shell",
  "platform-fallback": "系统常用位置",
};

export function cloneCliTools(config: CliToolsConfig): CliToolsConfig {
  return { ...config };
}

function isAbsoluteCliPath(path: string, platform: string): boolean {
  if (platform === "windows") {
    return (
      /^[A-Za-z]:[\\/]/.test(path) ||
      /^\\\\[^\\]+\\[^\\]+/.test(path) ||
      /^\/\/[^/]+\/[^/]+/.test(path)
    );
  }
  return path.startsWith("/");
}

export function cliToolsPathErrors(
  config: CliToolsConfig,
  platform: string | undefined = import.meta.env.TAURI_ENV_PLATFORM,
): Partial<Record<CliTool, string>> {
  const errors: Partial<Record<CliTool, string>> = {};
  // Plain web builds do not have a Tauri host platform. Avoid turning a heuristic into a false
  // hard gate there; deserialization/save in Rust remains the authoritative validation boundary.
  if (!platform) return errors;

  for (const tool of CLI_TOOLS) {
    const path = config[CLI_TOOL_META[tool].pathKey];
    if (path !== "" && !isAbsoluteCliPath(path, platform)) {
      errors[tool] = "请输入当前系统的绝对路径；最终以后端校验为准。";
    }
  }
  return errors;
}

export function isCliToolsSaveError(message: string): boolean {
  return (
    message.includes("CLI 路径") ||
    message.includes("cliTools") ||
    CLI_TOOLS.some((tool) => message.includes(CLI_TOOL_META[tool].pathKey))
  );
}

export function cliProbeSnapshotMatches(
  probed: CliToolsConfig,
  current: CliToolsConfig,
): boolean {
  return CLI_TOOLS.every(
    (tool) =>
      probed[CLI_TOOL_META[tool].pathKey] === current[CLI_TOOL_META[tool].pathKey],
  );
}

export function probeStatusesByTool(
  statuses: CliToolProbeStatus[],
): Partial<Record<CliTool, CliToolProbeStatus>> {
  const indexed: Partial<Record<CliTool, CliToolProbeStatus>> = {};
  for (const status of statuses) indexed[status.tool] = status;
  return indexed;
}
