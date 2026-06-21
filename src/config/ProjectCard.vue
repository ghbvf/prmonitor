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
import { computed, ref, watch } from "vue";
import type { Project } from "./types";
import { pollingEnabledForMode, type SourceKind } from "../types";
import { applySourceKindDefaults } from "./defaults";
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

// Risk banner gate (818, F8): the CLI-polling risk warning applies only to the modes
// that actually run the periodic poll loop (pull-only / hybrid) — exactly
// `pollingEnabledForMode`. webhook-only AND manual don't run the loop, so neither
// should show it (manual only does on-demand one-shot pulls, not periodic polling).
const showRiskBanner = computed(() => pollingEnabledForMode(props.project.updateMode));

// This card's own comma-joined authors buffer. Seeded from the project's authors and
// re-seeded whenever the bound project identity changes (e.g. the list reorders or a
// card is deleted and instances re-key) so the buffer never bleeds across projects.
const authorsInput = ref(props.project.authors.join(", "));
// Re-seed ONLY on id change (card now bound to a different project — delete/reorder).
// While the id is stable, authorsInput is a local-first buffer NOT re-seeded from
// props: the card emits normalized string[] back up on every edit, so props.authors
// already reflects local edits — re-seeding on every authors change would fight the
// user's in-progress typing (e.g. clobber a trailing comma).
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

// Conditional-visibility filter (818 F14): drop fields whose `visibleWhen` predicate is
// false for THIS project draft (e.g. azureOrg/azureProject hidden for a github source).
// A field without `visibleWhen` is always shown. Hidden fields keep their stored value —
// we never clear them, so flipping sourceKind back to azure restores what was typed.
function visibleFields(fields: FieldDef[]): FieldDef[] {
  return fields.filter((f) => !f.visibleWhen || f.visibleWhen(props.project));
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
  // Changing the source auto-corrects the fields a Bitbucket source requires (717): the
  // backend `validate_project` rejects the github-shaped defaults (labelSource "native",
  // updateMode webhook-only/hybrid). This card is stateless (it emits granular updates to
  // ProjectsManager rather than holding the draft), so run the shared helper on a copy and
  // emit one `update` per field it changed — keeping the bitbucket rule single-sourced with
  // OnboardingWizard instead of re-implementing it here.
  if (key === "sourceKind") {
    const next = { ...props.project };
    applySourceKindDefaults(next, value as SourceKind);
    emit("update", "sourceKind", next.sourceKind);
    if (next.labelSource !== props.project.labelSource) {
      emit("update", "labelSource", next.labelSource);
    }
    if (next.updateMode !== props.project.updateMode) {
      emit("update", "updateMode", next.updateMode);
    }
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

    <!-- Risk banner (818, F8): only pull-only / hybrid run the periodic CLI poll loop,
         which can trip account/API rate-limit risk control. Warn for those two only —
         manual (on-demand one-shot) and webhook-only don't poll periodically. -->
    <p v-if="showRiskBanner" class="risk-banner" role="alert">
      ⚠️ CLI 轮询可能触发账号/API 风控，请谨慎开启
    </p>

    <div v-for="g in PROJECT_GROUPS" :key="g.id" class="group">
      <h4 class="group-title">{{ g.title }}</h4>
      <ConfigField
        v-for="def in visibleFields(g.fields)"
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
.risk-banner {
  margin: 0;
  padding: var(--space-3) var(--space-4);
  font-size: var(--font-size-sm);
  color: var(--color-warn);
  background: var(--color-warn-bg);
  border: 1px solid var(--color-warn-border);
  border-radius: var(--radius-sm);
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
