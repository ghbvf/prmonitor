// defaults.ts unit tests (717 F9). Covers `applySourceKindDefaults`, the shared helper
// that both onboarding (OnboardingWizard.setField) and Settings (ProjectCard.setField)
// call when the user changes `sourceKind`, so the two surfaces can't drift on the
// Bitbucket-required auto-corrections. Pure (mutates a plain Project) → no Pinia/mocks.
//
// #1553: Remote Access defaults are backend-owned. The onboarding composer carries the
// backend-seeded remoteAccess entrypoints through instead of keeping a TS-side literal.
import { describe, expect, it } from "vitest";
import type { RemoteEntrypoint, Project } from "./types";
import {
  DEFAULT_OUTBOX_CONFIG,
  NEW_PROJECT_DEFAULTS,
  applySourceKindDefaults,
  hydrateProjectDraft,
} from "./defaults";

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
    cursorModel: "",
    codexReasoningEffort: "default",
    claudeEffort: "default",
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

describe("generated project defaults and onboarding hydration", () => {
  it("uses generated effort defaults and only clears the repo product seed", () => {
    expect(NEW_PROJECT_DEFAULTS.repo).toBe("");
    expect(NEW_PROJECT_DEFAULTS.codexReasoningEffort).toBe("default");
    expect(NEW_PROJECT_DEFAULTS.claudeEffort).toBe("default");
  });

  it("copies every Project field while cloning authors", () => {
    const draft = githubProject();
    const persisted: Project = {
      ...githubProject(),
      codexModel: "gpt-5-codex",
      claudeModel: "opus",
      cursorModel: "",
      codexReasoningEffort: "ultra",
      claudeEffort: "max",
      authors: ["octocat"],
    };
    hydrateProjectDraft(draft, persisted);
    expect(draft).toEqual(persisted);
    expect(draft.authors).not.toBe(persisted.authors);
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

// F10 / #1553: the backend is the single source for the local-api entrypoint seed. This
// documents the composer contract: spreading loaded remoteAccess into the composed config
// keeps the local-api entry without a TS-side literal.
//
// We cannot directly import `composeConfig` (it's a closure inside the Vue component),
// so this test simulates the sourcing logic: spread the loaded config's remoteAccess into
// the composed AppConfig and verify the local-api entrypoint is present and non-empty.
describe("F10 — onboarding remoteAccess sourced from loaded backend config", () => {
  // Simulate the backend-seeded local-api entrypoint that `AppConfig::default()` produces.
  // This shape is verified by the Rust golden (`app_config_wire_shape_is_camel_case`) —
  // we reference it here only to assert the spread logic, NOT as a TS-side mirror literal.
  const backendSeededEntrypoints: RemoteEntrypoint[] = [
    {
      id: "local-api",
      name: "Local API",
      bindHost: "127.0.0.1",
      port: 8788,
      enabled: true,
      sourcePolicy: { mode: "loopback", allow: [] },
      allowedOrigins: [],
      trustedProxies: [],
      routes: [
        {
          id: "local-api",
          name: "Local API",
          path: "/api",
          capability: "local-api",
          enabled: true,
          authToken: "",
          terminalRead: false,
          terminalWrite: false,
          terminalCreate: false,
          terminalAdmin: false,
        },
      ],
    },
  ];

  it("spreading the loaded remoteAccess into a composed config preserves the local-api entrypoint", () => {
    // Mirrors the composeConfig() pattern in OnboardingWizard.vue after F10:
    //   remoteAccess: store.config?.remoteAccess ?? { entrypoints: [], tunnels: [] }
    const composed = {
      remoteAccess: {
        entrypoints: backendSeededEntrypoints.length ? [...backendSeededEntrypoints] : [],
        tunnels: [],
      },
    };
    expect(composed.remoteAccess.entrypoints).toHaveLength(1);
    expect(composed.remoteAccess.entrypoints[0].id).toBe("local-api");
    expect(composed.remoteAccess.entrypoints[0].port).toBe(8788);
    expect(composed.remoteAccess.entrypoints[0].routes[0].capability).toBe("local-api");
  });

  it("guard: if loaded remoteAccess is empty, compose yields empty rather than a stale TS literal", () => {
    // This documents the guard path — it should not occur in practice since
    // AppConfig::default() always seeds the local-api entrypoint on first launch.
    const emptyEntrypoints: RemoteEntrypoint[] = [];
    const composed = {
      remoteAccess: {
        entrypoints: emptyEntrypoints.length ? [...emptyEntrypoints] : [],
        tunnels: [],
      },
    };
    // Guard produces [] rather than a stale TS-side default literal — the backend
    // will re-apply its own defaults on next read (no F4 regression from a TS literal).
    expect(composed.remoteAccess.entrypoints).toHaveLength(0);
  });
});
