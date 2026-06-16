<script setup lang="ts">
// Settings page (#34): a grouped editor over the persisted AppConfig. Left nav
// lists the GROUPS; the right pane renders the active group's fields via
// ConfigField. Reuses the useConfigStore draft/hydrate/save flow verbatim from the
// old ConfigPanel — edits land on a local `draft`, committed via store.save();
// the draft re-hydrates whenever the store's config arrives/changes.
import { onMounted, reactive, ref, watch } from "vue";
import { useConfigStore } from "./useConfigStore";
import type { AppConfig } from "./types";
import { GROUPS, type FieldDef, type FieldKey } from "./fields";
import ConfigField from "./ConfigField.vue";
import WebhookPanel from "../pr/WebhookPanel.vue";

const store = useConfigStore();

// `saved` lets the composition root reschedule (poll interval may have changed);
// `close` returns to the monitor view.
const emit = defineEmits<{ saved: []; close: [] }>();

// Editable draft (decoupled from the store). `authors` is surfaced as a
// comma-joined string via `authorsInput` and normalized back to string[] on save.
const draft = reactive<AppConfig>({
  repo: "",
  repoRoot: "",
  pollIntervalSecs: 0,
  authors: [],
  reviewLabel: "",
  checkLabel: "",
  skillRelPath: "",
  prCooldownSeconds: 0,
  sourceKind: "github",
  engineKind: "codex",
  autoReview: true,
  webhookEnabled: false,
  webhookPort: 8787,
  webhookSecret: "",
  cloudflaredBin: "cloudflared",
});

const authorsInput = ref("");

function hydrate(cfg: AppConfig) {
  draft.repo = cfg.repo;
  draft.repoRoot = cfg.repoRoot;
  draft.pollIntervalSecs = cfg.pollIntervalSecs;
  draft.authors = [...cfg.authors];
  draft.reviewLabel = cfg.reviewLabel;
  draft.checkLabel = cfg.checkLabel;
  draft.skillRelPath = cfg.skillRelPath;
  draft.prCooldownSeconds = cfg.prCooldownSeconds;
  draft.sourceKind = cfg.sourceKind;
  draft.engineKind = cfg.engineKind;
  draft.autoReview = cfg.autoReview;
  draft.webhookEnabled = cfg.webhookEnabled;
  draft.webhookPort = cfg.webhookPort;
  draft.webhookSecret = cfg.webhookSecret;
  draft.cloudflaredBin = cfg.cloudflaredBin;
  authorsInput.value = cfg.authors.join(", ");
}

// Populate the draft once the async config lands (and on any later replacement).
watch(
  () => store.config,
  (cfg) => {
    if (cfg) hydrate(cfg);
  },
  { immediate: true },
);

onMounted(() => store.load());

const activeGroupId = ref(GROUPS[0].id);

// Read/write a field's draft value by key. `authors` is special-cased to the
// comma-joined `authorsInput` so the csv ConfigField round-trips through one
// string source (mirrors the old ConfigPanel join/split on ", ").
function fieldValue(def: FieldDef): string | number | boolean | string[] {
  if (def.key === "authors") return authorsInput.value;
  return draft[def.key];
}

function setField(def: FieldDef, value: string | number | boolean | string[]) {
  if (def.key === "authors") {
    authorsInput.value = Array.isArray(value) ? value.join(", ") : String(value);
    return;
  }
  // Each FieldDef.kind matches its AppConfig field's value type (number kinds map
  // to numeric keys, select/text to string keys), so the per-key assignment is
  // type-correct at runtime; the cast bridges the heterogeneous emit signature.
  (draft as Record<FieldKey, unknown>)[def.key] = value;
}

// Clear the "已保存。" banner / stale error the moment the user resumes editing.
function onEdit() {
  store.savedOk = false;
  store.error = null;
}

