// Config slice → backend adapter. Wraps the config commands.
import { invoke } from "../api";
import type { AppConfig, ListenerRuntimeStatus } from "./types";

export function appVersion(): Promise<string> {
  return invoke<string>("app_version");
}

export function getConfig(): Promise<AppConfig> {
  return invoke<AppConfig>("get_config");
}

export function setConfig(config: AppConfig): Promise<void> {
  return invoke<void>("set_config", { config });
}

// Persist the active-project selection (#35). Centralizes the `set_active_project`
// command name + `{ projectId }` arg shape here alongside the other config commands
// so the wire string lives in one place (the composition-root projects.ts calls this
// rather than re-spelling the command name).
export function setActiveProject(projectId: string): Promise<void> {
  return invoke<void>("set_active_project", { projectId });
}

// Fetch the runtime binding status for every configured listener (AB#1225 PR1).
// Mirrors the Rust `get_listener_runtime_status` command's return shape
// (`Vec<ListenerRuntimeStatus>` → `ListenerRuntimeStatus[]` camelCase JSON).
export function getListenerRuntimeStatus(): Promise<ListenerRuntimeStatus[]> {
  return invoke<ListenerRuntimeStatus[]>("get_listener_runtime_status");
}
