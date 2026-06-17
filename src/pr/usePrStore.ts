// PR slice store. Funnels the polling-scheduler backend round-trips through here
// so the views stay free of invoke/error plumbing. Event-driven: `subscribe`
// wires the `prs:updated` push stream into state; the commands only trigger the
// backend and let the listener apply results.
// Mirrors the option-store pattern set by useConfigStore.
//
// Multi-project (#35): state is partitioned by `projectId`. Every per-project
// field is a `Record<string, …>` keyed by the project id the backend stamps on
// each event (`PrEvent.projectId`). Components stay prop-free by reading the
// `currentPrs`/`stalePrs`/`archivedPrs` wrappers, which resolve the ACTIVE
// project from `useProjects()`; tests can target a specific partition via the
// parameterized `…For(id)` getters. The gh-auth fields stay scalar — gh login is
// process-global, not per project.
import { defineStore } from "pinia";
import {
  getPrs,
  ghStatus,
  onPrsUpdated,
  pollNow,
  setPrArchived,
  startPolling,
  stopPolling,
} from "./api";
import type { TrackedPrView } from "../types";
import type { GhStatus } from "./types";
import { useProjects } from "../projects";

interface PrState {
  // Per-project partitions, keyed by projectId. A missing key reads as the
  // documented empty default (see the `…For` getters / scalar reads below).
  prs: Record<string, TrackedPrView[]>;
  loading: Record<string, boolean>;
  error: Record<string, string | null>;
  lastPulledAt: Record<string, number | null>;
  // Whether a project's loop is running. Defaults true on first read — see the
  // `polling` auto-start note below.
  polling: Record<string, boolean>;
  // Set when a NON-active project receives a PR it hadn't seen before, so the
  // ProjectSwitcher can flag it; cleared when the user switches to that project.
  hasNewPr: Record<string, boolean>;
  // Visited-once guard for loadSnapshot (per project), so a switch back to an
  // already-baselined project does not re-fetch needlessly.
  snapshotLoaded: Record<string, boolean>;
  // gh CLI auth is process-global, not per-project — stays scalar.
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
    prs: {},
    loading: {},
    error: {},
    lastPulledAt: {},
    polling: {},
    hasNewPr: {},
    snapshotLoaded: {},
    gh: null,
    ghLoading: false,
  }),
  getters: {
    // Parameterized tracking-aware partitions (#38), scoped to one project (#35).
    // The store holds each project's full retained list; these split it into the
    // three sections PrList renders. "current" = active and not archived; "stale"
    // = retained-but-inactive and not archived; "archived" = hidden until restored.
    currentPrsFor(state) {
      return (id: string): TrackedPrView[] =>
        (state.prs[id] ?? []).filter(
          (p) => !p.archived && p.presence === "current",
        );
    },
    stalePrsFor(state) {
      return (id: string): TrackedPrView[] =>
        (state.prs[id] ?? []).filter(
          (p) => !p.archived && p.presence === "stale",
        );
    },
    archivedPrsFor(state) {
      return (id: string): TrackedPrView[] =>
        (state.prs[id] ?? []).filter((p) => p.archived);
    },
    // Active-project wrappers: read the current selection from useProjects() so
    // components stay prop-free. (Pinia getters can call other getters via `this`.)
    currentPrs(): TrackedPrView[] {
      return this.currentPrsFor(useProjects().activeProjectId.value);
    },
    stalePrs(): TrackedPrView[] {
      return this.stalePrsFor(useProjects().activeProjectId.value);
    },
    archivedPrs(): TrackedPrView[] {
      return this.archivedPrsFor(useProjects().activeProjectId.value);
    },
    // Scalar reads of the ACTIVE project's per-project flags, so the existing
    // prop-free components (PollControls / PrList) keep their `.loading`/`.error`/…
    // call sites. A missing key reads as the documented default: polling defaults
    // true (see the auto-start note), everything else falsy/null.
    loadingActive(state): boolean {
      return state.loading[useProjects().activeProjectId.value] ?? false;
    },
    errorActive(state): string | null {
      return state.error[useProjects().activeProjectId.value] ?? null;
    },
    lastPulledAtActive(state): number | null {
      return state.lastPulledAt[useProjects().activeProjectId.value] ?? null;
    },
    // Reflects the backend's *conditional* auto-start (lib.rs): the loop starts at
    // launch only when the persisted config is valid. The monitor view is reached
    // either with a valid config (loop already running) or right after onboarding
    // calls startPolling — so `true` holds on both paths. (A non-empty but invalid
    // hand-edited repoRoot is the lone exception; the user fixes it in Settings.)
    pollingFor(state) {
      return (id: string): boolean => state.polling[id] ?? true;
    },
    pollingActive(state): boolean {
      return state.polling[useProjects().activeProjectId.value] ?? true;
    },
  },
  actions: {
    // Wire the `prs:updated` push stream into state, routing each payload to the
    // project it belongs to (#35). Returns the `Promise<UnlistenFn>` so the
    // component can await it for cleanup.
    subscribe() {
      return onPrsUpdated((e) => {
        const pid = e.projectId;
        if (e.kind === "updated") {
          // Flag a NON-active project that gained a PR number it had not held,
          // so the ProjectSwitcher can badge it. Compute BEFORE overwriting prs.
          const activeId = useProjects().activeProjectId.value;
          if (pid !== activeId) {
            const prevNumbers = new Set(
              (this.prs[pid] ?? []).map((p) => p.number),
            );
            const gainedNew = e.prs.some((p) => !prevNumbers.has(p.number));
            if (gainedNew) this.hasNewPr[pid] = true;
          }
          this.prs[pid] = e.prs;
          this.lastPulledAt[pid] = Date.now();
          // Clear any stale error banner so a recovered poll un-masks the list.
          this.error[pid] = null;
        } else {
          this.error[pid] = e.message;
        }
        this.loading[pid] = false;
      });
    },
    // Read one project's backend PR snapshot into its partition (#35). Guarded
    // visited-once so a switch back to an already-baselined project skips the
    // round-trip. Tolerates a rejected command by surfacing the message (no list
    // mutation) rather than throwing.
    async loadSnapshot(projectId: string) {
      if (this.snapshotLoaded[projectId]) return;
      this.snapshotLoaded[projectId] = true;
      try {
        this.prs[projectId] = await getPrs(projectId);
      } catch (err) {
        // Reset the guard so a later retry (e.g. after fixing auth) can re-fetch.
        this.snapshotLoaded[projectId] = false;
        this.error[projectId] = toMessage(err);
      }
    },
    // Startup wiring: subscribe FIRST (await the listener registration) THEN read
    // the active project's snapshot, so no `prs:updated` event fired between
    // snapshot-read and listener-registration is lost (#27 F3). Returns the
    // `Promise<UnlistenFn>` for the component to await + invoke on unmount.
    async init() {
      const unlisten = this.subscribe(); // onPrsUpdated -> Promise<UnlistenFn>
      await unlisten; // ensure the listener is registered
      const activeId = useProjects().activeProjectId.value;
      if (activeId) await this.loadSnapshot(activeId); // baseline active project
      return unlisten;
    },
    // Switch the active project (#35): persist the selection, baseline its list if
    // not yet loaded, and clear its new-PR flag. ProjectSwitcher calls this.
    async switchTo(id: string) {
      await useProjects().setActive(id);
      this.hasNewPr[id] = false;
      await this.loadSnapshot(id);
    },
    async pollNow(projectId: string) {
      if (this.loading[projectId]) return;
      this.loading[projectId] = true;
      this.error[projectId] = null;
      try {
        await pollNow(projectId);
        // On success leave loading=true; the `prs:updated` listener clears it
        // when the event arrives. The error case clears loading itself since
        // no event will come.
      } catch (err) {
        this.error[projectId] = toMessage(err);
        this.loading[projectId] = false;
      }
    },
    // Global pause/resume of the scheduler (#35): start_polling / stop_polling
    // reconcile/stop ALL projects, so the toggle flips every project's flag.
    async toggle() {
      const ids = useProjects().projects.value.map((p) => p.id);
      const activeId = useProjects().activeProjectId.value;
      const wasPolling = this.pollingFor(activeId);
      try {
        if (wasPolling) {
          await stopPolling();
          for (const id of ids) this.polling[id] = false;
        } else {
          await startPolling();
          for (const id of ids) this.polling[id] = true;
        }
      } catch (err) {
        this.error[activeId] = toMessage(err);
      }
    },
    // Archive / unarchive a retained PR within a project (#38, #35). Fire-and-rely:
    // the backend flips the flag and re-emits `prs:updated` for that project, so
    // the `subscribe` listener refreshes the list — no local mutation here. A
    // rejected invoke surfaces the message on that project.
    async setArchived(projectId: string, number: number, archived: boolean) {
      try {
        await setPrArchived(projectId, number, archived);
      } catch (err) {
        this.error[projectId] = toMessage(err);
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
