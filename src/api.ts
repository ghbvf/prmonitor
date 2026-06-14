// Adapter to the Rust backend: `invoke` wrappers (and, in later PRs, event /
// channel subscriptions). The only place the frontend talks to Tauri commands.

import { invoke } from "@tauri-apps/api/core";
import type { AppConfig } from "./types";

export function appVersion(): Promise<string> {
  return invoke<string>("app_version");
}

export function getConfig(): Promise<AppConfig> {
  return invoke<AppConfig>("get_config");
}
