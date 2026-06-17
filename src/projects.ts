// Composition-root project state (#35): the monitored-project list + active
// selection. Lives at the composition layer (a `src/` root file, NOT inside a slice)
// — App.vue and the slice views read it without prop-drilling. Module-level singleton
// `ref`s (mirroring useAppView.ts / the review slice's useReviewStore pattern).
import { ref } from "vue";
import type { AppConfig, Project } from "./config/types";
import { setActiveProject } from "./config/api";

const activeProjectId = ref<string>("");
const projects = ref<Project[]>([]);

export function useProjects() {
  return {
    activeProjectId,
    projects,
    setActive: async (id: string) => {
      // Persist FIRST, then update the local active id (#35 F6). The previous order
      // (optimistic local update before the await) left the UI switched to a project the
      // backend never persisted if the command rejected (stale/dangling id, IPC error) —
      // and there was no rollback. Persisting first means a failure throws BEFORE any
      // local switch, so the UI stays on the current project and the caller surfaces it.
      // Centralized with the other config commands (see config/api.ts). projects.ts is
      // a composition-root module (not a slice), so importing config/api is allowed —
      // and config/api never imports projects.ts, so there's no value-import cycle.
      await setActiveProject(id);
      activeProjectId.value = id;
    },
    hydrate: (cfg: AppConfig) => {
      projects.value = cfg.projects;
      activeProjectId.value = cfg.activeProjectId;
    },
  };
}
