import { defineStore } from "pinia";
import { messagingEventRaw, messagingEventReplay, messagingEventsList } from "./api";
import type { MessagingEventEntry } from "../types.generated";

interface MessagingState {
  entries: MessagingEventEntry[];
  loading: boolean;
  error: string | null;
  rawCache: Record<number, string>;
  rawLoading: Record<number, boolean>;
  rawError: Record<number, string | null>;
  replayLoading: Record<number, boolean>;
}

function toMessage(err: unknown): string {
  return (err as { message?: string })?.message ?? String(err);
}

export const useMessagingStore = defineStore("messaging", {
  state: (): MessagingState => ({
    entries: [],
    loading: false,
    error: null,
    rawCache: {},
    rawLoading: {},
    rawError: {},
    replayLoading: {},
  }),
  actions: {
    async refresh() {
      this.loading = true;
      this.error = null;
      try {
        this.entries = await messagingEventsList();
      } catch (err) {
        this.error = toMessage(err);
      } finally {
        this.loading = false;
      }
    },
    async fetchRaw(id: number) {
      if (this.rawCache[id] !== undefined || this.rawLoading[id]) return;
      this.rawLoading[id] = true;
      this.rawError[id] = null;
      try {
        this.rawCache[id] = await messagingEventRaw(id);
      } catch (err) {
        this.rawError[id] = toMessage(err);
      } finally {
        this.rawLoading[id] = false;
      }
    },
    async replay(id: number) {
      if (this.replayLoading[id]) return;
      this.replayLoading[id] = true;
      this.error = null;
      try {
        await messagingEventReplay(id);
        await this.refresh();
      } catch (err) {
        this.error = toMessage(err);
      } finally {
        this.replayLoading[id] = false;
      }
    },
  },
});