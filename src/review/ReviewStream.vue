<script setup lang="ts">
// Renders the aggregated review stream: message items expanded, reasoning items
// collapsed into a <details>. Presentational — items are aggregated by the store.
import type { StreamItem } from "./types";

defineProps<{ items: StreamItem[] }>();
</script>

<template>
  <div class="stream">
    <template v-for="item in items" :key="item.itemId">
      <details v-if="item.kind === 'reasoning'" class="reasoning">
        <summary>reasoning</summary>
        <pre class="text">{{ item.text }}</pre>
      </details>
      <pre v-else class="message text">{{ item.text }}</pre>
    </template>
  </div>
</template>

<style scoped>
.stream {
  margin-top: 8px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.text {
  margin: 0;
  white-space: pre-wrap;
  word-break: break-word;
  font: inherit;
}
.message {
  line-height: 1.5;
}
.reasoning {
  border-left: 2px solid rgba(128, 128, 128, 0.3);
  padding-left: 8px;
}
.reasoning summary {
  cursor: pointer;
  color: #888;
  font-size: 12px;
}
.reasoning .text {
  margin-top: 4px;
  color: #888;
}
</style>
