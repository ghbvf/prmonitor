import type { AppConfig, RuleActionKind, RuleConfig } from "./types";

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
    actions: ["review"],
  };
}

export function toggleRuleAction(
  rule: RuleConfig,
  kind: RuleActionKind,
  checked: boolean,
): RuleConfig {
  if (checked && !rule.actions.includes(kind)) {
    return { ...rule, actions: [...rule.actions, kind] };
  }
  if (!checked) {
    return { ...rule, actions: rule.actions.filter((action) => action !== kind) };
  }
  return rule;
}
