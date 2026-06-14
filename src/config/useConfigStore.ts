// Config slice store. The repo's first Pinia store — sets the option-store pattern
// other slices follow. State holds the persisted AppConfig plus async UI flags;
// actions funnel every backend round-trip (getConfig / setConfig) through here so
// the view stays free of invoke/error plumbing.
import { defineStore } from "pinia";
import { getConfig, setConfig } from "./api";
import type { AppConfig } from "./types";

interface ConfigState {
  config: AppConfig | null;
  loading: boolean;
  saving: boolean;
  error: string | null;
  savedOk: boolean;
}

// A rejected Tauri invoke throws the AppError object `{ message }`; fall back to
// a stringified form for any non-conforming throw.
function toMessage(err: unknown): string {
  return (err as { message?: string })?.message ?? String(err);
}

export const useConfigStore = defineStore("config", {
  state: (): ConfigState => ({
    config: null,
    loading: false,
    saving: false,
    error: null,
    savedOk: false,
  }),
  actions: {
    async load() {
      if (this.loading) return;
      this.loading = true;
      this.error = null;
      try {
        this.config = await getConfig();
      } catch (err) {
        this.error = toMessage(err);
      } finally {
        this.loading = false;
      }
    },
    async save(next: AppConfig) {
      if (this.saving) return;
      this.saving = true;
      this.error = null;
      this.savedOk = false;
      try {
        await setConfig(next);
        this.config = next;
        this.savedOk = true;
      } catch (err) {
        this.error = toMessage(err);
      } finally {
        this.saving = false;
      }
    },
  },
});
