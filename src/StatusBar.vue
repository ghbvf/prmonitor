<script setup lang="ts">
// App-shell status strip (composition layer, sibling of App.vue): surfaces the
// `gh` CLI auth state (pr store) and codex availability (review store). Living at
// the shell layer — not inside a slice — is what makes reading across both slices
// legitimate, exactly as App.vue does its cross-slice wiring.
import { usePrStore } from "./pr/usePrStore";
import { useReviewStore } from "./review/useReviewStore";

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
