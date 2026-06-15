<script setup lang="ts">
// Review streaming panel: starts/stops a review for the selected PR and renders
// the streamed deltas. The selected PR is passed down by the composition root
// (App.vue) so the pr slice and review slice stay decoupled.
import { computed, onMounted, onUnmounted } from "vue";
import type { PullRequestView } from "../types";
import ReviewStream from "./ReviewStream.vue";
import { useReviewStore } from "./useReviewStore";

const props = defineProps<{ selectedPr: PullRequestView | null }>();

const {
  items,
  running,
  finalStatus,
  error,
  activePr,
  listenerReady,
  listenerError,
  start,
  stop,
  refreshCodexStatus,
  init,
} = useReviewStore();

// Friendly label for the codex turn status ("interrupted" / "completed" / ...).
const finalLabel = computed(() => {
  switch (finalStatus.value) {
    case "completed":
      return "完成 / completed";
    case "interrupted":
      return "已停止 / interrupted";
    case "failed":
      return "失败 / failed";
    default:
      return finalStatus.value ?? "";
  }
});

// Attach the streamed-event listener for the panel's lifetime; hydrate codex
// availability for the StatusBar. Mirrors PollControls' mount/unmount pattern.
let unlisten: Awaited<ReturnType<typeof init>> | null = null;
onMounted(async () => {
  refreshCodexStatus();
  unlisten = await init();
});
onUnmounted(() => unlisten?.());

function onStart() {
  if (props.selectedPr) start(props.selectedPr.number, props.selectedPr.kind);
}
</script>

<template>
  <section class="review-panel">
    <header class="head">
      <h2>Review</h2>
      <div class="actions">
        <button
          type="button"
          :disabled="selectedPr == null || running || !listenerReady"
          @click="onStart"
        >
          开始 review
        </button>
        <button type="button" :disabled="!running" @click="stop">停止</button>
      </div>
    </header>

    <p class="status">
      <template v-if="activePr != null">
        PR #{{ activePr }} —
        <span v-if="running">运行中… / running</span>
        <span v-else-if="finalStatus">已结束 / {{ finalLabel }}</span>
        <span v-else-if="error">启动失败 / failed</span>
        <span v-else>未开始</span>
      </template>
      <span v-else-if="selectedPr">
        已选中 PR #{{ selectedPr.number }}（{{ selectedPr.kind }}）— 点「开始 review」
      </span>
      <span v-else class="muted">Select a PR to review.</span>
    </p>

    <p v-if="error" class="error">{{ error }}</p>
    <p v-if="listenerError" class="error">事件监听注册失败 / {{ listenerError }}</p>
    <p v-else-if="!listenerReady" class="muted">正在连接事件流… / connecting</p>

    <p v-if="running && items.length === 0" class="muted">等待输出… / waiting</p>

    <ReviewStream :items="items" />
  </section>
</template>

<style scoped>
.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}
.head h2 {
  margin: 0;
}
.actions {
  display: flex;
  gap: 6px;
}
.actions button {
  padding: 4px 10px;
  font: inherit;
  cursor: pointer;
}
.actions button:disabled {
  cursor: default;
  opacity: 0.5;
}
.status {
  margin: 8px 0 0;
  font-size: 13px;
}
.muted {
  color: #888;
}
.error {
  margin: 8px 0 0;
  color: #c00;
  font-size: 12px;
}
</style>
