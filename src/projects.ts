// Composition-root project state (#35): the monitored-project list + active
// selection. Lives at the composition layer (a `src/` root file, NOT inside a slice)
// — App.vue and the slice views read it without prop-drilling. Module-level singleton
// `ref`s (mirroring useAppView.ts / the review slice's useReviewStore pattern).
import { ref } from "vue";
import { invoke } from "./api";
import type { AppConfig, Project } from "./config/types";

const activeProjectId = ref<string>("");
const projects = ref<Project[]>([]);

// Persist the active selection to the backend. Kept as a local `invoke` wrapper
// (rather than importing config/api.ts) to avoid a value-import cycle and a hard
// dependency on a `setActiveProject` export that another slice owns. The `set_active_project`
// command name + `{ projectId }` arg shape mirror the backend command registration.
function persistActive(projectId: string): Promise<void> {
  return invoke<void>("set_active_project", { projectId });
}

export function useProjects() {
  return {
    activeProjectId,
    projects,
    setActive: async (id: string) => {
      activeProjectId.value = id;
      await persistActive(id);
    },
    hydrate: (cfg: AppConfig) => {
      projects.value = cfg.projects;
      activeProjectId.value = cfg.activeProjectId;
    },
  };
}
