// Config slice → backend adapter. Wraps the config commands.
import { getTransport } from "../transport";
import type {
  AppConfig,
  CliToolProbeStatus,
  CliToolsConfig,
  NotificationChannel,
  RemoteAccessRuntimeStatus,
} from "./types";
import type { FeishuConnectionStatus } from "../types.generated";

export function appVersion(): Promise<string> {
  return getTransport().request<string>("app_version");
}

export function getConfig(): Promise<AppConfig> {
  return getTransport().request<AppConfig>("get_config");
}

export function setConfig(config: AppConfig): Promise<void> {
  return getTransport().request<void>("set_config", { config });
}

export function probeCliTools(
  cliTools: CliToolsConfig,
  refreshPath: boolean,
): Promise<CliToolProbeStatus[]> {
  return getTransport().request<CliToolProbeStatus[]>("probe_cli_tools", {
    cliTools,
    refreshPath,
  });
}

// Persist the active-project selection (#35). Centralizes the `set_active_project`
// command name + `{ projectId }` arg shape here alongside the other config commands
// so the wire string lives in one place (the composition-root projects.ts calls this
// rather than re-spelling the command name).
export function setActiveProject(projectId: string): Promise<void> {
  return getTransport().request<void>("set_active_project", { projectId });
}

export function notificationTestSend(channel: NotificationChannel): Promise<string> {
  return getTransport().request<string>("notification_test_send", { channel });
}

export function getRemoteAccessRuntimeStatus(): Promise<RemoteAccessRuntimeStatus> {
  return getTransport().request<RemoteAccessRuntimeStatus>("get_remote_access_runtime_status");
}

export function getFeishuConnectionStatuses(): Promise<FeishuConnectionStatus[]> {
  return getTransport().request<FeishuConnectionStatus[]>("messaging_connection_statuses_list");
}
