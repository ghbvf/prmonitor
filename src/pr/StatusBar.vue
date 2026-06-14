<script setup lang="ts">
// Thin status strip: surfaces the `gh` CLI auth state (sourced from the pr store)
// plus the codex availability (sourced from the review store). This is App-shell
// assembly — the codex line reads across into the review slice by design.
import { usePrStore } from "./usePrStore";
import { useReviewStore } from "../review/useReviewStore";

const store = usePrStore();
// Destructure the codex ref so the template auto-unwraps it (the review store is
// a plain factory object, not a Pinia store, so `review.codex` would stay a Ref).
const { codex } = useReviewStore();
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
      <span
        class="dot"
        :class="codex == null ? 'idle' : codex.available ? 'ok' : 'warn'"
      ></span>
      <span class="text">
        codex — {{ codex == null ? "初始化中… / starting" : codex.message }}
      </span>
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
