// Inbox slice store (AB#1065). Funnels the inbox backend round-trips through here so the
// view stays free of invoke/error plumbing. Event-driven: `subscribe` wires the
// `inbox:updated` push stream into state; the commands only trigger the backend and let
// the listener apply results. Mirrors the option-store pattern set by usePrStore.
import { defineStore } from "pinia";
import { inboxGetRaw, inboxList, inboxReplay, onInboxUpdated } from "./api";
import { assertNever } from "../types";
import type { InboxEntry } from "../types";

interface InboxState {
  // The retained inbox entries, newest first (backend order; the UPSERT below preserves
  // it by re-sorting on `id` descending).
  entries: InboxEntry[];
  loading: boolean;
  error: string | null;
  // Optional project-scope filter passed to `inbox_list`; null = every project.
  projectId: string | null;
  // Raw payloads fetched lazily by the "View raw" disclosure, cached once per entry id.
  rawCache: Record<number, string>;
  // Per-entry in-flight flag for the raw fetch, so the toggle can show a pending state
  // and the cache-once guard ignores a second click while the first is still loading.
  rawLoading: Record<number, boolean>;
  // Per-entry raw-fetch error, kept SEPARATE from the shared `error` so a failed raw
  // disclosure shows inline next to that row's <pre> region instead of hijacking the
  // panel-wide banner. Null/absent = no error; cleared when a (re)fetch starts/succeeds.
  rawError: Record<number, string | null>;
  // Per-entry in-flight flag for replay, so the row's button disables + shows "重放中…"
  // while the command is outstanding (prevents multi-click double-dispatch).
  replayLoading: Record<number, boolean>;
}

// A rejected Tauri invoke throws the AppError object `{ message }`; fall back to a
// stringified form for any non-conforming throw (mirrors usePrStore.toMessage).
function toMessage(err: unknown): string {
  return (err as { message?: string })?.message ?? String(err);
}

export const useInboxStore = defineStore("inbox", {
  state: (): InboxState => ({
    entries: [],
    loading: false,
    error: null,
    projectId: null,
    rawCache: {},
    rawLoading: {},
    rawError: {},
    replayLoading: {},
  }),
  actions: {
    // Wire the `inbox:updated` push stream into state. Each event carries ONE entry;
    // UPSERT it by `id` (replace an existing entry — e.g. a replay flipping status — or
    // prepend a new one) and keep the list newest-first by re-sorting on id descending.
    // Returns the `Promise<UnlistenFn>` so the component can await it for cleanup.
    subscribe() {
      return onInboxUpdated((e) => {
        switch (e.kind) {
          case "updated":
            if (this.projectId !== null && e.entry.event.projectId !== this.projectId) {
              return;
            }
            this.upsert(e.entry);
            return;
          case "error":
            this.error = `${e.operation}: ${e.message}`;
            return;
          default:
            return assertNever(e);
        }
      });
    },
    // Replace-or-prepend an entry by id, then re-sort newest-first. Extracted so the
    // listener and any future direct apply share one ordering invariant.
    upsert(entry: InboxEntry) {
      const idx = this.entries.findIndex((x) => x.id === entry.id);
      if (idx >= 0) {
        this.entries[idx] = entry;
      } else {
        this.entries.push(entry);
      }
      this.entries.sort((a, b) => b.id - a.id);
    },
    // Re-read the inbox snapshot into state (AB#1065). Surfaces a rejected command via
    // `error` (no list mutation) rather than throwing. Scoped to `projectId` when set.
    async refresh() {
      this.loading = true;
      this.error = null;
      try {
        this.entries = await inboxList(this.projectId ?? undefined);
      } catch (err) {
        this.error = toMessage(err);
      } finally {
        this.loading = false;
      }
    },
    // Startup wiring: subscribe FIRST (await the listener registration) THEN read the
    // snapshot, so no `inbox:updated` event fired between snapshot-read and listener
    // registration is lost (mirrors usePrStore.init's #27 F3 ordering). Returns the
    // `Promise<UnlistenFn>` for the component to await + invoke on unmount.
    async init() {
      const unlisten = this.subscribe(); // onInboxUpdated -> Promise<UnlistenFn>
      await unlisten; // ensure the listener is registered
      await this.refresh();
      return unlisten;
    },
    // Lazily fetch one entry's raw payload, cached once. A second call for an already-
    // cached or in-flight id no-ops. A rejected fetch surfaces on the per-entry `rawError`
    // (NOT the shared banner) so the failure shows inline at that row, and clears the
    // in-flight flag so the user can retry from the same disclosure.
    async fetchRaw(id: number) {
      if (this.rawCache[id] !== undefined || this.rawLoading[id]) return;
      this.rawLoading[id] = true;
      this.rawError[id] = null; // clear any prior failure before the (re)try
      try {
        this.rawCache[id] = await inboxGetRaw(id);
      } catch (err) {
        this.rawError[id] = toMessage(err);
      } finally {
        this.rawLoading[id] = false;
      }
    },
    // Re-run processing for one entry (AB#1065). On success, the backend ALSO re-emits
    // `inbox:updated` for the entry (the `subscribe` listener would apply the new status),
    // but we DETERMINISTICALLY reconcile by awaiting a refresh too (pr-review F5): the row
    // reflects the new status even if that push is missed, rather than depending on a
    // best-effort event. A rejected invoke surfaces the message and still refreshes so any
    // partial state is reconciled. `replayLoading[id]` gates re-entry so a double-click
    // can't double-dispatch (the view also disables the button on it).
    async replay(id: number) {
      if (this.replayLoading[id]) return;
      this.replayLoading[id] = true;
      this.error = null;
      try {
        await inboxReplay(id);
        // Deterministic reconcile (F5): pull the post-replay snapshot so the status is
        // authoritative even without the re-emitted `inbox:updated`.
        await this.refresh();
      } catch (err) {
        const message = toMessage(err);
        // The replay never ran, so the re-emit won't come — pull a fresh snapshot so any
        // partial state is reconciled rather than left to a phantom listener update.
        // refresh() resets `error` to null on its way in, so set the replay message AFTER
        // it returns: the failure the user acted on is the proximate cause and must survive
        // the fallback refresh (which, if it ALSO fails, is the less actionable error here).
        await this.refresh();
        this.error = message;
      } finally {
        this.replayLoading[id] = false;
      }
    },
  },
});
