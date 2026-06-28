// Shared per-project field defaults (#35). Single-sourced here so the onboarding
// wizard's initial draft (OnboardingWizard.vue) and a freshly added project
// (ProjectsManager's `newProject()`) can't drift: both seed from these values.
// Mirrors the backend single-project defaults (AppConfig::default in
// config/model.rs) so an added/onboarded project is immediately valid except for
// the user-supplied repo/repoRoot.
import type { SourceKind } from "../types";
import type { NotificationChannel, OutboxConfig, Project } from "./types";
import {
  DEFAULT_MESSAGING_INTEGRATION as GENERATED_DEFAULT_MESSAGING_INTEGRATION,
  DEFAULT_MESSAGING_SETTINGS as GENERATED_DEFAULT_MESSAGING_SETTINGS,
  DEFAULT_NOTIFICATION_SETTINGS as GENERATED_DEFAULT_NOTIFICATION_SETTINGS,
} from "./types.generated";

// The fixed id the wizard gives the first project; mirrors the backend migration's
// fixed id for symmetry. ProjectsManager mints a fresh uuid per added project, so
// only the onboarding seed uses this.
export const DEFAULT_PROJECT_ID = "default";

// Global outbox worker policy default (AB#1182): mirrors the backend `OutboxConfig::default`
// (`DEFAULT_NOTIFICATION_TTL_SECS` = 7200 = 2h). There's no settings-panel control for it yet, so
// the SettingsView draft just carries the loaded value through a save round-trip and a fresh
// onboarding config seeds this. Single-sourced here so the two AppConfig literals can't drift.
export const DEFAULT_OUTBOX_CONFIG: OutboxConfig = {
  notificationTtlSecs: 2 * 60 * 60,
};

export const DEFAULT_NOTIFICATION_SETTINGS = GENERATED_DEFAULT_NOTIFICATION_SETTINGS;
export const DEFAULT_MESSAGING_SETTINGS = GENERATED_DEFAULT_MESSAGING_SETTINGS;
export const DEFAULT_MESSAGING_INTEGRATION = GENERATED_DEFAULT_MESSAGING_INTEGRATION;

export const DEFAULT_NOTIFICATION_CHANNEL: NotificationChannel = {
  ...DEFAULT_NOTIFICATION_SETTINGS.channels[0],
};

// Per-project field defaults sans identity (`id`/`name`): callers supply those.
// ProjectsManager spreads this with `{ id: crypto.randomUUID(), name: "新项目" }`;
// OnboardingWizard seeds its draft with `{ id: DEFAULT_PROJECT_ID, name: "默认项目" }`.
export const NEW_PROJECT_DEFAULTS: Omit<Project, "id" | "name"> = {
  enabled: true,
  repo: "",
  repoRoot: "",
  pollIntervalSecs: 120,
  authors: [],
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
