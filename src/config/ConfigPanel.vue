<script setup lang="ts">
// Config slice view: editable form over the persisted AppConfig. Edits land on a
// local `draft` (decoupled from the store) and are committed via store.save();
// the draft is (re)hydrated whenever the store's loaded config arrives/changes.
import { onMounted, reactive, ref, watch } from "vue";
import { useConfigStore } from "./useConfigStore";
import type { AppConfig } from "./types";

const store = useConfigStore();

// Editable draft. `authors` is exposed to the user as a comma-joined string and
// normalized back to string[] on save.
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

// Clear the "Saved." banner / stale error the moment the user resumes editing.
// Fires only on real DOM input, not the programmatic hydrate() above.
function onEdit() {
  store.savedOk = false;
  store.error = null;
}

function onSave() {
  const authors = authorsInput.value
    .split(",")
    .map((a) => a.trim())
    .filter((a) => a.length > 0);
  store.save({ ...draft, authors });
}
</script>

<template>
  <section class="config-panel">
    <h2>Config</h2>

    <p v-if="store.loading" class="muted">Loading…</p>
    <p v-else-if="store.error && !store.config" class="error">{{ store.error }}</p>

    <form v-else class="form" @submit.prevent="onSave" @input="onEdit">
      <label>
        <span>Repo</span>
        <input v-model="draft.repo" type="text" />
      </label>

      <label>
        <span>Repo root</span>
        <input v-model="draft.repoRoot" type="text" />
      </label>

      <label>
        <span>Review label</span>
        <input v-model="draft.reviewLabel" type="text" />
      </label>

      <label>
        <span>Check label</span>
        <input v-model="draft.checkLabel" type="text" />
      </label>

      <label>
        <span>Skill rel path</span>
        <input v-model="draft.skillRelPath" type="text" />
      </label>

      <label>
        <span>Poll interval (secs)</span>
        <input v-model.number="draft.pollIntervalSecs" type="number" min="1" />
      </label>

      <label>
        <span>PR cooldown (secs)</span>
        <input v-model.number="draft.prCooldownSeconds" type="number" min="1" />
      </label>

      <label>
        <span>Authors (comma-separated)</span>
        <input v-model="authorsInput" type="text" />
      </label>

      <label>
        <span>Source</span>
        <!-- #11: add <option>s here as SourceKind widens (gitlab / bitbucket). -->
        <select v-model="draft.sourceKind">
          <option value="github">github</option>
        </select>
      </label>

      <label>
        <span>Engine</span>
        <!-- #11: add <option>s here as EngineKind widens (claude). -->
        <select v-model="draft.engineKind">
          <option value="codex">codex</option>
        </select>
      </label>

      <button type="submit" :disabled="store.saving">
        {{ store.saving ? "Saving…" : "Save" }}
      </button>

      <p v-if="store.error" class="error">{{ store.error }}</p>
      <p v-else-if="store.savedOk" class="ok">Saved.</p>
    </form>
  </section>
</template>

<style scoped>
.config-panel {
  margin-bottom: 16px;
}
.muted {
  color: #888;
}
.form {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.form label {
  display: flex;
  flex-direction: column;
  gap: 2px;
  font-size: 12px;
}
.form input,
.form select {
  padding: 4px 6px;
}
.form button {
  margin-top: 4px;
  padding: 6px;
}
.error {
  color: #c00;
  font-size: 12px;
}
.ok {
  color: #2a7;
  font-size: 12px;
}
</style>
