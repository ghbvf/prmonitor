<script setup lang="ts">
// Thin status strip: surfaces the `gh` CLI auth state (sourced from the pr store)
// plus a codex placeholder. The codex status becomes real once the review slice
// lands; this component is a placeholder for it for now.
import { usePrStore } from "./usePrStore";

const store = usePrStore();
</script>

<template>
  <footer class="status-bar">
    <span class="item">
      <span
        class="dot"
        :class="store.gh?.authenticated ? 'ok' : 'warn'"
      ></span>
      <span class="text">gh — {{ store.gh?.message ?? "未知 / unknown" }}</span>
    </span>

    <span class="item">
      <span class="dot idle"></span>
      <span class="text">codex —（真实状态随 review 切片落地，本组件暂为占位）</span>
    </span>
  </footer>
</template>

<style scoped>
.status-bar {
  display: flex;
  align-items: center;
  gap: 16px;
  padding: 6px 16px;
  border-top: 1px solid rgba(128, 128, 128, 0.3);
  font-size: 12px;
  color: #888;
}
.item {
  display: flex;
  align-items: center;
  gap: 6px;
}
.dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: #888;
}
.dot.ok {
  background: #2a7;
}
.dot.warn {
  background: #e0a000;
}
.dot.idle {
  background: #888;
}
</style>
