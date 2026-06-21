<script setup lang="ts">
// App-shell status strip (composition layer, sibling of App.vue): surfaces the
// `gh` CLI auth state (pr store) and codex availability (review store). Living at
// the shell layer — not inside a slice — is what makes reading across both slices
// legitimate, exactly as App.vue does its cross-slice wiring.
import { computed, onMounted, watch } from "vue";
import { useConfigStore } from "./config/useConfigStore";
import { githubCliStatusRelevantForSource } from "./types";
import { usePrStore } from "./pr/usePrStore";
import { useReviewStore } from "./review/useReviewStore";

const store = usePrStore();
const configStore = useConfigStore();
// Destructure the codex ref so the template auto-unwraps it (the review store is
// a plain factory object, not a Pinia store, so `review.codex` would stay a Ref).
const { codex, refreshCodexStatus, startCodexServer, stopCodexServer } =
  useReviewStore();
const ghRequired = computed(
  () =>
    configStore.config?.projects.some(
      (p) =>
        p.enabled &&
        githubCliStatusRelevantForSource(p.sourceKind, p.updateMode),
    ) ?? false,
);
// Self-refresh codex status on mount so the StatusBar shows the real state even
// when the user never opened ReviewPanel. Only refresh when still null to avoid
// clobbering a fresher value from another source (e.g. ReviewPanel's own poll).
onMounted(() => {
  if (codex.value == null) refreshCodexStatus();
});
watch(
  ghRequired,
  (required) => {
    if (required) void store.refreshGhStatus();
  },
  { immediate: true },
);
</script>

<template>
  <footer class="status-bar">
    <span class="item">
      <span
        class="dot"
        :class="
          !ghRequired
            ? 'idle'
            : store.gh == null
              ? 'idle'
              : store.gh.authenticated
                ? 'ok'
                : 'warn'
        "
      ></span>
      <span class="text">
        gh —
        {{
          !ghRequired
            ? "当前配置不需要"
            : (store.gh?.message ?? "检查中… / checking")
        }}
      </span>
    </span>

    <span class="item">
      <span
        class="dot"
        :class="
          codex == null
            ? 'idle'
            : codex.desiredRunning === false
              ? 'idle'
              : codex.available
                ? 'ok'
                : 'warn'
        "
      ></span>
      <span class="text">
        codex — {{ codex == null ? "初始化中… / starting" : codex.message }}
      </span>
      <button
        v-if="codex != null"
        type="button"
        class="codex-btn"
        @click="
          codex.desiredRunning === false
            ? startCodexServer()
            : stopCodexServer()
        "
      >
        {{ codex.desiredRunning === false ? "启动" : "停止" }}
      </button>
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
.codex-btn {
  margin-left: var(--space-3);
  padding: 0 var(--space-3);
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-sm);
  background: transparent;
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
  line-height: 1.6;
  cursor: pointer;
}
.codex-btn:hover {
  color: var(--color-text);
  border-color: var(--color-text-muted);
}
</style>
