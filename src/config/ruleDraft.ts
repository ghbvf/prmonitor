import type { AppConfig, RuleActionConfig, RuleActionKind, RuleConfig } from "./types";

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
  return {
    id: kind,
    kind,
    enabled: true,
    target: { kind: "none" },
    dedupePolicy: "event",
    delaySecs: 0,
    dependsOn: [],
    level: "action",
  };
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
    actions: [createRuleAction("review")],
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
  if (!checked) {
    return { ...rule, actions: rule.actions.filter((action) => action.kind !== kind) };
  }
  return rule;
}
