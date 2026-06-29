import { describe, expect, it } from "vitest";
import {
  DEFAULT_NOTIFICATION_SETTINGS,
  DEFAULT_REVIEW_LIFECYCLE_NOTIFICATION_CONFIG,
  NEW_PROJECT_DEFAULTS,
} from "./defaults";
import {
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
    cloudflaredBin: "cloudflared",
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
      id: "review",
      kind: "review",
      enabled: true,
      target: { kind: "none" },
      dedupePolicy: "event",
      delaySecs: 0,
      dependsOn: [],
      level: "action",
    }],
    allowActionKinds: [],
    denyActionKinds: [],
  };
}

describe("rule draft helpers", () => {
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
      actions: [expect.objectContaining({ id: "review", kind: "review" })],
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

  it("toggles actions without duplicates and supports removal", () => {
    const base = rule();
    const withCheck = toggleRuleAction(base, "check", true);
    expect(withCheck.actions.map((action) => action.kind)).toEqual(["review", "check"]);
    expect(toggleRuleAction(withCheck, "check", true).actions.map((action) => action.kind)).toEqual([
      "review",
      "check",
    ]);
    expect(toggleRuleAction(withCheck, "review", false).actions.map((action) => action.kind)).toEqual([
      "check",
    ]);
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
      dependsOn: ["review"],
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
