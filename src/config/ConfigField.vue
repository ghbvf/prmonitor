<script setup lang="ts">
// Presentational field renderer (#34): renders ONE FieldDef against its draft
// value, driven by `def.kind`. Stateless — owns no draft; parents (SettingsView /
// OnboardingWizard) bind via v-model and normalize the csv<->string[] form on save.
// `edit` fires on real user input so parents can clear the saved/error banner.
import { computed } from "vue";
import type { FieldDef } from "./fields";

// modelValue is the heterogeneous draft value for this field: string (text/csv —
// csv is carried as a comma-joined string), number, or string[] (an authors array
// passed through untouched; the csv path joins/splits it). string[] is accepted so
// callers may bind the raw array and let the parent normalize.
type FieldValue = string | number | string[];

const props = defineProps<{ def: FieldDef; modelValue: FieldValue }>();
const emit = defineEmits<{
  "update:modelValue": [value: FieldValue];
  edit: [];
}>();

// csv fields surface as a comma-joined string regardless of whether the bound
// value arrives as string[] or an already-joined string.
const csvText = computed(() =>
  Array.isArray(props.modelValue)
    ? props.modelValue.join(", ")
    : String(props.modelValue ?? ""),
);

function onText(e: Event) {
  emit("update:modelValue", (e.target as HTMLInputElement).value);
  emit("edit");
}

function onNumber(e: Event) {
  // A blank/invalid number input yields NaN; coerce to 0 so it fails the
  // "必须大于 0" validation cleanly instead of serializing to JSON null and
  // confusing the backend deserializer.
  const n = (e.target as HTMLInputElement).valueAsNumber;
  emit("update:modelValue", Number.isNaN(n) ? 0 : n);
  emit("edit");
}

function onCsv(e: Event) {
  // Emit the raw comma-joined string; the parent splits/trims on save (consistent
  // with the old ConfigPanel, which joins/splits on ", ").
  emit("update:modelValue", (e.target as HTMLInputElement).value);
  emit("edit");
}

function onSelect(e: Event) {
  emit("update:modelValue", (e.target as HTMLSelectElement).value);
  emit("edit");
}
</script>

<template>
  <label class="field">
    <span class="label">{{ def.label }}</span>

    <input
      v-if="def.kind === 'text'"
      type="text"
      :value="modelValue"
      @input="onText"
    />

    <input
      v-else-if="def.kind === 'number'"
      type="number"
      min="1"
      :value="modelValue"
      @input="onNumber"
    />

    <input
      v-else-if="def.kind === 'csv'"
      type="text"
      :value="csvText"
      @input="onCsv"
    />

    <select
      v-else-if="def.kind === 'select'"
      :value="modelValue"
      :disabled="def.readonly"
      @change="onSelect"
    >
      <option v-for="opt in def.options" :key="opt" :value="opt">{{ opt }}</option>
    </select>

    <small v-if="def.hint" class="hint">{{ def.hint }}</small>
  </label>
</template>

<style scoped>
.field {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
}
.label {
  font-size: var(--font-size-sm);
  color: var(--color-text);
}
.field input,
.field select {
  padding: var(--space-2) var(--space-3);
  font: inherit;
  font-size: var(--font-size-md);
  color: var(--color-text);
  background: var(--color-surface);
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-sm);
}
.field input:focus,
.field select:focus {
  outline: none;
  border-color: var(--color-accent);
}
.field select:disabled {
  color: var(--color-text-muted);
  background: var(--color-neutral-bg);
  cursor: not-allowed;
}
.hint {
  font-size: var(--font-size-xs);
  color: var(--color-text-muted);
}
</style>
