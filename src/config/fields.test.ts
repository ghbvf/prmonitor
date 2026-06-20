// fields.ts unit tests (#34). The field-definition source drives BOTH the Settings
// grouped form and the onboarding wizard, plus two pure helpers — `validateStep`
// (per-step frontend gate) and `errorToStep` (route a backend AppError back to the
// step that owns the offending field). Pure functions → no Pinia/mocks needed.
//
// Multi-project (#35): coverage is split into PROJECT_GROUPS (per-project `Project`
// keys) and GLOBAL_GROUPS (global webhook `AppConfig` keys).
import { describe, expect, it } from "vitest";
import type { AppConfig, Project } from "./types";
import { UPDATE_MODES, pollingEnabledForMode, manualPullAllowedForMode } from "../types";
import {
  PROJECT_GROUPS,
  GLOBAL_GROUPS,
  STEPS,
  STEP_FIELDS,
  visibleStepFields,
  validateStep,
  errorToStep,
} from "./fields";

// A fully-valid single project; each test perturbs one field to assert its step's gate.
function validProject(): Project {
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
    skillRelPath: ".codex/skills/pr-review/SKILL.md",
    prCooldownSeconds: 1800,
    updateMode: "webhook-only",
    sourceKind: "github",
    azureOrg: "",
    azureProject: "",
    engineKind: "codex",
    autoReview: false,
  };
}

// The global (non-per-project) AppConfig keys. Only the webhook/shell fields are
// driven by GLOBAL_GROUPS; `projects`/`activeProjectId` are structural and managed
// by the project list/selector UI, not a field group.
const GLOBAL_FORM_KEYS: (keyof AppConfig)[] = [
  "webhookEnabled",
  "webhookPort",
  "webhookSecret",
  "cloudflaredBin",
  "webhookTunnelMode",
  "webhookTunnelCommand",
  "webhookPublicUrl",
];

// `Project` identity fields managed by the project list/selector UI (not a field
// group), so PROJECT_GROUPS deliberately omits them.
const PROJECT_IDENTITY_KEYS: (keyof Project)[] = ["id", "name", "enabled"];

describe("PROJECT_GROUPS", () => {
  it("covers every Project key (except identity fields) exactly once across groups", () => {
    const keys = PROJECT_GROUPS.flatMap((g) => g.fields.map((f) => f.key)).sort();
    const expected = (Object.keys(validProject()) as (keyof Project)[])
      .filter((k) => !PROJECT_IDENTITY_KEYS.includes(k))
      .sort();
    expect(keys).toEqual(expected);
  });

  it("autoReview is a checkbox in the polling group", () => {
    const f = PROJECT_GROUPS.flatMap((g) => g.fields).find((f) => f.key === "autoReview");
    expect(f?.kind).toBe("checkbox");
  });

  it("engine group: sourceKind selectable (818), engineKind read-only (#11)", () => {
    const engine = PROJECT_GROUPS.find((g) => g.id === "engine");
    expect(engine).toBeDefined();
    const byKey = new Map(engine!.fields.map((f) => [f.key, f]));
    // sourceKind is now an editable select offering github + azure (818).
    const source = byKey.get("sourceKind");
    expect(source?.kind).toBe("select");
    expect(source?.readonly).toBeFalsy();
    expect(source?.options).toEqual(["github", "azure"]);
    // engineKind stays single-arm read-only (widening tracked by #11).
    expect(byKey.get("engineKind")?.readonly).toBe(true);
    // Azure org/project fields live in the engine group too (818).
    expect(byKey.get("azureOrg")?.kind).toBe("text");
    expect(byKey.get("azureProject")?.kind).toBe("text");
  });

  it("updateMode is a select single-sourced from UPDATE_MODES (818)", () => {
    const f = PROJECT_GROUPS.flatMap((g) => g.fields).find((f) => f.key === "updateMode");
    expect(f?.kind).toBe("select");
    expect(f?.options).toEqual(UPDATE_MODES);
    // Every wire value has a display label (the Chinese mode names).
    for (const m of UPDATE_MODES) {
      expect(f?.optionLabels?.[m]).toBeTruthy();
    }
  });

  it("azure fields are visibleWhen sourceKind === azure (818 F14)", () => {
    const byKey = new Map(
      PROJECT_GROUPS.flatMap((g) => g.fields).map((f) => [f.key, f]),
    );
    const azureP = { ...validProject(), sourceKind: "azure" as const };
    const githubP = { ...validProject(), sourceKind: "github" as const };
    for (const key of ["azureOrg", "azureProject"] as const) {
      const f = byKey.get(key);
      expect(f?.visibleWhen).toBeTypeOf("function");
      expect(f!.visibleWhen!(azureP)).toBe(true);
      expect(f!.visibleWhen!(githubP)).toBe(false);
    }
  });

  it("non-azure project fields have no visibleWhen (always visible)", () => {
    // sourceKind, repo, updateMode, etc. must NOT be conditionally hidden — only the
    // azure-specific fields carry a predicate.
    const conditional = PROJECT_GROUPS.flatMap((g) => g.fields)
      .filter((f) => f.visibleWhen)
      .map((f) => f.key)
      .sort();
    expect(conditional).toEqual(["azureOrg", "azureProject"]);
  });
});

