// Pure draft mutations for the projects list (config slice). Extracted from
// ProjectsManager so the `activeProjectId` invariant — the crux of add/delete — is
// unit-testable without a Vue component harness. The invariant mirrors the backend
// `validate()` (config/model.rs): a NON-EMPTY `projects` requires `activeProjectId` to
// name an existing project; an EMPTY list pairs with an empty `activeProjectId` (the
// cleared / first-launch state). Locked by projectOps.test.ts so the save contract
// can't silently regress (Medium: a runtime-shape guard over a cross-end invariant).
import { NEW_PROJECT_DEFAULTS } from "./defaults";
import type { AppConfig, Project } from "./types";

// A fresh project: identity (id/name) minted here, the rest from the shared
// NEW_PROJECT_DEFAULTS (single-sourced with the onboarding wizard in defaults.ts) so
// the two seed paths can't drift.
export function makeProject(): Project {
  return { ...NEW_PROJECT_DEFAULTS, id: crypto.randomUUID(), name: "新项目" };
}

// Append a fresh project and return it. When the list was empty (activeProjectId ""),
// the new project becomes active so the saved config keeps the backend invariant
// (non-empty projects ⇒ activeProjectId names an existing project).
export function addProjectToDraft(draft: AppConfig): Project {
  const p = makeProject();
  draft.projects.push(p);
  if (draft.activeProjectId === "") draft.activeProjectId = p.id;
  return p;
}

// Remove the project with `id`. Returns false (no-op) if not found. When the removed
// project was the active one, fall back to the new first project — or "" when the list
// is now empty (the cleared state the backend accepts).
export function deleteProjectFromDraft(draft: AppConfig, id: string): boolean {
  const idx = draft.projects.findIndex((x) => x.id === id);
  if (idx === -1) return false;
  draft.projects.splice(idx, 1);
  if (draft.activeProjectId === id) {
    draft.activeProjectId = draft.projects[0]?.id ?? "";
  }
  return true;
}
