<script setup lang="ts">
import type { AppConfig, RuleActionKind, RuleConfig } from "./types";
import { RULE_ACTION_KINDS } from "./types";
import { createDefaultRule, parseRuleCsv, removeRuleAction, toggleRuleAction } from "./ruleDraft";
import type { EventType, SourceKind } from "../types";
import { EVENT_TYPES, SOURCE_KINDS } from "../types.generated";

const props = defineProps<{ draft: AppConfig }>();
const emit = defineEmits<{ edit: [] }>();

function assertNever(value: never): never {
  throw new Error(`Unhandled rule action kind: ${value}`);
}

function actionLabel(kind: RuleActionKind): string {
  switch (kind) {
    case "runSkill":
      return "Run skill";
    case "notify":
      return "Notify";
    default:
      return assertNever(kind);
  }
}

function runSkillActions(rule: RuleConfig) {
  return rule.actions.filter((action) => action.kind === "runSkill");
}

function runSkillTitle(action: Extract<import("./types").RuleActionConfig, { kind: "runSkill" }>) {
  const name = action.skillName.trim() || "runSkill";
  const extra = action.extraArgs.trim();
  return extra ? `${name} ${extra}` : name;
}

function onSkillField(
  action: Extract<import("./types").RuleActionConfig, { kind: "runSkill" }>,
  key: "skillName" | "skillPath" | "commandTemplate" | "extraArgs",
  event: Event,
) {
  action[key] = (event.target as HTMLInputElement).value;
  emit("edit");
}

function addRule() {
  props.draft.rules.push(createDefaultRule(props.draft));
  emit("edit");
}

function deleteRule(index: number) {
  props.draft.rules.splice(index, 1);
  emit("edit");
}

function onText(rule: RuleConfig, key: keyof Pick<RuleConfig, "id" | "name" | "repo" | "titleContains" | "bodyContains">, event: Event) {
  rule[key] = (event.target as HTMLInputElement).value;
  emit("edit");
}

function onProject(rule: RuleConfig, event: Event) {
  rule.projectId = (event.target as HTMLSelectElement).value;
  emit("edit");
}

function onEnabled(rule: RuleConfig, event: Event) {
  rule.enabled = (event.target as HTMLInputElement).checked;
  emit("edit");
}

function onSource(rule: RuleConfig, event: Event) {
  const value = (event.target as HTMLSelectElement).value;
  rule.source = value === "" ? null : (value as SourceKind);
  emit("edit");
}

function onEventType(rule: RuleConfig, event: Event) {
  const value = (event.target as HTMLSelectElement).value;
  rule.eventType = value === "" ? null : (value as EventType);
  emit("edit");
}

function onCsv(rule: RuleConfig, key: "labelsAny" | "labelsAll", event: Event) {
  rule[key] = parseRuleCsv((event.target as HTMLInputElement).value);
  emit("edit");
}

function toggleAction(rule: RuleConfig, kind: RuleActionKind, event: Event) {
  const checked = (event.target as HTMLInputElement).checked;
  rule.actions = toggleRuleAction(rule, kind, checked).actions;
  emit("edit");
}

function removeAction(rule: RuleConfig, actionId: string) {
  rule.actions = removeRuleAction(rule, actionId).actions;
  emit("edit");
}

function hasAction(rule: RuleConfig, kind: RuleActionKind): boolean {
  return rule.actions.some((action) => action.kind === kind && action.enabled);
}
</script>

