import type { AppConfig, RuleActionConfig, RuleActionKind, RuleConfig } from "./types";
import { assertNever } from "../types";
import {
  DEFAULT_COMMAND_TEMPLATE,
  DEFAULT_SKILL_NAME,
  DEFAULT_SKILL_PATH,
} from "../types.generated";

export { DEFAULT_SKILL_NAME, DEFAULT_SKILL_PATH, DEFAULT_COMMAND_TEMPLATE };

export function parseRuleCsv(value: string): string[] {
  return value
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

export function nextRuleId(rules: Pick<RuleConfig, "id">[]): string {
  const used = new Set(rules.map((rule) => rule.id));
  let n = 1;
  while (used.has(`rule-${n}`)) n += 1;
  return `rule-${n}`;
}

export function createRuleAction(kind: RuleActionKind): RuleActionConfig {
  switch (kind) {
    case "runSkill":
      return {
        id: "runSkill",
        kind: "runSkill",
        enabled: true,
        dedupePolicy: "event",
        delaySecs: 0,
        level: "action",
        skillName: DEFAULT_SKILL_NAME,
        skillPath: DEFAULT_SKILL_PATH,
        commandTemplate: DEFAULT_COMMAND_TEMPLATE,
        extraArgs: "",
      };
    case "notify":
      return {
        id: "notify",
        kind: "notify",
        enabled: true,
        target: { kind: "none" },
        dedupePolicy: "event",
        delaySecs: 0,
        level: "action",
      };
    default:
      return assertNever(kind);
  }
}

export function createDefaultRule(draft: AppConfig): RuleConfig {
  const id = nextRuleId(draft.rules);
  const projectId = draft.activeProjectId || draft.projects[0]?.id || "";
  return {
    id,
    name: "新规则",
    enabled: true,
    source: null,
    eventType: "pullRequest",
    projectId,
    repo: draft.projects.find((p) => p.id === projectId)?.repo ?? "",
    labelsAny: [],
    labelsAll: [],
    titleContains: "",
    bodyContains: "",
    actions: [createRuleAction("runSkill")],
    allowActionKinds: [],
    denyActionKinds: [],
  };
}

export function toggleRuleAction(
  rule: RuleConfig,
  kind: RuleActionKind,
  checked: boolean,
): RuleConfig {
  if (checked) {
    if (!rule.actions.some((action) => action.kind === kind)) {
      return { ...rule, actions: [...rule.actions, createRuleAction(kind)] };
    }
    return {
      ...rule,
      actions: rule.actions.map((action) =>
        action.kind === kind ? { ...action, enabled: true } : action,
      ),
    };
  }
  // Disable by kind — do not delete, so migrated multi-runSkill (review+check) is preserved.
  return {
    ...rule,
    actions: rule.actions.map((action) =>
      action.kind === kind ? { ...action, enabled: false } : action,
    ),
  };
}

/** Remove a single action by id (per-row control in RulesManager). */
export function removeRuleAction(rule: RuleConfig, actionId: string): RuleConfig {
  return { ...rule, actions: rule.actions.filter((action) => action.id !== actionId) };
}
