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
  gap: var(--space-8);
  padding: var(--space-3) var(--space-8);
  border-top: 1px solid var(--color-border-strong);
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
}
.item {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.dot {
  width: 8px;
  height: 8px;
  border-radius: var(--radius-full);
  background: var(--color-text-muted);
}
.dot.ok {
  background: var(--color-success);
}
.dot.warn {
  background: var(--color-warn-dot);
}
.dot.idle {
  background: var(--color-text-muted);
}
</style>
