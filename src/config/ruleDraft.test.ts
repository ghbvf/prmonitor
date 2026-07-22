import { describe, expect, it } from "vitest";
import {
  DEFAULT_CLI_TOOLS_CONFIG,
  DEFAULT_NOTIFICATION_SETTINGS,
  DEFAULT_REVIEW_LIFECYCLE_NOTIFICATION_CONFIG,
  NEW_PROJECT_DEFAULTS,
} from "./defaults";
import {
  createRuleAction,
  createDefaultRule,
  nextRuleId,
  parseRuleCsv,
  toggleRuleAction,
} from "./ruleDraft";
import type { AppConfig, Project, RuleConfig } from "./types";

function project(id: string, repo: string): Project {
  return { ...NEW_PROJECT_DEFAULTS, id, name: id, repo };
}

function draft(projects: Project[], activeProjectId: string): AppConfig {
  return {
    projects,
    activeProjectId,
    webhookEnabled: false,
    webhookPort: 8787,
    webhookSecret: "",
    cliTools: { ...DEFAULT_CLI_TOOLS_CONFIG },
    webhookTunnelMode: "quick",
    webhookTunnelCommand: "",
    webhookPublicUrl: "",
    localApiToken: "",
    outbox: { notificationTtlSecs: 7200 },
    notifications: {
      ...DEFAULT_NOTIFICATION_SETTINGS,
      channels: DEFAULT_NOTIFICATION_SETTINGS.channels.map((c) => ({ ...c })),
    },
    messaging: { integrations: [] },
    remoteAccess: { entrypoints: [], tunnels: [] },
    rules: [],
    reviewLifecycleNotifications: DEFAULT_REVIEW_LIFECYCLE_NOTIFICATION_CONFIG,
  };
}

function rule(): RuleConfig {
  return {
    id: "rule-1",
    name: "Ready",
    enabled: true,
    source: null,
    eventType: "pullRequest",
    projectId: "p1",
    repo: "owner/repo",
    labelsAny: [],
    labelsAll: [],
    titleContains: "",
    bodyContains: "",
    actions: [{
      id: "runSkill",
      kind: "runSkill",
      enabled: true,
      dedupePolicy: "event",
      delaySecs: 0,
      level: "action",
      skillName: "pr-review",
      skillPath: ".codex/skills/pr-review/SKILL.md",
      commandTemplate: "/{skill} {pr}",
      extraArgs: "",
    }],
    allowActionKinds: [],
    denyActionKinds: [],
  };
}

describe("rule draft helpers", () => {
  it("creates the complete ordered-action contract", () => {
    expect(Object.keys(createRuleAction("runSkill"))).toEqual([
      "id",
      "kind",
      "enabled",
      "dedupePolicy",
      "delaySecs",
      "level",
      "skillName",
      "skillPath",
      "commandTemplate",
      "extraArgs",
    ]);
  });

  it("creates a default rule from the active project and repo", () => {
    const d = draft([project("p1", "owner/repo")], "p1");
    const created = createDefaultRule(d);

    expect(created).toMatchObject({
      id: "rule-1",
      name: "新规则",
      enabled: true,
      eventType: "pullRequest",
      projectId: "p1",
      repo: "owner/repo",
      actions: [expect.objectContaining({ id: "runSkill", kind: "runSkill", skillName: "pr-review" })],
      allowActionKinds: [],
      denyActionKinds: [],
    });
  });

  it("allocates the next unused rule id", () => {
    expect(nextRuleId([{ id: "rule-1" }, { id: "rule-3" }])).toBe("rule-2");
  });

  it("parses comma separated labels by trimming and dropping blanks", () => {
    expect(parseRuleCsv(" ready, ,needs-review, urgent ")).toEqual([
      "ready",
      "needs-review",
      "urgent",
    ]);
  });

  it("toggles actions without duplicates and disables instead of deleting", () => {
    const base = rule();
    const withNotify = toggleRuleAction(base, "notify", true);
    expect(withNotify.actions.map((action) => action.kind)).toEqual(["runSkill", "notify"]);
    expect(toggleRuleAction(withNotify, "notify", true).actions.map((action) => action.kind)).toEqual([
      "runSkill",
      "notify",
    ]);
    const disabledSkill = toggleRuleAction(withNotify, "runSkill", false);
    expect(disabledSkill.actions.map((action) => action.kind)).toEqual(["runSkill", "notify"]);
    expect(disabledSkill.actions.find((action) => action.kind === "runSkill")?.enabled).toBe(false);
  });

  it("preserves multiple runSkill actions when kind is unchecked", () => {
    const base = rule();
    const review = base.actions[0];
    if (review.kind !== "runSkill") throw new Error("expected runSkill");
    const multi: RuleConfig = {
      ...base,
      actions: [
        { ...review, id: "review", extraArgs: "" },
        { ...review, id: "check", extraArgs: "--check" },
      ],
    };
    const disabled = toggleRuleAction(multi, "runSkill", false);
    expect(disabled.actions.map((a) => a.id)).toEqual(["review", "check"]);
    expect(disabled.actions.every((a) => a.kind === "runSkill" && !a.enabled)).toBe(true);
    const reenabled = toggleRuleAction(disabled, "runSkill", true);
    expect(reenabled.actions.every((a) => a.enabled)).toBe(true);
  });

  it("re-enables an existing disabled action without losing its settings", () => {
    const base = rule();
    const disabled = {
      id: "notify",
      kind: "notify" as const,
      enabled: false,
      target: { kind: "notificationChannels" as const, channelIds: ["desktop"] },
      dedupePolicy: "action" as const,
      delaySecs: 30,
      level: "important",
    };
    const updated = toggleRuleAction(
      { ...base, actions: [base.actions[0], disabled] },
      "notify",
      true,
    );

    expect(updated.actions.find((action) => action.kind === "notify")).toEqual({
      ...disabled,
      enabled: true,
    });
  });
});
