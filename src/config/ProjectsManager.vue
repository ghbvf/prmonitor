<script setup lang="ts">
// Projects list manager (#35): the per-project editor surface inside Settings. Owns
// the add/delete/active-select operations over the SettingsView AppConfig draft's
// `projects` array; each project's fields are edited by a ProjectCard. Edits land on
// the draft in place (the parent's reactive AppConfig), committed by SettingsView's
// existing save flow alongside the global webhook fields.
//
// Each project renders as a collapsible row: an always-visible header (chevron + the
// 启动/enabled toggle + name + 当前/active pick + delete) over a ProjectCard that holds
// the detailed fields and only shows when expanded. The header owns the project-identity
// controls (name/enabled) and the list operations; ProjectCard stays a pure field form.
// The add/delete draft mutations (and the activeProjectId invariant they keep) live in
// projectOps.ts so they're unit-testable without a component harness.
import { ref, watch } from "vue";
import type { AppConfig } from "./types";
import type { ProjectFieldKey } from "./fields";
import { addProjectToDraft, deleteProjectFromDraft } from "./projectOps";
import ProjectCard from "./ProjectCard.vue";

// The live AppConfig draft (reactive, owned by SettingsView). We mutate `projects`
// and `activeProjectId` in place; SettingsView persists the whole draft on save.
const props = defineProps<{ draft: AppConfig }>();
const emit = defineEmits<{ edit: [] }>();

// Which project cards are expanded — independent toggle (multiple may be open at once)
// keyed by project id. A long list stays scannable because cards collapse to a single
// header row by default. Vue tracks Set .add/.delete/.has reactively inside a ref.
const expanded = ref(new Set<string>());
// Seed the open set ONCE, when projects first hydrate (length 0 → N): expand the active
// project so the user lands on their current project open, everything else collapsed.
// `seeded` is a plain variable, not a ref — it's only read/written inside the watch
// callback and never used in the template, so it needs no reactivity. It guards against
// re-hydrates and the user's later manual collapses (we never re-add a closed project).
let seeded = false;
watch(
  () => props.draft.projects.length,
  (n) => {
    if (seeded || n === 0) return;
    if (props.draft.activeProjectId) expanded.value.add(props.draft.activeProjectId);
    seeded = true;
  },
  { immediate: true },
);

function isExpanded(id: string): boolean {
  return expanded.value.has(id);
}
function toggleExpand(id: string) {
  if (expanded.value.has(id)) expanded.value.delete(id);
  else expanded.value.add(id);
}

function addProject() {
  // projectOps keeps the activeProjectId invariant (first project added to an empty
  // list becomes active); here we only own the UI concern of opening the fresh card.
  const p = addProjectToDraft(props.draft);
  expanded.value.add(p.id);
  emit("edit");
}

// Apply a single field edit (from a card OR the row header's name/enabled controls)
// onto the matching draft project. Indexed by the project's stable `id` (not array
// position) so a concurrent reorder/delete can't misroute the write.
function onUpdate(
  id: string,
  key: ProjectFieldKey,
  value: string | number | boolean | string[],
) {
  const p = props.draft.projects.find((x) => x.id === id);
  if (!p) return;
  // Each ProjectFieldKey's value type matches the emitted value (the card mirrors the
  // FieldDef.kind→value-type pairing); the cast bridges the heterogeneous signature.
  (p as Record<ProjectFieldKey, unknown>)[key] = value;
  emit("edit");
}

// Header name/enabled controls route through onUpdate (name/enabled are valid
// ProjectFieldKeys — `keyof Project`). Small handlers keep the input/checkbox event
// unwrapping out of the template (mirrors ProjectCard's old onName).
function onName(id: string, e: Event) {
  onUpdate(id, "name", (e.target as HTMLInputElement).value);
}
function onEnabled(id: string, e: Event) {
  onUpdate(id, "enabled", (e.target as HTMLInputElement).checked);
}

// Two-step in-app delete confirmation. `window.confirm` is unreliable in the Tauri
// webview (wry doesn't wire WKWebView's JS confirm panel, so it returns false WITHOUT
// prompting → the splice never ran and the row never disappeared). A pure-Vue confirm
// strip works in every webview. At most one row is pending at a time — clicking 删除 on
// another row just moves the pending id.
const pendingDeleteId = ref<string | null>(null);

function requestDelete(id: string) {
  // Clicking 删除 only arms the confirm — the destructive splice waits for confirmDelete.
  pendingDeleteId.value = id;
}
function cancelDelete() {
  pendingDeleteId.value = null;
}
function confirmDelete(id: string) {
  // projectOps keeps activeProjectId pointing at a project that still exists (or "" when
  // the list empties — the cleared state the backend accepts); we clean up UI state.
  if (deleteProjectFromDraft(props.draft, id)) {
    expanded.value.delete(id);
    emit("edit");
  }
  pendingDeleteId.value = null;
}

function setActive(id: string) {
  props.draft.activeProjectId = id;
  emit("edit");
}
</script>

