// Shared per-project field defaults (#35). Single-sourced here so the onboarding
// wizard's initial draft (OnboardingWizard.vue) and a freshly added project
// (ProjectsManager's `newProject()`) can't drift: both seed from these values.
// Mirrors the backend single-project defaults (AppConfig::default in
// config/model.rs) so an added/onboarded project is immediately valid except for
// the user-supplied repo/repoRoot.
import type { Project } from "./types";

// The fixed id the wizard gives the first project; mirrors the backend migration's
// fixed id for symmetry. ProjectsManager mints a fresh uuid per added project, so
// only the onboarding seed uses this.
export const DEFAULT_PROJECT_ID = "default";

// Per-project field defaults sans identity (`id`/`name`): callers supply those.
// ProjectsManager spreads this with `{ id: crypto.randomUUID(), name: "新项目" }`;
// OnboardingWizard seeds its draft with `{ id: DEFAULT_PROJECT_ID, name: "默认项目" }`.
export const NEW_PROJECT_DEFAULTS: Omit<Project, "id" | "name"> = {
  enabled: true,
  repo: "",
  repoRoot: "",
  pollIntervalSecs: 120,
  authors: [],
  reviewLabel: "pr-status/needs-review-again",
  checkLabel: "pr-status/needs-check-fix",
  skillRelPath: ".codex/skills/pr-review/SKILL.md",
  prCooldownSeconds: 1800,
  sourceKind: "github",
  engineKind: "codex",
  autoReview: false,
};
