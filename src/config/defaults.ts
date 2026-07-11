// Shared per-project field defaults (#35). Single-sourced here so the onboarding
// wizard's initial draft (OnboardingWizard.vue) and a freshly added project
// (ProjectsManager's `newProject()`) can't drift: both seed from these values.
// Mirrors the backend single-project defaults (AppConfig::default in
// config/model.rs) so an added/onboarded project is immediately valid except for
// the user-supplied repo/repoRoot.
import type { SourceKind } from "../types";
import type {
  CliToolsConfig,
  NotificationChannel,
  OutboxConfig,
  Project,
  ReviewLifecycleNotificationConfig,
} from "./types";
import {
  DEFAULT_CLI_TOOLS_CONFIG as GENERATED_DEFAULT_CLI_TOOLS_CONFIG,
  DEFAULT_MESSAGING_INTEGRATION as GENERATED_DEFAULT_MESSAGING_INTEGRATION,
  DEFAULT_MESSAGING_SETTINGS as GENERATED_DEFAULT_MESSAGING_SETTINGS,
  DEFAULT_NOTIFICATION_SETTINGS as GENERATED_DEFAULT_NOTIFICATION_SETTINGS,
  DEFAULT_REVIEW_LIFECYCLE_NOTIFICATION_CONFIG as GENERATED_DEFAULT_REVIEW_LIFECYCLE_NOTIFICATION_CONFIG,
  DEFAULT_PROJECT as GENERATED_DEFAULT_PROJECT,
} from "./types.generated";

// The fixed id the wizard gives the first project; mirrors the backend migration's
// fixed id for symmetry. ProjectsManager mints a fresh uuid per added project, so
// only the onboarding seed uses this.
export const DEFAULT_PROJECT_ID = "default";

export const DEFAULT_CLI_TOOLS_CONFIG: CliToolsConfig =
  GENERATED_DEFAULT_CLI_TOOLS_CONFIG;

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
export const DEFAULT_REVIEW_LIFECYCLE_NOTIFICATION_CONFIG: ReviewLifecycleNotificationConfig =
  GENERATED_DEFAULT_REVIEW_LIFECYCLE_NOTIFICATION_CONFIG;

export const DEFAULT_NOTIFICATION_CHANNEL: NotificationChannel = {
  ...DEFAULT_NOTIFICATION_SETTINGS.channels[0],
};

// Per-project field defaults sans identity (`id`/`name`): callers supply those.
// ProjectsManager spreads this with `{ id: crypto.randomUUID(), name: "新项目" }`;
// OnboardingWizard seeds its draft with `{ id: DEFAULT_PROJECT_ID, name: "默认项目" }`.
const { id: _defaultId, name: _defaultName, ...generatedProjectDefaults } =
  GENERATED_DEFAULT_PROJECT;

export const NEW_PROJECT_DEFAULTS: Omit<Project, "id" | "name"> = {
  ...generatedProjectDefaults,
  // A newly added project must ask for its repository instead of inheriting the backend's
  // historical first-project seed. Every other field stays Rust-generated.
  repo: "",
  authors: [...generatedProjectDefaults.authors],
};

export function hydrateProjectDraft(draft: Project, project: Project): void {
  Object.assign(draft, project, { authors: [...project.authors] });
}

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
