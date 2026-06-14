// PR slice store. Funnels the PR-discovery backend round-trips (fetchPrsNow /
// ghStatus) through here so the views stay free of invoke/error plumbing.
// Mirrors the option-store pattern set by useConfigStore.
import { defineStore } from "pinia";
import { fetchPrsNow, ghStatus } from "./api";
import type { PullRequestView } from "../types";
import type { GhStatus } from "./types";

interface PrState {
  prs: PullRequestView[];
  loading: boolean;
  error: string | null;
  gh: GhStatus | null;
  ghLoading: boolean;
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
  }),
  actions: {
    async fetchNow() {
      if (this.loading) return;
      this.loading = true;
      this.error = null;
      try {
        this.prs = await fetchPrsNow();
      } catch (err) {
        this.error = toMessage(err);
      } finally {
        this.loading = false;
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
