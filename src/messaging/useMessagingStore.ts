import { defineStore } from "pinia";
import {
  messagingEventRaw,
  messagingEventReplay,
  messagingEventsList,
  messagingIntegrationsList,
  messagingConnectionStatusesList,
  messagingSend,
  messagingSendsList,
} from "./api";
import type { OutboxEntry } from "../types";
import type {
  FeishuConnectionStatus,
  MessagingEventEntry,
  MessagingIntegrationOption,
  SendMessagingRequest,
} from "../types.generated";

interface MessagingState {
  integrations: MessagingIntegrationOption[];
  connectionStatuses: FeishuConnectionStatus[];
  entries: MessagingEventEntry[];
  sends: OutboxEntry[];
  integrationsLoading: boolean;
  connectionStatusesLoading: boolean;
  connectionStatusesError: string | null;
  connectionStatusesStale: boolean;
  connectionStatusesLastUpdatedAt: number | null;
  loading: boolean;
  sendsLoading: boolean;
  sendLoading: boolean;
  error: string | null;
  sendError: string | null;
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
    integrations: [],
    connectionStatuses: [],
    entries: [],
    sends: [],
    integrationsLoading: false,
    connectionStatusesLoading: false,
    connectionStatusesError: null,
    connectionStatusesStale: false,
    connectionStatusesLastUpdatedAt: null,
    loading: false,
    sendsLoading: false,
    sendLoading: false,
    error: null,
    sendError: null,
    rawCache: {},
    rawLoading: {},
    rawError: {},
    replayLoading: {},
  }),
  actions: {
    async refreshIntegrations() {
      this.integrationsLoading = true;
      this.error = null;
      try {
        this.integrations = await messagingIntegrationsList();
      } catch (err) {
        this.error = toMessage(err);
      } finally {
        this.integrationsLoading = false;
      }
    },
    async refreshConnectionStatuses() {
      this.connectionStatusesLoading = true;
      this.connectionStatusesError = null;
      try {
        this.connectionStatuses = await messagingConnectionStatusesList();
        this.connectionStatusesStale = false;
        this.connectionStatusesLastUpdatedAt = Date.now();
      } catch (err) {
        this.connectionStatusesError = toMessage(err);
        this.connectionStatusesStale = this.connectionStatuses.length > 0;
      } finally {
        this.connectionStatusesLoading = false;
      }
    },
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
    async refreshSends() {
      this.sendsLoading = true;
      this.error = null;
      try {
        this.sends = await messagingSendsList();
      } catch (err) {
        this.error = toMessage(err);
      } finally {
        this.sendsLoading = false;
      }
    },
    async send(request: SendMessagingRequest) {
      this.sendLoading = true;
      this.sendError = null;
      try {
        await messagingSend(request);
        await this.refreshSends();
      } catch (err) {
        this.sendError = toMessage(err);
      } finally {
        this.sendLoading = false;
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
