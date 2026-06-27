// fields.ts unit tests (#34). The field-definition source drives BOTH the Settings
// grouped form and the onboarding wizard, plus two pure helpers — `validateStep`
// (per-step frontend gate) and `errorToStep` (route a backend AppError back to the
// step that owns the offending field). Pure functions → no Pinia/mocks needed.
//
// Multi-project (#35): coverage is split into PROJECT_GROUPS (per-project `Project`
// keys) and GLOBAL_GROUPS (global webhook `AppConfig` keys).
import { describe, expect, it } from "vitest";
import type { AppConfig, Listener, Project, Tunnel } from "./types";
import { LISTENER_KINDS, LISTENER_AUTH_MODES, WEBHOOK_TUNNEL_MODES } from "./types";
import {
  UPDATE_MODES,
  ENGINE_KINDS,
  autoReviewSourceCli,
  statusBarSourceTool,
  pollingEnabledForMode,
  manualPullAllowedForMode,
} from "../types";
import {
  PROJECT_GROUPS,
  GLOBAL_GROUPS,
  LISTENER_GROUPS,
  TUNNEL_GROUPS,
  STEPS,
  STEP_FIELDS,
  listenerGroupsForKind,
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

// The global (non-per-project) AppConfig keys. Only the webhook/shell fields are
// driven by GLOBAL_GROUPS; `projects`/`activeProjectId` are structural and managed
// by the project list/selector UI, not a field group. AB#1225 PR1: `localApiPort` has been
// removed from AppConfig (port is now owned by the listeners[] entry of kind "local-api");
// only `localApiToken` remains in the global form. AB#1182: `outbox` is a nested policy
// object with no settings-panel control yet — none of these is a field group.
const GLOBAL_FORM_KEYS: (keyof AppConfig)[] = [
  "webhookEnabled",
  "webhookPort",
  "webhookSecret",
  "cloudflaredBin",
  "webhookTunnelMode",
  "webhookTunnelCommand",
  "webhookPublicUrl",
  "localApiToken",
];

// `Project` identity fields managed by the project list/selector UI (not a field
// group), so PROJECT_GROUPS deliberately omits them.
const PROJECT_IDENTITY_KEYS: (keyof Project)[] = ["id", "name", "enabled"];

// A fully-valid listener fixture (AB#1064), used to enumerate the Listener key set the
// LISTENER_GROUPS coverage test asserts against.
function validListener(): Listener {
  return {
    id: "lis-1",
    name: "Local API",
    kind: "local-api",
    bindHost: "127.0.0.1",
    port: 8788,
    enabled: false,
    auth: "none",
    authToken: "",
    terminalRead: false,
    terminalWrite: false,
    terminalCreate: false,
    terminalAdmin: false,
    allowedOrigins: [],
    publicUrl: "",
  };
}

// A fully-valid tunnel fixture (AB#1064), used to enumerate the Tunnel key set the
// TUNNEL_GROUPS coverage test asserts against.
function validTunnel(): Tunnel {
  return {
    id: "tun-1",
    name: "Web",
    mode: "quick",
    targetListenerId: "lis-1",
    command: "",
    publicUrl: "",
    enabled: false,
  };
}

// `Listener`/`Tunnel` identity fields managed by the RemoteAccess list/card header (a
// per-row name input + add/delete controls), not a field group — so LISTENER_GROUPS /
// TUNNEL_GROUPS deliberately omit them (mirrors PROJECT_IDENTITY_KEYS).
const LISTENER_IDENTITY_KEYS: (keyof Listener)[] = ["id", "name"];
const TUNNEL_IDENTITY_KEYS: (keyof Tunnel)[] = ["id", "name"];

describe("PROJECT_GROUPS", () => {
  it("covers every Project key (except identity fields) exactly once across groups", () => {
    const keys = PROJECT_GROUPS.flatMap((g) => g.fields.map((f) => f.key)).sort();
    const expected = (Object.keys(validProject()) as (keyof Project)[])
      .filter((k) => !PROJECT_IDENTITY_KEYS.includes(k))
      .sort();
    expect(keys).toEqual(expected);
  });

  it("legacy trigger controls are not project fields", () => {
    const keys = PROJECT_GROUPS.flatMap((g) => g.fields.map((f) => f.key));
    expect(keys).not.toContain("autoReview");
    expect(keys).not.toContain("reviewLabel");
    expect(keys).not.toContain("checkLabel");
  });

  it("engine group: sourceKind selectable (818), engineKind selectable (#718)", () => {
    const engine = PROJECT_GROUPS.find((g) => g.id === "engine");
    expect(engine).toBeDefined();
    const byKey = new Map(engine!.fields.map((f) => [f.key, f]));
    // sourceKind is now an editable select offering github + azure + bitbucket (818, 717).
    const source = byKey.get("sourceKind");
    expect(source?.kind).toBe("select");
    expect(source?.readonly).toBeFalsy();
    expect(source?.options).toEqual(["github", "azure", "bitbucket"]);
    // engineKind is now an editable select single-sourced from ENGINE_KINDS (#718),
    // no longer single-arm read-only. options === the as-const array (can't drift from
    // the type), with a Chinese display label for each wire value.
    const eng = byKey.get("engineKind");
    expect(eng?.kind).toBe("select");
    expect(eng?.readonly).toBeFalsy();
    expect(eng?.options).toEqual(ENGINE_KINDS);
    for (const k of ENGINE_KINDS) {
      expect(eng?.optionLabels?.[k]).toBeTruthy();
    }
    // Azure org/project fields live in the engine group too (818).
    expect(byKey.get("azureOrg")?.kind).toBe("text");
    expect(byKey.get("azureProject")?.kind).toBe("text");
    // Bitbucket host/project/token fields live in the engine group too (717); the token
    // is rendered masked.
    expect(byKey.get("bitbucketHost")?.kind).toBe("text");
    expect(byKey.get("bitbucketProject")?.kind).toBe("text");
    expect(byKey.get("bitbucketToken")?.kind).toBe("text");
    expect(byKey.get("bitbucketToken")?.secret).toBe(true);
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

  it("bitbucket fields are visibleWhen sourceKind === bitbucket (717)", () => {
    const byKey = new Map(
      PROJECT_GROUPS.flatMap((g) => g.fields).map((f) => [f.key, f]),
    );
    const bitbucketP = { ...validProject(), sourceKind: "bitbucket" as const };
    const githubP = { ...validProject(), sourceKind: "github" as const };
    for (const key of ["bitbucketHost", "bitbucketProject", "bitbucketToken"] as const) {
      const f = byKey.get(key);
      expect(f?.visibleWhen).toBeTypeOf("function");
      expect(f!.visibleWhen!(bitbucketP)).toBe(true);
      expect(f!.visibleWhen!(githubP)).toBe(false);
    }
  });

  it("labelSource is a select single-sourced from LABEL_SOURCES (717)", () => {
    const f = PROJECT_GROUPS.flatMap((g) => g.fields).find((f) => f.key === "labelSource");
    expect(f?.kind).toBe("select");
    expect(f?.options).toEqual(["native", "title"]);
    // Both wire values carry a Chinese display label.
    expect(f?.optionLabels?.native).toBeTruthy();
    expect(f?.optionLabels?.title).toBeTruthy();
  });

  it("only the azure + bitbucket source fields and the per-engine skill/model fields carry a visibleWhen predicate", () => {
    // sourceKind, repo, updateMode, labelSource, etc. must NOT be conditionally hidden —
    // only the per-source connection fields (gated on sourceKind), skillRelPath +
    // codexModel (gated on engineKind === codex, #718), and claudeModel (gated on
    // engineKind === claude) carry a predicate.
    const conditional = PROJECT_GROUPS.flatMap((g) => g.fields)
      .filter((f) => f.visibleWhen)
      .map((f) => f.key)
      .sort();
    expect(conditional).toEqual([
      "azureOrg",
      "azureProject",
      "bitbucketHost",
      "bitbucketProject",
      "bitbucketToken",
      "claudeModel",
      "codexModel",
      "skillRelPath",
    ]);
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

describe("autoReviewSourceCli (818)", () => {
  it("requires gh only for GitHub poll modes (webhook classifies from the payload)", () => {
    expect(autoReviewSourceCli("github", "webhook-only")).toBe(null);
    expect(autoReviewSourceCli("github", "pull-only")).toBe("gh");
    expect(autoReviewSourceCli("github", "hybrid")).toBe("gh");
    expect(autoReviewSourceCli("github", "manual")).toBe(null);
  });

  it("requires az for every auto-updating Azure mode incl. webhook (re-runs az), not manual", () => {
    // Azure's webhook path re-runs `az` discovery (pr/webhook.rs → discover_once), so
    // webhook-only/pull-only/hybrid all auto-depend on az; manual is on-demand only.
    expect(autoReviewSourceCli("azure", "webhook-only")).toBe("az");
    expect(autoReviewSourceCli("azure", "pull-only")).toBe("az");
    expect(autoReviewSourceCli("azure", "hybrid")).toBe("az");
    expect(autoReviewSourceCli("azure", "manual")).toBe(null);
  });

  it("never blocks on a CLI for Bitbucket (REST + token)", () => {
    for (const mode of UPDATE_MODES) {
      expect(autoReviewSourceCli("bitbucket", mode)).toBe(null);
    }
  });
});

describe("statusBarSourceTool (818)", () => {
  it("surfaces the source tool for any discovering mode; Azure always uses az", () => {
    // GitHub/Bitbucket: only webhook-only (purely push-driven, no CLI/REST) shows nothing.
    expect(statusBarSourceTool("github", "webhook-only")).toBe(null);
    expect(statusBarSourceTool("github", "pull-only")).toBe("gh");
    expect(statusBarSourceTool("github", "hybrid")).toBe("gh");
    expect(statusBarSourceTool("github", "manual")).toBe("gh");
    // Azure: webhook re-runs `az` discovery, so az is surfaced in EVERY mode incl. webhook-only.
    expect(statusBarSourceTool("azure", "webhook-only")).toBe("az");
    expect(statusBarSourceTool("azure", "pull-only")).toBe("az");
    expect(statusBarSourceTool("azure", "hybrid")).toBe("az");
    expect(statusBarSourceTool("azure", "manual")).toBe("az");
    expect(statusBarSourceTool("bitbucket", "webhook-only")).toBe(null);
    expect(statusBarSourceTool("bitbucket", "pull-only")).toBe("bitbucket");
    expect(statusBarSourceTool("bitbucket", "hybrid")).toBe("bitbucket");
    expect(statusBarSourceTool("bitbucket", "manual")).toBe("bitbucket");
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

  // AB#1043: the local API token is a bearer secret — it must render masked, same as the
  // webhook secret. Locks the `secret: true` flag against a future refactor dropping it.
  it("masks the localApiToken field", () => {
    const f = GLOBAL_GROUPS.flatMap((g) => g.fields).find((f) => f.key === "localApiToken");
    expect(f?.secret).toBe(true);
  });

  // AB#1225 PR1: localApiPort is gone from the global form (port now lives in listeners[]);
  // webhookPort keeps the default min (undefined → renderer uses 1).
  it("webhookPort keeps the default min (undefined)", () => {
    const all = GLOBAL_GROUPS.flatMap((g) => g.fields);
    expect(all.find((f) => f.key === "webhookPort")?.min).toBeUndefined();
  });

  // localApiPort must no longer appear in any GLOBAL_GROUPS field (AB#1225 PR1 removal).
  // Cast to `string` to compare against the removed key without a TS2367 error.
  it("localApiPort is absent from GLOBAL_GROUPS (port moved to listeners[])", () => {
    const all = GLOBAL_GROUPS.flatMap((g) => g.fields);
    expect(all.find((f) => (f.key as string) === "localApiPort")).toBeUndefined();
  });
});

describe("LISTENER_GROUPS (AB#1064)", () => {
  it("covers every Listener key (except identity fields) exactly once across groups", () => {
    const keys = LISTENER_GROUPS.flatMap((g) => g.fields.map((f) => f.key)).sort();
    const expected = (Object.keys(validListener()) as (keyof Listener)[])
      .filter((k) => !LISTENER_IDENTITY_KEYS.includes(k))
      .sort();
    expect(keys).toEqual(expected);
  });

  it("kind is a select single-sourced from LISTENER_KINDS (can't drift from the type)", () => {
    const f = LISTENER_GROUPS.flatMap((g) => g.fields).find((f) => f.key === "kind");
    expect(f?.kind).toBe("select");
    expect(f?.options).toEqual(LISTENER_KINDS);
  });

  it("auth is a select single-sourced from LISTENER_AUTH_MODES", () => {
    const f = LISTENER_GROUPS.flatMap((g) => g.fields).find((f) => f.key === "auth");
    expect(f?.kind).toBe("select");
    expect(f?.options).toEqual(LISTENER_AUTH_MODES);
  });

  it("masks the terminal authToken field", () => {
    const f = LISTENER_GROUPS.flatMap((g) => g.fields).find((f) => f.key === "authToken");
    expect(f?.secret).toBe(true);
  });

  it("allowedOrigins is a csv field (string[] round-trip, like authors)", () => {
    const f = LISTENER_GROUPS.flatMap((g) => g.fields).find(
      (f) => f.key === "allowedOrigins",
    );
    expect(f?.kind).toBe("csv");
    expect(f?.hint).toContain("仅允许");
  });

  it("enabled is a checkbox and port is a number", () => {
    const byKey = new Map(
      LISTENER_GROUPS.flatMap((g) => g.fields).map((f) => [f.key, f]),
    );
    expect(byKey.get("enabled")?.kind).toBe("checkbox");
    expect(byKey.get("port")?.kind).toBe("number");
  });

  // The port field carries `min: 0` so the documented "0 = 未设置/不绑定" value isn't flagged
  // invalid by the number input (the renderer reads `def.min ?? 1`, which would otherwise mark
  // a 0 port red). Locks the regression that produced the red number-input state.
  it("port field has min 0 (so 0 = 未设置 isn't flagged invalid)", () => {
    const port = LISTENER_GROUPS.flatMap((g) => g.fields).find((f) => f.key === "port");
    expect(port?.min).toBe(0);
  });

  it("remote-web listener editor hides raw allowedOrigins", () => {
    const remoteKeys = listenerGroupsForKind("remote-web").flatMap((g) =>
      g.fields.map((f) => f.key),
    );
    const terminalKeys = listenerGroupsForKind("terminal").flatMap((g) =>
      g.fields.map((f) => f.key),
    );
    expect(remoteKeys).not.toContain("allowedOrigins");
    expect(terminalKeys).toContain("allowedOrigins");
  });
});

describe("TUNNEL_GROUPS (AB#1064)", () => {
  it("covers every Tunnel key (except identity fields) exactly once across groups", () => {
    const keys = TUNNEL_GROUPS.flatMap((g) => g.fields.map((f) => f.key)).sort();
    const expected = (Object.keys(validTunnel()) as (keyof Tunnel)[])
      .filter((k) => !TUNNEL_IDENTITY_KEYS.includes(k))
      .sort();
    expect(keys).toEqual(expected);
  });

  it("mode is a select single-sourced from WEBHOOK_TUNNEL_MODES (reused, no drift)", () => {
    const f = TUNNEL_GROUPS.flatMap((g) => g.fields).find((f) => f.key === "mode");
    expect(f?.kind).toBe("select");
    expect(f?.options).toEqual(WEBHOOK_TUNNEL_MODES);
  });

  it("enabled is a checkbox; targetListenerId, command, and publicUrl are text", () => {
    const byKey = new Map(
      TUNNEL_GROUPS.flatMap((g) => g.fields).map((f) => [f.key, f]),
    );
    expect(byKey.get("enabled")?.kind).toBe("checkbox");
    expect(byKey.get("targetListenerId")?.kind).toBe("text");
    expect(byKey.get("command")?.kind).toBe("text");
    expect(byKey.get("publicUrl")?.kind).toBe("text");
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
  // Bitbucket source (717): repo is a BARE slug (no slash, no whitespace); the project
  // key lives in the bitbucketProject field.
  it("accepts a bare name for a bitbucket source", () => {
    expect(
      validateStep("repo", { ...validProject(), sourceKind: "bitbucket", repo: "gocell" }),
    ).toBeNull();
  });
  it.each(["proj/repo", "GOCELL/gocell", "", "  ", "two words"])(
    "rejects %j for a bitbucket source (slash, whitespace, or empty)",
    (repo) => {
      expect(
        validateStep("repo", { ...validProject(), sourceKind: "bitbucket", repo }),
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
  it("skips the gate for a non-codex engine (#718)", () => {
    // claude discovers `.claude/skills/` from cwd → skillRelPath is unused, so an empty
    // path must NOT block (the field is also hidden via visibleWhen; backend skips it too).
    expect(
      validateStep("skill", {
        ...validProject(),
        engineKind: "claude",
        skillRelPath: "",
      }),
    ).toBeNull();
  });
});

describe("skillRelPath field is codex-only (#718)", () => {
  it("is hidden for a claude project, shown for codex", () => {
    const field = PROJECT_GROUPS.flatMap((g) => g.fields).find(
      (f) => f.key === "skillRelPath",
    );
    expect(field?.visibleWhen).toBeDefined();
    expect(field?.visibleWhen?.({ ...validProject(), engineKind: "codex" })).toBe(
      true,
    );
    expect(
      field?.visibleWhen?.({ ...validProject(), engineKind: "claude" }),
    ).toBe(false);
  });
});

describe("validateStep — update (intervals)", () => {
  it("accepts positive intervals", () => {
    expect(validateStep("update", validProject())).toBeNull();
  });
  it.each([
    { pollIntervalSecs: 0 },
    { prCooldownSeconds: 0 },
    { pollIntervalSecs: -1 },
    { prCooldownSeconds: -5 },
    { pollIntervalSecs: NaN },
    { prCooldownSeconds: NaN },
  ])("rejects non-positive / NaN %o", (patch) => {
    expect(validateStep("update", { ...validProject(), ...patch })).toBeTruthy();
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

describe("validateStep — source (717 bitbucket)", () => {
  function bitbucketProject(): Project {
    return {
      ...validProject(),
      sourceKind: "bitbucket",
      repo: "gocell",
      bitbucketHost: "https://bitbucket.mycompany.com",
      bitbucketProject: "GOCELL",
      bitbucketToken: "secret-pat",
    };
  }
  it("accepts a bitbucket source with host + project + token filled", () => {
    expect(validateStep("source", bitbucketProject())).toBeNull();
  });
  it.each([
    { bitbucketHost: "", token: "bitbucketHost" },
    { bitbucketHost: "   ", token: "bitbucketHost" },
    { bitbucketProject: "", token: "bitbucketProject" },
    { bitbucketProject: "  ", token: "bitbucketProject" },
    { bitbucketToken: "", token: "bitbucketToken" },
    { bitbucketToken: "   ", token: "bitbucketToken" },
  ])("rejects a bitbucket source with a blank field %o", ({ token, ...patch }) => {
    const err = validateStep("source", { ...bitbucketProject(), ...patch });
    expect(err).toBeTruthy();
    // Error must start with the offending field token so errorToStep can route it.
    expect(err!.startsWith(token)).toBe(true);
  });
});

// Bitbucket-only update gates (717): the backend `validate_project` enforces two
// extra constraints for a Bitbucket source — labels must be title-parsed (no native
// labels) and webhook-driven update modes are invalid (no inbound webhook). validateStep
// pre-gates both in the update step (where labelSource + updateMode are surfaced), so
// the user is caught before submit. Error tokens (`labelSource`/`updateMode`) route via
// errorToStep back to this step. Backend remains the source of truth.
describe("validateStep — update (717 bitbucket gates)", () => {
  function bitbucketProject(): Project {
    return {
      ...validProject(),
      sourceKind: "bitbucket",
      repo: "gocell",
      bitbucketHost: "https://bitbucket.mycompany.com",
      bitbucketProject: "GOCELL",
      bitbucketToken: "secret-pat",
      labelSource: "title",
      updateMode: "pull-only",
    };
  }
  it("accepts a bitbucket source with labelSource=title + updateMode=pull-only", () => {
    expect(validateStep("update", bitbucketProject())).toBeNull();
  });
  it("blocks a bitbucket source whose labelSource is native (no native labels)", () => {
    const err = validateStep("update", {
      ...bitbucketProject(),
      labelSource: "native",
    });
    expect(err).toBeTruthy();
    // Error must start with the field token so errorToStep can route it.
    expect(err!.startsWith("labelSource")).toBe(true);
  });
  it.each(["webhook-only", "hybrid"] as const)(
    "blocks a bitbucket source whose updateMode is %s (no inbound webhook)",
    (updateMode) => {
      const err = validateStep("update", {
        ...bitbucketProject(),
        updateMode,
      });
      expect(err).toBeTruthy();
      // Error must start with the field token so errorToStep can route it.
      expect(err!.startsWith("updateMode")).toBe(true);
    },
  );
  it("does not impose the bitbucket gates on a github source", () => {
    // validProject() is github with labelSource=native + updateMode=webhook-only —
    // those are fine for github, so the update step must still pass.
    expect(validateStep("update", validProject())).toBeNull();
  });
});

describe("STEPS ordering", () => {
  it("is the wizard sequence source→repo→repoRoot→skill→update→done (717 F8)", () => {
    // `source` comes BEFORE `repo` so the source is chosen before validateStep("repo")
    // branches the repo-shape check on draft.sourceKind — otherwise a Bitbucket/Azure
    // bare slug would be checked against the default github owner/name rule and the user
    // could never advance past the repo step.
    expect(STEPS).toEqual([
      "source",
      "repo",
      "repoRoot",
      "skill",
      "update",
      "done",
    ]);
  });

  it("source step precedes the repo step (717 F8)", () => {
    expect(STEPS.indexOf("source")).toBeLessThan(STEPS.indexOf("repo"));
  });
});

describe("STEP_FIELDS — onboarding wizard step → field wiring (818 F2/F3)", () => {
  it("the source step owns sourceKind + the azure + bitbucket fields (818 F2, 717)", () => {
    expect(STEP_FIELDS.source).toContain("sourceKind");
    expect(STEP_FIELDS.source).toContain("azureOrg");
    expect(STEP_FIELDS.source).toContain("azureProject");
    expect(STEP_FIELDS.source).toContain("bitbucketHost");
    expect(STEP_FIELDS.source).toContain("bitbucketProject");
    expect(STEP_FIELDS.source).toContain("bitbucketToken");
  });

  it("the update step surfaces updateMode (818 F3) + labelSource (717)", () => {
    expect(STEP_FIELDS.update).toContain("updateMode");
    expect(STEP_FIELDS.update).toContain("labelSource");
  });

  it("every STEP_FIELDS key is a real PROJECT_GROUPS field", () => {
    const grouped = new Set(PROJECT_GROUPS.flatMap((g) => g.fields).map((f) => f.key));
    for (const keys of Object.values(STEP_FIELDS)) {
      for (const k of keys) expect(grouped.has(k)).toBe(true);
    }
  });
});

describe("visibleStepFields — conditional fields per step (818 F2)", () => {
  it("hides the azure + bitbucket fields on the source step for a github source", () => {
    const keys = visibleStepFields("source", validProject()).map((f) => f.key);
    expect(keys).toEqual(["sourceKind"]);
  });

  it("shows the azure fields on the source step for an azure source", () => {
    const draft = { ...validProject(), sourceKind: "azure" as const };
    const keys = visibleStepFields("source", draft).map((f) => f.key);
    expect(keys).toEqual(["sourceKind", "azureOrg", "azureProject"]);
  });

  it("shows the bitbucket fields on the source step for a bitbucket source (717)", () => {
    const draft = { ...validProject(), sourceKind: "bitbucket" as const };
    const keys = visibleStepFields("source", draft).map((f) => f.key);
    expect(keys).toEqual([
      "sourceKind",
      "bitbucketHost",
      "bitbucketProject",
      "bitbucketToken",
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
  it("interval messages → update step", () => {
    expect(errorToStep("pollIntervalSecs 必须大于 0")).toBe("update");
    expect(errorToStep("prCooldownSeconds 必须大于 0")).toBe("update");
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
  // Bitbucket connection fields (717): the source step owns them (rendered + validated for
  // a bitbucket source), so a backend rejection routes back to source. All three tokens
  // are bitbucket-prefixed and collide with no other field token.
  it("bitbucket messages → source step (717, owned by the source step)", () => {
    expect(errorToStep("bitbucketHost 不能为空（bitbucket 源需填 Server/DC 基址）")).toBe("source");
    expect(errorToStep("bitbucketProject 不能为空（bitbucket 源需填项目 key）")).toBe("source");
    expect(errorToStep("bitbucketToken 不能为空（bitbucket 源需填 access token）")).toBe("source");
  });
  // labelSource (717) lives in the labels group, surfaced in the update step alongside
  // reviewLabel/checkLabel, so a backend rejection routes there.
  it("labelSource message → update step (717)", () => {
    expect(errorToStep("labelSource 取值非法")).toBe("update");
    expect(errorToStep("labelSource 必须为 title（Bitbucket 源无原生标签）")).toBe("update");
  });
  // updateMode (717) lives in the polling group, surfaced in the update step. The
  // backend rejects webhook-only/hybrid for a Bitbucket source (no inbound webhook), so
  // its rejection routes back to the update step too.
  it("updateMode message → update step (717)", () => {
    expect(errorToStep("updateMode Bitbucket 源不支持 webhook（无入站）")).toBe("update");
  });
});