describe("pollingEnabledForMode (818)", () => {
  it("enables polling only for pull-only / hybrid", () => {
    expect(pollingEnabledForMode("webhook-only")).toBe(false);
    expect(pollingEnabledForMode("pull-only")).toBe(true);
    expect(pollingEnabledForMode("hybrid")).toBe(true);
    expect(pollingEnabledForMode("manual")).toBe(false);
  });
});

describe("manualPullAllowedForMode (818 F7)", () => {
  it("allows the one-shot pull for everything except webhook-only", () => {
    // Distinct from pollingEnabledForMode: manual supports a backend one-shot pull.
    expect(manualPullAllowedForMode("webhook-only")).toBe(false);
    expect(manualPullAllowedForMode("pull-only")).toBe(true);
    expect(manualPullAllowedForMode("hybrid")).toBe(true);
    expect(manualPullAllowedForMode("manual")).toBe(true);
  });
});

describe("GLOBAL_GROUPS", () => {
  it("covers all global (webhook) AppConfig keys exactly once across groups", () => {
    const keys = GLOBAL_GROUPS.flatMap((g) => g.fields.map((f) => f.key)).sort();
    expect(keys).toEqual([...GLOBAL_FORM_KEYS].sort());
  });

  it("masks the webhook secret field", () => {
    const f = GLOBAL_GROUPS.flatMap((g) => g.fields).find((f) => f.key === "webhookSecret");
    expect(f?.secret).toBe(true);
  });
});

describe("validateStep — repo", () => {
  it("accepts owner/name (github source)", () => {
    expect(validateStep("repo", validProject())).toBeNull();
  });
  it.each(["ghbvf", "a/b/c", "", "owner /name", "owner/"])(
    "rejects %j (github source)",
    (repo) => {
      expect(validateStep("repo", { ...validProject(), repo })).toBeTruthy();
    },
  );
  // Azure source (818 F4): repo is a BARE name (no slash); org/project come from the
  // azureOrg/azureProject fields, so a slash here is wrong and a bare name is valid.
  it("accepts a bare name for an azure source", () => {
    expect(
      validateStep("repo", { ...validProject(), sourceKind: "azure", repo: "gocell" }),
    ).toBeNull();
  });
  it.each(["org/repo", "shengming0923/gocell", "", "  "])(
    "rejects %j for an azure source (slash or empty)",
    (repo) => {
      expect(
        validateStep("repo", { ...validProject(), sourceKind: "azure", repo }),
      ).toBeTruthy();
    },
  );
});

describe("validateStep — repoRoot", () => {
  it("accepts a non-empty path", () => {
    expect(validateStep("repoRoot", validProject())).toBeNull();
  });
  it.each(["", "   "])("rejects empty/whitespace-only %j", (repoRoot) => {
    expect(validateStep("repoRoot", { ...validProject(), repoRoot })).toBeTruthy();
  });
});

describe("validateStep — skill", () => {
  it("accepts a relative path", () => {
    expect(validateStep("skill", validProject())).toBeNull();
  });
  it("rejects empty", () => {
    expect(
      validateStep("skill", { ...validProject(), skillRelPath: "" }),
    ).toBeTruthy();
  });
  it("rejects an absolute path", () => {
    expect(
      validateStep("skill", { ...validProject(), skillRelPath: "/etc/x" }),
    ).toBeTruthy();
  });
});

