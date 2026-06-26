import "./design/tokens.css";
import { createApp } from "vue";
import { createPinia } from "pinia";
import App from "./App.vue";
import { setTransport } from "./transport";

// Pick the backend transport at boot, BEFORE mounting, so every store init / event
// subscription (which runs after mount) sees a ready transport (AB#1375). Tauri v2 injects
// `__TAURI_INTERNALS__` on window; its absence means we are a plain browser → HttpTransport.
// The adapters are DYNAMICALLY imported so the desktop-only `@tauri-apps/*` code lands in a
// separate chunk the browser SPA build never loads.
async function boot() {
  const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
  const transport = isTauri
    ? (await import("./transport/tauri")).createTauriTransport()
    : (await import("./transport/http")).createHttpTransport(
        import.meta.env.VITE_API_BASE_URL ?? "",
      );
  setTransport(transport);
  createApp(App).use(createPinia()).mount("#app");
}

void boot();
