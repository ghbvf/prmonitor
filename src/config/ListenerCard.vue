<script setup lang="ts">
// Per-listener editor card (AB#1064): renders ONE listener's editable fields via the
// shared ConfigField over fields.ts LISTENER_GROUPS. Mirrors ProjectCard — identity
// (name) and the delete action live in the RemoteAccessManager row header, not here, so
// this stays a pure field form. Stateless about persistence: it emits granular `update`
// to the parent (RemoteAccessManager), which owns the AppConfig draft and the save flow.
//
// Each card owns its OWN `allowedOriginsInput` csv buffer (a local ref keyed off this
// card's listener) so N cards never share origin state — exactly how ProjectCard handles
// the `authors` string[] via csv, re-seeded only on id change.
import { ref, watch } from "vue";
import type { Listener } from "./types";
import { LISTENER_GROUPS, type FieldDef, type ListenerFieldKey } from "./fields";
import { normalizeAllowedOrigins } from "./remoteAccessOps";
import ConfigField from "./ConfigField.vue";

const props = defineProps<{ listener: Listener }>();
const emit = defineEmits<{
  // A single per-listener field changed (key + new value). `allowedOrigins` arrives
  // already normalized to string[]; every other key carries its native value type.
  update: [key: ListenerFieldKey, value: string | number | boolean | string[]];
  edit: [];
}>();

// This card's own comma-joined allowedOrigins buffer. Re-seeded ONLY on id change (the
// card now bound to a different listener — delete/reorder); while the id is stable the
// buffer is local-first (the card emits normalized string[] up on every edit, so
// re-seeding from props would fight the user's in-progress typing). Mirrors ProjectCard's
// authorsInput.
const allowedOriginsInput = ref(props.listener.allowedOrigins.join(", "));
watch(
  () => props.listener.id,
  () => {
    allowedOriginsInput.value = props.listener.allowedOrigins.join(", ");
  },
);

// Read a field's value for binding. `allowedOrigins` surfaces as this card's comma-joined
// buffer; every other key reads straight off the bound listener.
function fieldValue(def: FieldDef): string | number | boolean | string[] {
  if (def.key === "allowedOrigins") return allowedOriginsInput.value;
  return props.listener[def.key as ListenerFieldKey];
}

// Apply an edit. `allowedOrigins` is held locally as a csv string and emitted normalized
// to string[]; every other field emits its native value straight through.
function setField(def: FieldDef, value: string | number | boolean | string[]) {
  const key = def.key as ListenerFieldKey;
  if (key === "allowedOrigins") {
    allowedOriginsInput.value = Array.isArray(value) ? value.join(", ") : String(value);
    emit("update", "allowedOrigins", allowedOriginsArray());
    return;
  }
  emit("update", key, value);
}

// Normalize this card's csv buffer to the allowedOrigins string[] wire shape. The
// split/trim/drop-empties rule lives in normalizeAllowedOrigins (remoteAccessOps.ts) so
// it's unit-testable without a component harness (codex F6).
function allowedOriginsArray(): string[] {
  return normalizeAllowedOrigins(allowedOriginsInput.value);
}
</script>

<template>
  <article class="listener-card">
    <div v-for="g in LISTENER_GROUPS" :key="g.id" class="group">
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
.listener-card {
  display: flex;
  flex-direction: column;
  gap: var(--space-5);
  padding: var(--space-6);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-md);
}
.group {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
</style>