describe("validateStep — autoReview (intervals)", () => {
  it("accepts positive intervals", () => {
    expect(validateStep("autoReview", validProject())).toBeNull();
  });
  it.each([
    { pollIntervalSecs: 0 },
    { prCooldownSeconds: 0 },
    { pollIntervalSecs: -1 },
    { prCooldownSeconds: -5 },
    { pollIntervalSecs: NaN },
    { prCooldownSeconds: NaN },
  ])("rejects non-positive / NaN %o", (patch) => {
    expect(validateStep("autoReview", { ...validProject(), ...patch })).toBeTruthy();
  });
  it.each([
    { reviewLabel: "" },
    { reviewLabel: "   " },
    { checkLabel: "" },
    { checkLabel: "  " },
  ])("rejects blank trigger label %o", (patch) => {
    expect(validateStep("autoReview", { ...validProject(), ...patch })).toBeTruthy();
  });
});

describe("validateStep — done is confirm-only", () => {
  it("accepts done", () => {
    expect(validateStep("done", validProject())).toBeNull();
  });
});

describe("validateStep — source (818 F2)", () => {
  it("accepts a github source (no azure fields needed)", () => {
    // validProject() is a github source with empty azure fields — must pass.
    expect(validateStep("source", validProject())).toBeNull();
  });
  it("accepts an azure source with both org + project filled", () => {
    expect(
      validateStep("source", {
        ...validProject(),
        sourceKind: "azure",
        azureOrg: "shengming0923",
        azureProject: "gocell",
      }),
    ).toBeNull();
  });
  it.each([
    { azureOrg: "", azureProject: "gocell" },
    { azureOrg: "  ", azureProject: "gocell" },
    { azureOrg: "shengming0923", azureProject: "" },
    { azureOrg: "shengming0923", azureProject: "   " },
    { azureOrg: "", azureProject: "" },
  ])("rejects an azure source with a blank azure field %o", (patch) => {
    const err = validateStep("source", {
      ...validProject(),
      sourceKind: "azure",
      ...patch,
    });
    expect(err).toBeTruthy();
    // Error must start with the offending field token so errorToStep can route it.
    expect(err!.startsWith("azureOrg") || err!.startsWith("azureProject")).toBe(true);
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

describe("STEP_FIELDS — onboarding wizard step → field wiring (818 F2/F3)", () => {
  it("the source step owns sourceKind + the azure fields (818 F2)", () => {
    expect(STEP_FIELDS.source).toContain("sourceKind");
    expect(STEP_FIELDS.source).toContain("azureOrg");
    expect(STEP_FIELDS.source).toContain("azureProject");
  });

  it("the autoReview step surfaces updateMode (818 F3)", () => {
    expect(STEP_FIELDS.autoReview).toContain("updateMode");
  });

  it("every STEP_FIELDS key is a real PROJECT_GROUPS field", () => {
    const grouped = new Set(PROJECT_GROUPS.flatMap((g) => g.fields).map((f) => f.key));
    for (const keys of Object.values(STEP_FIELDS)) {
      for (const k of keys) expect(grouped.has(k)).toBe(true);
    }
  });
});

describe("visibleStepFields — conditional fields per step (818 F2)", () => {
  it("hides the azure fields on the source step for a github source", () => {
    const keys = visibleStepFields("source", validProject()).map((f) => f.key);
    expect(keys).toContain("sourceKind");
    expect(keys).not.toContain("azureOrg");
    expect(keys).not.toContain("azureProject");
  });

  it("shows the azure fields on the source step for an azure source", () => {
    const draft = { ...validProject(), sourceKind: "azure" as const };
    const keys = visibleStepFields("source", draft).map((f) => f.key);
    expect(keys).toEqual(["sourceKind", "azureOrg", "azureProject"]);
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
  // Azure fields (818 F2): the wizard `source` step NOW owns azureOrg/azureProject (it
  // renders + validates them for an azure source), so a backend rejection routes back to
  // the source step — not null. Both tokens map to "source".
  it("azure messages → source step (818 F2, owned by the source step)", () => {
    expect(errorToStep("azureOrg 不能为空（azure 源需填组织名）")).toBe("source");
    expect(errorToStep("azureProject 不能为空（azure 源需填项目名）")).toBe("source");
  });
});