<template>
  <div class="rules">
    <div class="rules-head">
      <p class="lead">规则把 inbox event 匹配为 action plan。空 matcher 表示通配。</p>
      <button type="button" class="add" @click="addRule">+ 添加规则</button>
    </div>

    <p v-if="draft.rules.length === 0" class="empty">暂无规则。</p>

    <div v-else class="rule-list">
      <section v-for="(rule, index) in draft.rules" :key="rule.id" class="rule-row">
        <header class="row-head">
          <label class="enabled">
            <input type="checkbox" :checked="rule.enabled" @change="onEnabled(rule, $event)" />
            <span>启用</span>
          </label>
          <input class="name" type="text" :value="rule.name" @input="onText(rule, 'name', $event)" />
          <button type="button" class="delete" @click="deleteRule(index)">删除</button>
        </header>

        <div class="grid">
          <label>
            <span>ID</span>
            <input type="text" :value="rule.id" @input="onText(rule, 'id', $event)" />
          </label>
          <label>
            <span>来源</span>
            <select :value="rule.source ?? ''" @change="onSource(rule, $event)">
              <option value="">任意</option>
              <option v-for="source in SOURCE_KINDS" :key="source" :value="source">{{ source }}</option>
            </select>
          </label>
          <label>
            <span>事件</span>
            <select :value="rule.eventType ?? ''" @change="onEventType(rule, $event)">
              <option value="">任意</option>
              <option v-for="eventType in EVENT_TYPES" :key="eventType" :value="eventType">{{ eventType }}</option>
            </select>
          </label>
          <label>
            <span>项目</span>
            <select :value="rule.projectId" @change="onProject(rule, $event)">
              <option value="">任意</option>
              <option v-for="project in draft.projects" :key="project.id" :value="project.id">{{ project.name }}</option>
            </select>
          </label>
          <label>
            <span>Repo</span>
            <input type="text" :value="rule.repo" @input="onText(rule, 'repo', $event)" />
          </label>
          <label>
            <span>Labels any</span>
            <input type="text" :value="rule.labelsAny.join(', ')" @input="onCsv(rule, 'labelsAny', $event)" />
          </label>
          <label>
            <span>Labels all</span>
            <input type="text" :value="rule.labelsAll.join(', ')" @input="onCsv(rule, 'labelsAll', $event)" />
          </label>
          <label>
            <span>Title contains</span>
            <input type="text" :value="rule.titleContains" @input="onText(rule, 'titleContains', $event)" />
          </label>
          <label>
            <span>Body contains</span>
            <input type="text" :value="rule.bodyContains" @input="onText(rule, 'bodyContains', $event)" />
          </label>
        </div>

        <div class="actions">
          <label v-for="kind in RULE_ACTION_KINDS" :key="kind" class="action">
            <input
              type="checkbox"
              :checked="hasAction(rule, kind)"
              @change="toggleAction(rule, kind, $event)"
            />
            <span>{{ actionLabel(kind) }}</span>
          </label>
        </div>

        <div v-for="action in runSkillActions(rule)" :key="action.id" class="grid skill-fields">
          <div class="skill-title">
            <span>{{ runSkillTitle(action) }}</span>
            <button type="button" class="delete" @click="removeAction(rule, action.id)">删除</button>
          </div>
          <label>
            <span>Skill name</span>
            <input
              type="text"
              :value="action.skillName"
              placeholder="pr-review"
              @input="onSkillField(action, 'skillName', $event)"
            />
          </label>
          <label>
            <span>Skill path</span>
            <input
              type="text"
              :value="action.skillPath"
              placeholder=".codex/skills/pr-review/SKILL.md"
              @input="onSkillField(action, 'skillPath', $event)"
            />
          </label>
          <label>
            <span>Command template</span>
            <input
              type="text"
              :value="action.commandTemplate"
              placeholder="/{skill} {pr}"
              title="Placeholders: {skill} {pr} {repo}"
              @input="onSkillField(action, 'commandTemplate', $event)"
            />
            <small class="hint">Placeholders: {skill} {pr} {repo}</small>
          </label>
          <label>
            <span>Extra args</span>
            <input
              type="text"
              :value="action.extraArgs"
              placeholder="--check"
              title="Appended to the rendered command (e.g. --check)"
              @input="onSkillField(action, 'extraArgs', $event)"
            />
            <small class="hint">Optional; e.g. --check for fix verification</small>
          </label>
        </div>
      </section>
    </div>
  </div>
</template>

<style scoped>
.rules {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.rules-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
}
.lead,
.empty {
  margin: 0;
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
}
.add,
.delete {
  padding: var(--space-2) var(--space-3);
  font: inherit;
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
  background: var(--color-surface);
  color: var(--color-text);
  cursor: pointer;
}
.delete {
  color: var(--color-danger);
}
.rule-list {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.rule-row {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
  padding: var(--space-4);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
  background: var(--color-surface);
}
.row-head,
.actions {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.name {
  flex: 1;
}
.enabled,
.action {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  font-size: var(--font-size-sm);
}
.grid {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: var(--space-3);
}
.skill-fields .skill-title {
  grid-column: 1 / -1;
  font-size: var(--font-size-sm);
  font-weight: 600;
  color: var(--color-text);
}
.hint {
  color: var(--color-text-muted);
  font-size: var(--font-size-xs, 0.75rem);
}
label {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  min-width: 0;
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
}
input,
select {
  min-width: 0;
  width: 100%;
  box-sizing: border-box;
  padding: var(--space-2) var(--space-3);
  font: inherit;
  color: var(--color-text);
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
}
@media (max-width: 900px) {
  .grid {
    grid-template-columns: 1fr;
  }
  .rules-head,
  .row-head,
  .actions {
    align-items: stretch;
    flex-direction: column;
  }
}
</style>
