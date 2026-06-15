// fields.ts unit tests (#34). The field-definition source drives BOTH the Settings
// grouped form and the onboarding wizard, plus two pure helpers — `validateStep`
// (per-step frontend gate) and `errorToStep` (route a backend AppError back to the
// step that owns the offending field). Pure functions → no Pinia/mocks needed.
import { describe, expect, it } from "vitest";
import type { AppConfig } from "./types";
import {
  GROUPS,
  STEPS,
  validateStep,
  errorToStep,
  type StepId,
} from "./fields";

// A fully-valid draft; each test perturbs one field to assert its step's gate.
function validDraft(): AppConfig {
  return {
    repo: "ghbvf/gocell",
    repoRoot: "/abs/path",
    pollIntervalSecs: 120,
    authors: [],
    reviewLabel: "pr-status/needs-review-again",
    checkLabel: "pr-status/needs-check-fix",
    skillRelPath: ".codex/skills/pr-review/SKILL.md",
    prCooldownSeconds: 1800,
    sourceKind: "github",
    engineKind: "codex",
  };
}

describe("GROUPS", () => {
  it("covers all 10 AppConfig keys exactly once across groups", () => {
    const keys = GROUPS.flatMap((g) => g.fields.map((f) => f.key)).sort();
    const expected = Object.keys(validDraft()).sort();
    expect(keys).toEqual(expected);
  });

  it("marks the engine group fields read-only (sourceKind/engineKind reserved #11)", () => {
    const engine = GROUPS.find((g) => g.id === "engine");
    expect(engine).toBeDefined();
    expect(engine!.fields.every((f) => f.readonly)).toBe(true);
    expect(engine!.fields.map((f) => f.key).sort()).toEqual([
      "engineKind",
      "sourceKind",
    ]);
  });
});

describe("validateStep — repo", () => {
  it("accepts owner/name", () => {
    expect(validateStep("repo", validDraft())).toBeNull();
  });
  it.each(["ghbvf", "a/b/c", "", "owner /name", "owner/"])(
    "rejects %j",
    (repo) => {
      expect(validateStep("repo", { ...validDraft(), repo })).toBeTruthy();
    },
  );
});

describe("validateStep — repoRoot", () => {
  it("accepts a non-empty path", () => {
    expect(validateStep("repoRoot", validDraft())).toBeNull();
  });
  it.each(["", "   "])("rejects empty/whitespace-only %j", (repoRoot) => {
    expect(validateStep("repoRoot", { ...validDraft(), repoRoot })).toBeTruthy();
  });
});

describe("validateStep — skill", () => {
  it("accepts a relative path", () => {
    expect(validateStep("skill", validDraft())).toBeNull();
  });
  it("rejects empty", () => {
    expect(
      validateStep("skill", { ...validDraft(), skillRelPath: "" }),
    ).toBeTruthy();
  });
  it("rejects an absolute path", () => {
    expect(
      validateStep("skill", { ...validDraft(), skillRelPath: "/etc/x" }),
    ).toBeTruthy();
  });
});

describe("validateStep — autoReview (intervals)", () => {
  it("accepts positive intervals", () => {
    expect(validateStep("autoReview", validDraft())).toBeNull();
  });
  it.each([
    { pollIntervalSecs: 0 },
    { prCooldownSeconds: 0 },
    { pollIntervalSecs: -1 },
    { prCooldownSeconds: -5 },
    { pollIntervalSecs: NaN },
    { prCooldownSeconds: NaN },
  ])("rejects non-positive / NaN %o", (patch) => {
    expect(validateStep("autoReview", { ...validDraft(), ...patch })).toBeTruthy();
  });
});

describe("validateStep — source/done are confirm-only", () => {
  it.each(["source", "done"] as StepId[])("accepts %s", (step) => {
    expect(validateStep(step, validDraft())).toBeNull();
  });
});

describe("STEPS ordering", () => {
  it("is the wizard sequence repo→repoRoot→skill→source→autoReview→done", () => {
    expect(STEPS).toEqual([
      "repo",
      "repoRoot",
      "skill",
      "source",
      "autoReview",
      "done",
    ]);
  });
});

describe("errorToStep — routes backend AppError messages", () => {
  it("repoRoot message → repoRoot step", () => {
    expect(errorToStep("repoRoot 必须是存在的绝对目录路径: ")).toBe("repoRoot");
  });
  it("skill message → skill step", () => {
    expect(errorToStep("skill 路径不存在: /x/y")).toBe("skill");
    expect(errorToStep("skillRelPath 必须是相对路径: /x")).toBe("skill");
  });
  it("path-escape message (names both fields) → skill step, not repoRoot", () => {
    expect(errorToStep("skillRelPath 不能逃逸 repoRoot: ../x")).toBe("skill");
  });
  it("interval messages → autoReview step", () => {
    expect(errorToStep("pollIntervalSecs 必须大于 0")).toBe("autoReview");
    expect(errorToStep("prCooldownSeconds 必须大于 0")).toBe("autoReview");
  });
  it("unknown message → null (caller falls back to the done step)", () => {
    expect(errorToStep("某种未知错误")).toBeNull();
  });
});
