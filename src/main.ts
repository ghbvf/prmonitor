import "./design/tokens.css";
import { createApp } from "vue";
import { createPinia } from "pinia";
import App from "./App.vue";
import { setTransport } from "./transport";
import { isRemoteWebConsolePath } from "./remoteConsole/route";

// Pick the backend transport at boot, BEFORE mounting, so every store init / event
// subscription (which runs after mount) sees a ready transport (AB#1375). Tauri v2 injects
// `__TAURI_INTERNALS__` on window; its absence means we are a plain browser → HttpTransport.
// The adapters are DYNAMICALLY imported so the desktop-only `@tauri-apps/*` code lands in a
// separate chunk the browser SPA build never loads.
async function boot() {
  const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
  const transport = isTauri
    ? (await import("./transport/tauri")).createTauriTransport()
    : await browserTransport();
  setTransport(transport);
  const Root = isTauri ? App : await browserRoot();
  createApp(Root).use(createPinia()).mount("#app");
}

async function browserRoot() {
  if (isRemoteWebConsolePath(window.location.pathname)) {
    return (await import("./RemoteWebConsoleApp.vue")).default;
  }
  return (await import("./RemoteTerminalApp.vue")).default;
}

async function browserTransport() {
  const http = await import("./transport/http");
  ensureBrowserToken(http.REMOTE_BEARER_TOKEN_KEY, "Remote access token");
  if (isRemoteWebConsolePath(window.location.pathname)) {
    const { LOCAL_API_BEARER_TOKEN_KEY } = await import("./remoteConsole/api");
    ensureBrowserToken(LOCAL_API_BEARER_TOKEN_KEY, "Local API token");
  }
  return http.createHttpTransport(browserApiBaseUrl(), {
    onAuthRejected: () => {
      sessionStorage.removeItem(http.REMOTE_BEARER_TOKEN_KEY);
      const entered = promptForToken("Remote access token")?.trim();
      if (!entered) return false;
      sessionStorage.setItem(http.REMOTE_BEARER_TOKEN_KEY, entered);
      return true;
    },
  });
}

function ensureBrowserToken(key: string, label: string): void {
  const existing = sessionStorage.getItem(key)?.trim();
  if (existing) return;
  const entered = promptForToken(label)?.trim();
  if (entered) sessionStorage.setItem(key, entered);
}

function browserApiBaseUrl(): string {
  const configured = import.meta.env.VITE_API_BASE_URL?.trim();
  if (configured) return trimTrailingSlashes(configured);
  return trimTrailingSlashes(window.__PRMONITOR_REMOTE_BASE_PATH__?.trim() ?? "");
}

function trimTrailingSlashes(value: string): string {
  return value.replace(/\/+$/, "");
}

function promptForToken(label: string): string | null {
  return window.prompt(label);
}

void boot();
