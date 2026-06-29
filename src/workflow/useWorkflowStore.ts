import { defineStore } from "pinia";
import type { WorkflowEvent, WorkflowInstance } from "../types";
import {
  onWorkflowUpdated,
  workflowGetRaw,
  workflowList,
  workflowRetry,
} from "./api";

interface WorkflowState {
  entries: WorkflowInstance[];
  loading: boolean;
  error: string | null;
  cycleError: string | null;
  projectId: string | null;
  rawCache: Record<number, string>;
  rawLoading: Record<number, boolean>;
  rawError: Record<number, string | null>;
  retryLoading: Record<number, boolean>;
}

function toMessage(err: unknown): string {
  return (err as { message?: string })?.message ?? String(err);
}

export const useWorkflowStore = defineStore("workflow", {
  state: (): WorkflowState => ({
    entries: [],
    loading: false,
    error: null,
    cycleError: null,
    projectId: null,
    rawCache: {},
    rawLoading: {},
    rawError: {},
    retryLoading: {},
  }),
  actions: {
    subscribe() {
      return onWorkflowUpdated((e: WorkflowEvent) => {
        switch (e.kind) {
          case "updated":
            if (this.projectId !== null && e.instance.projectId !== this.projectId) {
              return;
            }
            this.upsert(e.instance);
            return;
          case "error":
            this.cycleError = `${e.operation}: ${e.message}`;
            return;
          default:
            return assertNeverWorkflowEvent(e);
        }
      });
    },
    upsert(entry: WorkflowInstance) {
      const idx = this.entries.findIndex((x) => x.id === entry.id);
      if (idx >= 0) {
        this.entries[idx] = entry;
      } else {
        this.entries.push(entry);
      }
      this.entries.sort((a, b) => b.updatedAt - a.updatedAt || b.id - a.id);
    },
    async refresh() {
      this.loading = true;
      this.error = null;
      try {
        this.entries = await workflowList(this.projectId ?? undefined);
      } catch (err) {
        this.error = toMessage(err);
      } finally {
        this.loading = false;
      }
    },
    async init() {
      const unlisten = this.subscribe();
      await unlisten;
      await this.refresh();
      return unlisten;
    },
    async fetchRaw(id: number) {
      if (this.rawCache[id] !== undefined || this.rawLoading[id]) return;
      this.rawLoading[id] = true;
      this.rawError[id] = null;
      try {
        this.rawCache[id] = await workflowGetRaw(id);
      } catch (err) {
        this.rawError[id] = toMessage(err);
      } finally {
        this.rawLoading[id] = false;
      }
    },
    async retry(id: number) {
      if (this.retryLoading[id]) return;
      this.retryLoading[id] = true;
      this.error = null;
      try {
        await workflowRetry(id);
        await this.refresh();
      } catch (err) {
        const message = toMessage(err);
        await this.refresh();
        this.error = message;
      } finally {
        this.retryLoading[id] = false;
      }
    },
  },
});

function assertNeverWorkflowEvent(x: never): never {
  throw new Error(`Unhandled workflow event: ${JSON.stringify(x)}`);
}
