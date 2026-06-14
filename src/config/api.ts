// Config slice → backend adapter. Wraps the config commands.
import { invoke } from "../api";
import type { AppConfig } from "./types";

export function appVersion(): Promise<string> {
  return invoke<string>("app_version");
}

export function getConfig(): Promise<AppConfig> {
  return invoke<AppConfig>("get_config");
}
