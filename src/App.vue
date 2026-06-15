<script setup lang="ts">
// Composition-root layout: wires the slice views together. Slices own their
// own UI + state; App only arranges them.
import { computed, onMounted, ref } from "vue";
import { appVersion } from "./config/api";
import ConfigPanel from "./config/ConfigPanel.vue";
import PollControls from "./pr/PollControls.vue";
import PrList from "./pr/PrList.vue";
import { usePrStore } from "./pr/usePrStore";
import StatusBar from "./StatusBar.vue";
import ReviewPanel from "./review/ReviewPanel.vue";
import ReviewSessions from "./review/ReviewSessions.vue";
import { useReviewStore } from "./review/useReviewStore";
import { reschedule } from "./pr/api";
import type { PullRequestView } from "./types";

const version = ref("");
onMounted(async () => {
  version.value = await appVersion();
});

// Login / availability banner (composition layer only): #8 auto-triggers reviews,
// which silently stall if `gh` isn't authenticated or codex is unavailable.
// Reading BOTH slices' status (gh from the pr store, codex from the review store)
// is legitimate here — App is the cross-slice wiring point, exactly like StatusBar.
// Hidden while either status is still loading (null) so a cold start doesn't flash
// a false warning; shown only on a confirmed unavailable signal.
const prStore = usePrStore();
const { codex } = useReviewStore();
const ghBlocked = computed(() => prStore.gh?.authenticated === false);
const codexBlocked = computed(() => codex.value?.available === false);
const showPrompt = computed(() => ghBlocked.value || codexBlocked.value);

// Cross-slice wiring (composition root only): a config save may change the poll
// interval, so reschedule the backend timer.
function onConfigSaved() {
  // Non-blocking: a reschedule failure only delays the period rebuild (the next
  // poll still runs on the old period), so log it rather than surfacing/throwing.
  reschedule().catch((e) => console.error("reschedule failed", e));
}

// Cross-slice wiring: the pr slice selects a PR, the review slice reviews it.
// Holding the selection here keeps the two slices decoupled (neither imports the
// other) — App passes it down to both PrList (highlight) and ReviewPanel (target).
const selectedPr = ref<PullRequestView | null>(null);
</script>

<template>
  <div class="app">
    <header class="app-bar">
      <strong>prmonitor</strong>
      <small v-if="version">v{{ version }}</small>
    </header>
    <div class="layout">
      <aside class="sidebar">
        <ConfigPanel @saved="onConfigSaved" />
        <PollControls />
        <PrList
          :selected-number="selectedPr?.number ?? null"
          @select="(pr) => (selectedPr = pr)"
        />
      </aside>
      <main class="content">
        <div v-if="showPrompt" class="availability" role="alert">
          自动 review 已暂停 —
          <template v-if="ghBlocked">
            gh 未登录，请运行 <code>gh auth login</code>
          </template>
          <template v-if="ghBlocked && codexBlocked"> ；</template>
          <template v-if="codexBlocked">
            codex 不可用（{{ codex?.message }}）
          </template>
        </div>
        <ReviewSessions />
        <ReviewPanel :selected-pr="selectedPr" />
      </main>
    </div>
    <StatusBar />
  </div>
</template>

<style>
:root {
  font-family: Inter, Avenir, Helvetica, Arial, sans-serif;
  color: #0f0f0f;
  background-color: #f6f6f6;
}

body {
  margin: 0;
}

@media (prefers-color-scheme: dark) {
  :root {
    color: #f6f6f6;
    background-color: #2f2f2f;
  }
}
</style>

<style scoped>
.app {
  display: flex;
  flex-direction: column;
  height: 100vh;
}

.app-bar {
  display: flex;
  align-items: baseline;
  gap: 8px;
  padding: 10px 16px;
  border-bottom: 1px solid rgba(128, 128, 128, 0.3);
}

.app-bar small {
  color: #888;
}

.layout {
  display: flex;
  flex: 1;
  min-height: 0;
}

.sidebar {
  width: 320px;
  border-right: 1px solid rgba(128, 128, 128, 0.3);
  overflow-y: auto;
  padding: 12px;
}

.content {
  flex: 1;
  overflow-y: auto;
  padding: 12px;
}

.availability {
  margin-bottom: 12px;
  padding: 8px 12px;
  border: 1px solid rgba(224, 160, 0, 0.5);
  border-radius: 4px;
  background: rgba(224, 160, 0, 0.1);
  font-size: 13px;
  color: #b87900;
}

.availability code {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: 12px;
}
</style>
