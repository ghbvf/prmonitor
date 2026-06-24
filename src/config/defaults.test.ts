// defaults.ts unit tests (717 F9). Covers `applySourceKindDefaults`, the shared helper
// that both onboarding (OnboardingWizard.setField) and Settings (ProjectCard.setField)
// call when the user changes `sourceKind`, so the two surfaces can't drift on the
// Bitbucket-required auto-corrections. Pure (mutates a plain Project) → no Pinia/mocks.
import { describe, expect, it } from "vitest";
import type { Project } from "./types";
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
    reviewLabel: "pr-status/needs-review-again",
    checkLabel: "pr-status/needs-check-fix",
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
    autoReview: false,
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
