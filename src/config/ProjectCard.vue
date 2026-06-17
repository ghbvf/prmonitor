<script setup lang="ts">
// Per-project editor card (#35): renders ONE project's editable fields. Reuses the
// fields.ts PROJECT_GROUPS field defs through the shared ConfigField, plus a `name`
// text input for the project's display name (an identity field PROJECT_GROUPS omits).
// Stateless about persistence — it emits granular `update`/`delete` to the parent
// (ProjectsManager), which owns the AppConfig draft and the save flow.
//
// Each card owns its OWN `authorsInput` csv buffer (a local ref keyed off this
// card's project) so N cards never share author state: the parent renders one card
// per `draft.projects[i]`, and each instance carries its own comma-joined string,
// normalized back to string[] on every edit (mirrors SettingsView's old single
// authors round-trip, now per-card).
import { ref, watch } from "vue";
import type { Project } from "./types";
import { PROJECT_GROUPS, type FieldDef, type ProjectFieldKey } from "./fields";
import ConfigField from "./ConfigField.vue";

const props = defineProps<{ project: Project }>();
const emit = defineEmits<{
  // A single per-project field changed (key + new value). `authors` arrives already
  // normalized to string[]; every other key carries its native value type.
  update: [key: ProjectFieldKey, value: string | number | boolean | string[]];
  delete: [];
  edit: [];
}>();

// This card's own comma-joined authors buffer. Seeded from the project's authors and
// re-seeded whenever the bound project identity changes (e.g. the list reorders or a
// card is deleted and instances re-key) so the buffer never bleeds across projects.
const authorsInput = ref(props.project.authors.join(", "));
watch(
  () => props.project.id,
  () => {
    authorsInput.value = props.project.authors.join(", ");
  },
);

// Read a field's value for binding. `authors` surfaces as this card's comma-joined
// buffer; every other key reads straight off the bound project.
function fieldValue(def: FieldDef): string | number | boolean | string[] {
  if (def.key === "authors") return authorsInput.value;
  return props.project[def.key as ProjectFieldKey];
}

// Apply an edit. `authors` is held locally as a csv string and emitted normalized to
// string[]; every other field emits its native value straight through.
function setField(def: FieldDef, value: string | number | boolean | string[]) {
  const key = def.key as ProjectFieldKey;
  if (key === "authors") {
    authorsInput.value = Array.isArray(value) ? value.join(", ") : String(value);
    emit("update", "authors", authorsArray());
    return;
  }
  emit("update", key, value);
}

function authorsArray(): string[] {
  return authorsInput.value
    .split(",")
    .map((a) => a.trim())
    .filter((a) => a.length > 0);
}

function onName(e: Event) {
  emit("update", "name", (e.target as HTMLInputElement).value);
  emit("edit");
}
</script>

<template>
  <article class="project-card">
    <header class="card-head">
      <label class="name-field">
        <span class="label">项目名称</span>
        <input
          type="text"
          :value="project.name"
          placeholder="新项目"
          @input="onName"
        />
      </label>
      <button type="button" class="delete" @click="emit('delete')">删除</button>
    </header>

    <div v-for="g in PROJECT_GROUPS" :key="g.id" class="group">
      <h4 class="group-title">{{ g.title }}</h4>
      <ConfigField
        v-for="def in g.fields"
        :key="def.key"
        :def="def"
        :model-value="fieldValue(def)"
        @update:model-value="setField(def, $event)"
        @edit="emit('edit')"
      />
    </div>
  </article>
</template>

<style scoped>
.project-card {
  display: flex;
  flex-direction: column;
  gap: var(--space-5);
  padding: var(--space-6);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-md);
}
.card-head {
  display: flex;
  align-items: flex-end;
  justify-content: space-between;
  gap: var(--space-4);
  padding-bottom: var(--space-4);
  border-bottom: 1px solid var(--color-border);
}
.name-field {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  flex: 1;
}
.name-field .label {
  font-size: var(--font-size-sm);
  color: var(--color-text);
}
.name-field input {
  padding: var(--space-2) var(--space-3);
  font: inherit;
  font-size: var(--font-size-md);
  color: var(--color-text);
  background: var(--color-surface);
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-sm);
}
.name-field input:focus {
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
.group {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.group-title {
  margin: 0;
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
}
</style>
