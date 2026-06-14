// PR slice store. Funnels the polling-scheduler backend round-trips through here
// so the views stay free of invoke/error plumbing. Event-driven: `subscribe`
// wires the `prs:updated` push stream into state; the commands only trigger the
// backend and let the listener apply results.
// Mirrors the option-store pattern set by useConfigStore.
import { defineStore } from "pinia";
import {
  getPrs,
  ghStatus,
  onPrsUpdated,
  pollNow,
  startPolling,
  stopPolling,
} from "./api";
import type { PullRequestView } from "../types";
import type { GhStatus } from "./types";

interface PrState {
  prs: PullRequestView[];
  loading: boolean;
  error: string | null;
  gh: GhStatus | null;
  ghLoading: boolean;
  lastPulledAt: number | null;
  polling: boolean;
}

// A rejected Tauri invoke throws the AppError object `{ message }`; fall back to
// a stringified form for any non-conforming throw.
function toMessage(err: unknown): string {
  return (err as { message?: string })?.message ?? String(err);
}

export const usePrStore = defineStore("pr", {
  state: (): PrState => ({
    prs: [],
    loading: false,
    error: null,
    gh: null,
    ghLoading: false,
    lastPulledAt: null,
    // Reflects the backend auto-start in lib.rs setup.
    polling: true,
  }),
  actions: {
    // Wire the `prs:updated` push stream into state. Returns the
    // `Promise<UnlistenFn>` so the component can await it for cleanup.
    subscribe() {
      return onPrsUpdated((e) => {
        if (e.kind === "updated") {
          this.prs = e.prs;
          this.lastPulledAt = Date.now();
          // Clear any stale error banner so a recovered poll un-masks the list.
          this.error = null;
        } else {
          this.error = e.message;
        }
        this.loading = false;
      });
    },
    // Read the backend's current PR snapshot into state. Tolerates a rejected
    // command by surfacing the message (no list mutation) rather than throwing.
    async loadSnapshot() {
      try {
        this.prs = await getPrs();
      } catch (err) {
        this.error = toMessage(err);
      }
    },
    // Startup wiring: subscribe FIRST (await the listener registration) THEN read
    // the snapshot, so no `prs:updated` event fired between snapshot-read and
    // listener-registration is lost (#27 F3). Returns the `Promise<UnlistenFn>`
    // for the component to await + invoke on unmount.
    async init() {
      const unlisten = this.subscribe(); // onPrsUpdated -> Promise<UnlistenFn>
      await unlisten; // ensure the listener is registered
      await this.loadSnapshot(); // baseline current state
      return unlisten;
    },
    async pollNow() {
      if (this.loading) return;
      this.loading = true;
      this.error = null;
      try {
        await pollNow();
        // On success leave loading=true; the `prs:updated` listener clears it
        // when the event arrives. The error case clears loading itself since
        // no event will come.
      } catch (err) {
        this.error = toMessage(err);
        this.loading = false;
      }
    },
    async toggle() {
      try {
        if (this.polling) {
          await stopPolling();
          this.polling = false;
        } else {
          await startPolling();
          this.polling = true;
        }
      } catch (err) {
        this.error = toMessage(err);
      }
    },
    async refreshGhStatus() {
      if (this.ghLoading) return;
      this.ghLoading = true;
      try {
        this.gh = await ghStatus();
      } catch (err) {
        this.gh = { authenticated: false, message: toMessage(err) };
      } finally {
        this.ghLoading = false;
      }
    },
  },
});
