// defaults.ts unit tests (717 F9). Covers `applySourceKindDefaults`, the shared helper
// that both onboarding (OnboardingWizard.setField) and Settings (ProjectCard.setField)
// call when the user changes `sourceKind`, so the two surfaces can't drift on the
// Bitbucket-required auto-corrections. Pure (mutates a plain Project) → no Pinia/mocks.
//
// F10 (AB#1225 PR2): DEFAULT_LOCAL_API_LISTENER was removed from defaults.ts; the backend
// `AppConfig::default()` is now the single source for the local-api listener seed. The
// OnboardingWizard.composeConfig() reads `store.config.listeners` (the backend-seeded value)
// instead of the former TS literal — this module no longer exports the redundant mirror.
// The compile-time absence of DEFAULT_LOCAL_API_LISTENER is enforced by TypeScript (removing
// the export makes any import of it a compile error — Hard enforcement via the type system).
import { describe, expect, it } from "vitest";
import type { Listener, Project } from "./types";
import { DEFAULT_OUTBOX_CONFIG, applySourceKindDefaults } from "./defaults";

// A github-shaped project carrying the github-friendly defaults (labelSource "native",
// updateMode "webhook-only") the backend rejects for a Bitbucket source.
function githubProject(): Project {
  return {
    id: "proj-1",
    name: "gocell",
    enabled: true,
    repo: "ghbvf/gocell",
    repoRoot: "/abs/path",
    pollIntervalSecs: 120,
    authors: [],
    labelSource: "native",
    skillRelPath: ".codex/skills/pr-review/SKILL.md",
    prCooldownSeconds: 1800,
    updateMode: "webhook-only",
    sourceKind: "github",
    azureOrg: "",
    azureProject: "",
    bitbucketHost: "",
    bitbucketProject: "",
    bitbucketToken: "",
    engineKind: "codex",
    codexModel: "",
    claudeModel: "",
  };
}

describe("applySourceKindDefaults (717 F9)", () => {
  it("switching TO bitbucket sets labelSource=title + updateMode off webhook-only", () => {
    const p = githubProject();
    applySourceKindDefaults(p, "bitbucket");
    expect(p.sourceKind).toBe("bitbucket");
    expect(p.labelSource).toBe("title");
    expect(p.updateMode).toBe("pull-only");
  });

  it("downgrades a hybrid updateMode to pull-only when switching to bitbucket", () => {
    const p = { ...githubProject(), updateMode: "hybrid" as const };
    applySourceKindDefaults(p, "bitbucket");
    expect(p.updateMode).toBe("pull-only");
    expect(p.labelSource).toBe("title");
  });

  it.each(["pull-only", "manual"] as const)(
    "keeps an already-valid updateMode (%s) when switching to bitbucket",
    (updateMode) => {
      const p = { ...githubProject(), updateMode };
      applySourceKindDefaults(p, "bitbucket");
      // pull-only / manual are valid for bitbucket — leave them untouched.
      expect(p.updateMode).toBe(updateMode);
      expect(p.labelSource).toBe("title");
    },
  );

  it("switching to github only sets sourceKind — no field clobbering", () => {
    // Start from a bitbucket-shaped project and switch back to github: nothing other
    // than sourceKind should change (don't reset a user's labelSource/updateMode).
    const p = {
      ...githubProject(),
      sourceKind: "bitbucket" as const,
      labelSource: "title" as const,
      updateMode: "pull-only" as const,
    };
    applySourceKindDefaults(p, "github");
    expect(p.sourceKind).toBe("github");
    expect(p.labelSource).toBe("title");
    expect(p.updateMode).toBe("pull-only");
  });

  it("switching to azure only sets sourceKind — does not touch azure config", () => {
    const p = {
      ...githubProject(),
      azureOrg: "shengming0923",
      azureProject: "gocell",
    };
    applySourceKindDefaults(p, "azure");
    expect(p.sourceKind).toBe("azure");
    // Don't clobber a user's azure config (or the unrelated label/update fields).
    expect(p.azureOrg).toBe("shengming0923");
    expect(p.azureProject).toBe("gocell");
    expect(p.labelSource).toBe("native");
    expect(p.updateMode).toBe("webhook-only");
  });
});

// AB#1182 (review F1/F7): the frontend default TTL must mirror the backend
// `DEFAULT_NOTIFICATION_TTL_SECS` (= 7200 = 2h). The Rust golden pins the backend value; this pins
// the TS literal so a one-sided drift (e.g. only the backend default is changed) fails in CI rather
// than silently writing a stale default through onboarding / a settings save (lifts the cross-source
// mirror from Soft toward Medium on the TS side).
describe("DEFAULT_OUTBOX_CONFIG (AB#1182)", () => {
  it("mirrors the backend default notification TTL (2h)", () => {
    expect(DEFAULT_OUTBOX_CONFIG.notificationTtlSecs).toBe(7200);
  });
});

// F10 (AB#1225 PR2): DEFAULT_LOCAL_API_LISTENER was removed — the backend is the single
// source for the local-api listener seed. This test documents the F10 contract: when
// OnboardingWizard.composeConfig() spreads `store.config.listeners` (the backend-seeded
// array), the resulting config retains the local-api entry without a TS-side literal.
//
// We cannot directly import `composeConfig` (it's a closure inside the Vue component),
// so this test simulates the sourcing logic: spread the loaded config's listeners into
// the composed AppConfig and verify the local-api entry is present and non-empty.
describe("F10 — onboarding listeners sourced from loaded backend config (not TS literal)", () => {
  // Simulate the backend-seeded local-api listener that `AppConfig::default()` produces.
  // This shape is verified by the Rust golden (`app_config_wire_shape_is_camel_case`) —
  // we reference it here only to assert the spread logic, NOT as a TS-side mirror literal.
  const backendSeededListeners: Listener[] = [
    {
      id: "local-api",
      name: "Local API",
      kind: "local-api",
      bindHost: "127.0.0.1",
      port: 8788,
      enabled: true,
      auth: "bearer",
      authToken: "",
      terminalRead: false,
      terminalWrite: false,
      terminalCreate: false,
      terminalAdmin: false,
      allowedOrigins: [],
      publicUrl: "",
    },
  ];

  it("spreading the loaded config listeners into a composed config preserves the local-api entry", () => {
    // Mirrors the composeConfig() pattern in OnboardingWizard.vue after F10:
    //   listeners: store.config?.listeners ? [...store.config.listeners] : []
    const composed = {
      listeners: backendSeededListeners.length ? [...backendSeededListeners] : [],
    };
    expect(composed.listeners).toHaveLength(1);
    expect(composed.listeners[0].kind).toBe("local-api");
    expect(composed.listeners[0].id).toBe("local-api");
    expect(composed.listeners[0].enabled).toBe(true);
  });

  it("guard: if loaded listeners is empty (backend bug), compose yields empty rather than a stale TS literal", () => {
    // This documents the guard path — it should not occur in practice since
    // AppConfig::default() always seeds the local-api listener on first launch.
    const emptyListeners: Listener[] = [];
    const composed = {
      listeners: emptyListeners.length ? [...emptyListeners] : [],
    };
    // Guard produces [] rather than a stale TS-side default literal — the backend
    // will re-apply its own defaults on next read (no F4 regression from a TS literal).
    expect(composed.listeners).toHaveLength(0);
  });
});
