<script setup lang="ts">
// Projects list manager (#35): the per-project editor surface inside Settings. Owns
// the add/delete/active-select operations over the SettingsView AppConfig draft's
// `projects` array; each project's fields are edited by a ProjectCard. Edits land on
// the draft in place (the parent's reactive AppConfig), committed by SettingsView's
// existing save flow alongside the global webhook fields.
import type { AppConfig, Project } from "./types";
import type { ProjectFieldKey } from "./fields";
import { NEW_PROJECT_DEFAULTS } from "./defaults";
import ProjectCard from "./ProjectCard.vue";

// The live AppConfig draft (reactive, owned by SettingsView). We mutate `projects`
// and `activeProjectId` in place; SettingsView persists the whole draft on save.
const props = defineProps<{ draft: AppConfig }>();
const emit = defineEmits<{ edit: [] }>();

// Sensible defaults for a fresh project — identity (id/name) is minted here, the rest
// come from the shared NEW_PROJECT_DEFAULTS (single-sourced with the onboarding wizard
// in defaults.ts) so the two seed paths can't drift.
function newProject(): Project {
  return { ...NEW_PROJECT_DEFAULTS, id: crypto.randomUUID(), name: "新项目" };
}

function addProject() {
  props.draft.projects.push(newProject());
  emit("edit");
}

// Apply a single field edit from a card onto the matching draft project. Indexed by
// the project's stable `id` (not array position) so a concurrent reorder/delete can't
// misroute the write.
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

function deleteProject(id: string) {
  // Guard: never delete the last project — the app always monitors ≥1 project.
  if (props.draft.projects.length <= 1) return;
  // Confirm before the destructive splice — a project carries its repo/label config.
  if (!window.confirm("确认删除该项目？/ Delete this project?")) return;
  const idx = props.draft.projects.findIndex((x) => x.id === id);
  if (idx === -1) return;
  props.draft.projects.splice(idx, 1);
  // If the active project was the one removed, fall back to the new first project so
  // `activeProjectId` always points at a project that still exists.
  if (props.draft.activeProjectId === id) {
    props.draft.activeProjectId = props.draft.projects[0].id;
  }
  emit("edit");
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
        管理监控的项目；选择一个作为「当前」项目（监控视图聚焦它）。
      </p>
      <button type="button" class="add" @click="addProject">+ 添加项目</button>
    </div>

    <div class="list">
      <div
        v-for="p in draft.projects"
        :key="p.id"
        class="list-item"
      >
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
        <ProjectCard
          :project="p"
          @update="(key, value) => onUpdate(p.id, key, value)"
          @delete="deleteProject(p.id)"
          @edit="emit('edit')"
        />
      </div>
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
  gap: var(--space-6);
}
.list-item {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.active-pick {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  align-self: flex-start;
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
  cursor: pointer;
}
</style>
