// TauriTransport: the desktop adapter and the ONLY module allowed to import `@tauri-apps/*`
// (enforced by the tauri-import funnel in slice-boundary.test.ts). request→invoke,
// subscribe→listen (unwrapping the Event.payload envelope), openExternal→opener plugin.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import type { Transport, UnlistenFn } from "./index";

export function createTauriTransport(): Transport {
  return {
    request<T>(command: string, args?: Record<string, unknown>): Promise<T> {
      return invoke<T>(command, args);
    },
    subscribe<T>(event: string, handler: (payload: T) => void): Promise<UnlistenFn> {
      // Tauri delivers an Event<T> envelope; the port contract hands callers the payload.
      return listen<T>(event, (e) => handler(e.payload));
    },
    openExternal(url: string): Promise<void> {
      return openUrl(url);
    },
  };
}
