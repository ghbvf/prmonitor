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
  margin-top: var(--space-4);
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
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
  border-left: 2px solid var(--color-border-strong);
  padding-left: var(--space-4);
}
.reasoning summary {
  cursor: pointer;
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
}
.reasoning .text {
  margin-top: var(--space-2);
  color: var(--color-text-muted);
}
</style>
