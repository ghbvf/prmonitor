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
      activeProjectId.value = id;
      // Centralized with the other config commands (see config/api.ts). projects.ts is
      // a composition-root module (not a slice), so importing config/api is allowed —
      // and config/api never imports projects.ts, so there's no value-import cycle.
      await setActiveProject(id);
    },
    hydrate: (cfg: AppConfig) => {
      projects.value = cfg.projects;
      activeProjectId.value = cfg.activeProjectId;
    },
  };
}
