<script setup lang="ts">
// PR poll controls slice view: the manual "立即拉取" trigger, the pause/resume
// toggle, and the last-pull readout. The store owns the command/event plumbing;
// this component subscribes to the `prs:updated` stream on mount and tears the
// subscription down on unmount.
import { computed, onMounted, onUnmounted } from "vue";
import { usePrStore } from "./usePrStore";

const store = usePrStore();

const lastPulledText = computed(() =>
  store.lastPulledAt === null
    ? "从未"
    : new Date(store.lastPulledAt).toLocaleTimeString(),
);

// Hold the Promise<UnlistenFn> so onUnmounted can await + invoke it.
let unlisten: ReturnType<typeof store.subscribe> | null = null;
onMounted(() => {
  unlisten = store.subscribe();
});
onUnmounted(async () => {
  if (unlisten) (await unlisten)();
});
</script>

<template>
  <section class="poll-controls">
    <div class="actions">
      <button type="button" :disabled="store.loading" @click="store.pollNow()">
        {{ store.loading ? "拉取中…" : "立即拉取" }}
      </button>
      <button type="button" @click="store.toggle()">
        {{ store.polling ? "暂停轮询" : "恢复轮询" }}
      </button>
    </div>
    <p class="muted">上次拉取：{{ lastPulledText }}</p>
  </section>
</template>

<style scoped>
.poll-controls {
  margin-bottom: 16px;
}
.actions {
  display: flex;
  gap: 8px;
}
.actions button {
  padding: 4px 8px;
  font-size: 12px;
}
.muted {
  color: #888;
  font-size: 12px;
  margin: 8px 0 0;
}
</style>
