// Shared per-project field defaults (#35). Single-sourced here so the onboarding
// wizard's initial draft (OnboardingWizard.vue) and a freshly added project
// (ProjectsManager's `newProject()`) can't drift: both seed from these values.
// Mirrors the backend single-project defaults (AppConfig::default in
// config/model.rs) so an added/onboarded project is immediately valid except for
// the user-supplied repo/repoRoot.
import type { SourceKind } from "../types";
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
  // Label source defaults to "native" (717): use the provider's own PR labels. A
  // bitbucket source must switch this to "title" (no native labels).
  labelSource: "native",
  skillRelPath: ".codex/skills/pr-review/SKILL.md",
  prCooldownSeconds: 1800,
  // Default webhook-only (818): push-driven, no CLI poll loop — avoids the account/API
  // risk control that CLI polling can trigger. Source defaults to github with empty
  // Azure/Bitbucket fields (only used when sourceKind switches to "azure"/"bitbucket").
  updateMode: "webhook-only",
  sourceKind: "github",
  azureOrg: "",
  azureProject: "",
  bitbucketHost: "",
  bitbucketProject: "",
  bitbucketToken: "",
  engineKind: "codex",
  // Empty = each engine uses its own default model (no --model / turn model injected).
  codexModel: "",
  claudeModel: "",
  autoReview: false,
};

// Auto-correct the project fields a Bitbucket source REQUIRES (717), applied in-place
// whenever the user changes `sourceKind`. The backend `validate_project` rejects a
// Bitbucket source that keeps the github-shaped defaults (labelSource "native",
// updateMode "webhook-only"/"hybrid"), so without this the user would have to manually
// fix two more fields or hit a submit error. Shared by BOTH places sourceKind is edited
// (OnboardingWizard.setField + ProjectCard.setField) so the two surfaces can't drift.
//
// Minimal by design: only when switching TO "bitbucket", only the two offending fields,
// and only when updateMode is webhook-driven (no inbound webhook on Bitbucket). Switching
// to a non-bitbucket source is a no-op — we never clobber a user's azure config etc.
export function applySourceKindDefaults(draft: Project, sourceKind: SourceKind): void {
  draft.sourceKind = sourceKind;
  if (sourceKind !== "bitbucket") return;
  // Bitbucket Server has no native PR labels → labels MUST come from the title.
  draft.labelSource = "title";
  // Bitbucket has no inbound webhook → webhook-driven modes are invalid; downgrade to
  // pull-only. pull-only / manual are already fine, so leave them untouched.
  if (draft.updateMode === "webhook-only" || draft.updateMode === "hybrid") {
    draft.updateMode = "pull-only";
  }
}