async function onSave() {
  const authors = authorsInput.value
    .split(",")
    .map((a) => a.trim())
    .filter((a) => a.length > 0);
  // store.save() resolves regardless of outcome (it catches and sets
  // store.error / store.savedOk); gate the emit on the success flag.
  await store.save({ ...draft, authors });
  if (store.savedOk) emit("saved");
}
</script>

<template>
  <section class="settings">
    <header class="settings-header">
      <h2>设置</h2>
      <button type="button" class="back" @click="emit('close')">← 返回监控</button>
    </header>

    <p v-if="store.loading && !store.config" class="muted">加载中…</p>
    <p v-else-if="store.error && !store.config" class="error">{{ store.error }}</p>

    <div v-else class="body">
      <nav class="nav">
        <button
          v-for="g in GROUPS"
          :key="g.id"
          type="button"
          class="nav-item"
          :class="{ active: g.id === activeGroupId }"
          @click="activeGroupId = g.id"
        >
          {{ g.title }}
        </button>
      </nav>

      <form class="pane" @submit.prevent="onSave">
        <template v-for="g in GROUPS" :key="g.id">
          <div v-if="g.id === activeGroupId" class="group">
            <ConfigField
              v-for="def in g.fields"
              :key="def.key"
              :def="def"
              :model-value="fieldValue(def)"
              @update:model-value="setField(def, $event)"
              @edit="onEdit"
            />
          </div>
        </template>

        <WebhookPanel v-if="activeGroupId === 'webhook'" />

        <div class="actions">
          <button type="submit" class="primary" :disabled="store.saving">
            {{ store.saving ? "保存中…" : "保存" }}
          </button>
          <p v-if="store.error" class="error">{{ store.error }}</p>
          <p v-else-if="store.savedOk" class="ok">已保存。</p>
        </div>
      </form>
    </div>
  </section>
</template>

<style scoped>
.settings {
  display: flex;
  flex-direction: column;
  height: 100%;
  background: var(--color-bg);
  color: var(--color-text);
}
.settings-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: var(--space-4) var(--space-6);
  border-bottom: 1px solid var(--color-border);
}
.settings-header h2 {
  margin: 0;
  font-size: var(--font-size-lg);
}
.back {
  background: none;
  border: none;
  padding: var(--space-2) var(--space-3);
  font: inherit;
  font-size: var(--font-size-sm);
  color: var(--color-accent);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.back:hover {
  background: var(--color-surface-hover);
}
.muted {
  padding: var(--space-6);
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
}
.body {
  display: flex;
  flex: 1;
  min-height: 0;
}
.nav {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  width: 160px;
  flex-shrink: 0;
  padding: var(--space-4);
  border-right: 1px solid var(--color-border);
}
.nav-item {
  text-align: left;
  background: none;
  border: none;
  padding: var(--space-3) var(--space-4);
  font: inherit;
  font-size: var(--font-size-md);
  color: var(--color-text);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.nav-item:hover {
  background: var(--color-surface-hover);
}
.nav-item.active {
  background: var(--color-accent-bg);
  color: var(--color-accent);
}
.pane {
  display: flex;
  flex-direction: column;
  flex: 1;
  gap: var(--space-6);
  padding: var(--space-6);
  overflow-y: auto;
}
.group {
  display: flex;
  flex-direction: column;
  gap: var(--space-5);
  max-width: 480px;
}
.actions {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  margin-top: auto;
}
.primary {
  align-self: flex-start;
  padding: var(--space-3) var(--space-6);
  font: inherit;
  font-size: var(--font-size-md);
  color: var(--color-surface);
  background: var(--color-accent);
  border: none;
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.primary:disabled {
  opacity: 0.6;
  cursor: not-allowed;
}
.error {
  margin: 0;
  font-size: var(--font-size-sm);
  color: var(--color-danger);
}
.ok {
  margin: 0;
  font-size: var(--font-size-sm);
  color: var(--color-success);
}
</style>
