<script setup lang="ts">
// Per-tunnel editor card (AB#1064): renders ONE tunnel's editable fields via the shared
// ConfigField over fields.ts TUNNEL_GROUPS. Mirrors ProjectCard/ListenerCard — identity
// (name) and the delete action live in the RemoteAccessManager row header, not here, so
// this stays a pure field form. Stateless about persistence: it emits granular `update` to
// the parent (RemoteAccessManager), which owns the AppConfig draft and the save flow.
//
// No csv buffer needed — a Tunnel has no string[] field (unlike ProjectCard's authors /
// ListenerCard's allowedOrigins), so every field round-trips its native value.
import { computed } from "vue";
import type { Listener, Tunnel } from "./types";
import { TUNNEL_GROUPS, type FieldDef, type TunnelFieldKey } from "./fields";
import ConfigField from "./ConfigField.vue";

const props = defineProps<{ tunnel: Tunnel; listeners: Listener[] }>();
const emit = defineEmits<{
  // A single per-tunnel field changed (key + new value). No csv/string[] field on Tunnel,
  // so every key carries its native value type.
  update: [key: TunnelFieldKey, value: string | number | boolean | string[]];
  edit: [];
}>();

// targetListenerId renders as a select sourced from the current listeners (codex F4) — the
// user picks from the live listeners instead of typing an opaque uuid. Built as a
// render-time FieldDef override (kind "select" + options/optionLabels) over the existing
// "text" def in fields.ts, so ConfigField's existing select rendering handles it WITHOUT a
// new FieldDef capability or ConfigField kind. A leading "" placeholder option lets an
// unset tunnel show blank; with zero listeners only the placeholder shows. `options` carries
// the listener ids (the wire VALUE, matching targetListenerId's contract); optionLabels maps
// id→name (falling back to the id when a listener has no name).
const targetListenerSelect = computed(() => {
  const options = ["", ...props.listeners.map((l) => l.id)];
  const optionLabels: Record<string, string> = { "": "（未选择）" };
  for (const l of props.listeners) optionLabels[l.id] = l.name || l.id;
  return { options, optionLabels };
});

// The FieldDef to render for `def`: the targetListenerId override (text → select sourced
// from listeners), else the def straight from TUNNEL_GROUPS unchanged.
function renderDef(def: FieldDef): FieldDef {
  if (def.key === "targetListenerId") {
    return {
      ...def,
      kind: "select",
      options: targetListenerSelect.value.options,
      optionLabels: targetListenerSelect.value.optionLabels,
    };
  }
  return def;
}

// Read a field's value for binding straight off the bound tunnel.
function fieldValue(def: FieldDef): string | number | boolean | string[] {
  return props.tunnel[def.key as TunnelFieldKey];
}

// Apply an edit — emit the native value straight through (no csv normalization).
function setField(def: FieldDef, value: string | number | boolean | string[]) {
  emit("update", def.key as TunnelFieldKey, value);
}
</script>

<template>
  <article class="tunnel-card">
    <div v-for="g in TUNNEL_GROUPS" :key="g.id" class="group">
      <ConfigField
        v-for="def in g.fields"
        :key="def.key"
        :def="renderDef(def)"
        :model-value="fieldValue(def)"
        @update:model-value="setField(def, $event)"
        @edit="emit('edit')"
      />
    </div>
  </article>
</template>

<style scoped>
.tunnel-card {
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