<template>
  <div class="projects">
    <div class="projects-head">
      <p class="lead">
        管理监控的项目；勾选「启动」决定监控哪些项目，选择一个作为「当前」项目（监控视图聚焦它）。
      </p>
      <button type="button" class="add" @click="addProject">+ 添加项目</button>
    </div>

    <div v-if="draft.projects.length > 0" class="list">
      <div
        v-for="p in draft.projects"
        :key="p.id"
        class="list-item"
        :class="{ disabled: !p.enabled }"
      >
        <header class="row-head">
          <button
            type="button"
            class="chevron"
            :aria-expanded="isExpanded(p.id)"
            :aria-controls="`project-card-${p.id}`"
            :aria-label="(isExpanded(p.id) ? '收起' : '展开') + '项目 ' + (p.name || '新项目')"
            :title="isExpanded(p.id) ? '收起' : '展开'"
            @click="toggleExpand(p.id)"
          >
            {{ isExpanded(p.id) ? "▾" : "▸" }}
          </button>
          <label
            class="enabled-pick"
            :title="p.enabled ? '已启动（监控中）' : '未启动（不轮询 / 不接 webhook）'"
          >
            <input type="checkbox" :checked="p.enabled" @change="onEnabled(p.id, $event)" />
            <span>启动</span>
          </label>
          <input
            class="name-input"
            type="text"
            :value="p.name"
            placeholder="新项目"
            :aria-label="`项目 ${p.name || '新项目'} 的名称`"
            @input="onName(p.id, $event)"
          />
          <label class="active-pick">
            <input
              type="radio"
              name="active-project"
              :value="p.id"
              :checked="p.id === draft.activeProjectId"
              @change="setActive(p.id)"
            />
            <span>当前</span>
          </label>
          <template v-if="pendingDeleteId === p.id">
            <span class="confirm-text">确认删除？</span>
            <button
              type="button"
              class="confirm-yes"
              :aria-label="`确认删除项目 ${p.name || '新项目'}`"
              @click="confirmDelete(p.id)"
            >
              确认
            </button>
            <button
              type="button"
              class="confirm-no"
              :aria-label="`取消删除项目 ${p.name || '新项目'}`"
              @click="cancelDelete"
            >
              取消
            </button>
          </template>
          <button
            v-else
            type="button"
            class="delete"
            :aria-label="`删除项目 ${p.name || '新项目'}`"
            @click="requestDelete(p.id)"
          >
            删除
          </button>
        </header>
        <ProjectCard
          v-show="isExpanded(p.id)"
          :id="`project-card-${p.id}`"
          :project="p"
          @update="(key, value) => onUpdate(p.id, key, value)"
          @edit="emit('edit')"
        />
      </div>
    </div>

    <div v-else class="empty-state">
      <p class="empty-title">还没有项目</p>
      <p class="empty-hint">点击下方按钮创建第一个项目。</p>
      <button type="button" class="add" @click="addProject">+ 添加项目</button>
    </div>
  </div>
</template>

<style scoped>
.projects {
  display: flex;
  flex-direction: column;
  gap: var(--space-6);
  width: 100%;
}
.projects-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
}
.lead {
  margin: 0;
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
}
.add {
  flex-shrink: 0;
  padding: var(--space-3) var(--space-5);
  font: inherit;
  font-size: var(--font-size-sm);
  color: var(--color-surface);
  background: var(--color-accent);
  border: none;
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.list {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.list-item {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
/* Disabled (未启动) projects dim their header so "won't be monitored" reads at a glance. */
.list-item.disabled .row-head {
  opacity: 0.55;
}
.row-head {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-2) var(--space-3);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
}
.chevron {
  flex-shrink: 0;
  width: 1.6rem;
  padding: var(--space-1) 0;
  font: inherit;
  color: var(--color-text-muted);
  background: none;
  border: none;
  cursor: pointer;
}
.enabled-pick,
.active-pick {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  flex-shrink: 0;
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
  cursor: pointer;
}
.name-input {
  flex: 1;
  min-width: 0;
  padding: var(--space-2) var(--space-3);
  font: inherit;
  font-size: var(--font-size-md);
  color: var(--color-text);
  background: var(--color-surface);
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-sm);
}
.name-input:focus {
  outline: none;
  border-color: var(--color-accent);
}
.delete {
  flex-shrink: 0;
  padding: var(--space-2) var(--space-4);
  font: inherit;
  font-size: var(--font-size-sm);
  color: var(--color-danger);
  background: none;
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.delete:hover {
  background: var(--color-surface-hover);
}
/* Inline delete-confirm strip — replaces the 删除 button while a row is pending. */
.confirm-text {
  flex-shrink: 0;
  font-size: var(--font-size-sm);
  color: var(--color-danger);
}
.confirm-yes,
.confirm-no {
  flex-shrink: 0;
  padding: var(--space-2) var(--space-4);
  font: inherit;
  font-size: var(--font-size-sm);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.confirm-yes {
  color: var(--color-surface);
  background: var(--color-danger);
  border: 1px solid var(--color-danger);
}
.confirm-no {
  color: var(--color-text);
  background: none;
  border: 1px solid var(--color-border-strong);
}
.confirm-no:hover {
  background: var(--color-surface-hover);
}
.empty-state {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-6);
  text-align: center;
  color: var(--color-text-muted);
  background: var(--color-surface);
  border: 1px dashed var(--color-border-strong);
  border-radius: var(--radius-md);
}
.empty-title {
  margin: 0;
  font-size: var(--font-size-md);
  color: var(--color-text);
}
.empty-hint {
  margin: 0;
  font-size: var(--font-size-sm);
}
</style>
