<script setup lang="ts">
// Composition-root layout: wires the slice views together. Slices own their
// own UI + state; App only arranges them.
import { onMounted, ref } from "vue";
import { appVersion } from "./api";
import ConfigPanel from "./config/ConfigPanel.vue";
import PrList from "./pr/PrList.vue";
import ReviewPanel from "./review/ReviewPanel.vue";

const version = ref("");
onMounted(async () => {
  version.value = await appVersion();
});
</script>

<template>
  <div class="app">
    <header class="app-bar">
      <strong>prmonitor</strong>
      <small v-if="version">v{{ version }}</small>
    </header>
    <div class="layout">
      <aside class="sidebar">
        <ConfigPanel />
        <PrList />
      </aside>
      <main class="content">
        <ReviewPanel />
      </main>
    </div>
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
</style>
