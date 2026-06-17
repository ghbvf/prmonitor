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
    autoReview: true,
    webhookEnabled: false,
    webhookPort: 8787,
    webhookSecret: "",
    cloudflaredBin: "cloudflared",
    webhookTunnelMode: "quick",
    webhookTunnelCommand: "",
    webhookPublicUrl: "",
  };
}

describe("GROUPS", () => {
  it("covers all 18 AppConfig keys exactly once across groups", () => {
    const keys = GROUPS.flatMap((g) => g.fields.map((f) => f.key)).sort();
    const expected = Object.keys(validDraft()).sort();
    expect(keys).toEqual(expected);
  });

  it("autoReview is a checkbox in the polling group", () => {
    const f = GROUPS.flatMap((g) => g.fields).find((f) => f.key === "autoReview");
    expect(f?.kind).toBe("checkbox");
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
  it.each([
    { reviewLabel: "" },
    { reviewLabel: "   " },
    { checkLabel: "" },
    { checkLabel: "  " },
  ])("rejects blank trigger label %o", (patch) => {
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
  it("repo message → repo step (checked after repoRoot)", () => {
    expect(errorToStep("repo 必须是 owner/name 格式: not-a-repo")).toBe("repo");
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
  it("label messages → autoReview step", () => {
    expect(errorToStep("reviewLabel 不能为空")).toBe("autoReview");
    expect(errorToStep("checkLabel 不能为空")).toBe("autoReview");
  });
  // Prefix-match (not substring) so an interpolated VALUE can't hijack routing.
  it("does not let an interpolated value hijack the route", () => {
    // A repoRoot path that contains "skill" must still route to repoRoot.
    expect(errorToStep("repoRoot 必须是存在的绝对目录路径: /home/me/skills")).toBe(
      "repoRoot",
    );
    // A repo value that contains "repoRoot" must still route to repo, not repoRoot.
    expect(errorToStep("repo 必须是 owner/name 格式: repoRoot/x")).toBe("repo");
  });
  it("unknown message → null (caller falls back to the done step)", () => {
    expect(errorToStep("某种未知错误")).toBeNull();
    // A non-field store error (no field-name prefix) is also unrouted.
    expect(errorToStep("打开配置存储失败: io")).toBeNull();
  });
  // Webhook fields are Settings-only (no onboarding step owns them), so their
  // validate() messages are intentionally NOT wizard-routed — they fall through to
  // null (→ done). SettingsView shows these backend errors directly. This case locks
  // that intent so a future reader doesn't mistake the missing branch for a gap.
  it("webhook messages → null (settings-only, not wizard-routed)", () => {
    expect(errorToStep("webhookSecret 不能为空（启用 webhook 时必填）")).toBeNull();
    expect(errorToStep("webhookPort 必须大于 0")).toBeNull();
    // The tunnel-mode fields (#9) are likewise Settings-only: command mode's
    // missing-command validate() error must also fall through to null, not route
    // to a wizard step. Message mirrors the backend's exact wording
    // (config/model.rs validate(), which starts with the `webhookTunnelCommand`
    // routing-field token).
    expect(
      errorToStep("webhookTunnelCommand 不能为空（command 模式需填隧道命令，可用 {port} 占位）"),
    ).toBeNull();
  });
});
