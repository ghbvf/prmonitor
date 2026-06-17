<script setup lang="ts">
// Review-sessions list: the concurrent-aware companion to ReviewPanel's single
// focused stream. #8 can auto-trigger several review sessions at once; this lists
// them all — running AND finished (the backend's `list_review_sessions` returns
// both) — from the shared store's `sessions` ref, and lets the user point the
// focused panel at any one. Reads the module-level singleton store — no second
// instance, no props. Mirrors PrList/PrRow's badge + muted conventions.
import { computed } from "vue";
import { useProjects } from "../projects";
import { useReviewStore } from "./useReviewStore";
import type { SessionStatus } from "./types";

const { sessions, activeThreadId, focus } = useReviewStore();
const { activeProjectId } = useProjects();

// Scope the list to the active project (#35): the store tracks every project's
// sessions, but the panel only ever focuses one project's at a time.
const visibleSessions = computed(() =>
  sessions.value.filter((s) => s.projectId === activeProjectId.value),
);

// Bilingual label for each session lifecycle status (mirrors ReviewPanel's
// finalLabel style). Default keeps the raw value so a new SessionStatus still
// renders something rather than blank.
function statusLabel(status: SessionStatus): string {
  switch (status) {
    case "starting":
      return "启动中 / starting";
    case "running":
      return "运行中 / running";
    case "interrupting":
      return "停止中 / interrupting";
    case "done":
      return "完成 / done";
    case "failed":
      return "失败 / failed";
    default:
      return status;
  }
}
</script>

<template>
  <section class="review-sessions">
    <header class="head">
      <h2>Review 会话 / Review sessions</h2>
    </header>

    <p v-if="visibleSessions.length === 0" class="muted">
      暂无会话 / No review sessions
    </p>

    <ul v-else class="rows">
      <li
        v-for="s in visibleSessions"
        :key="s.threadId"
        class="session-row"
        :class="{ focused: s.threadId === activeThreadId }"
        @click="focus(s.threadId, s.prNumber, s.status)"
      >
        <span class="pr">PR #{{ s.prNumber }}</span>
        <span class="badge kind">{{ s.kind }}</span>
        <span class="badge status" :class="`status-${s.status}`">
          {{ statusLabel(s.status) }}
        </span>
      </li>
    </ul>
  </section>
</template>

<style scoped>
.review-sessions {
  margin-bottom: var(--space-8);
}
.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
}
.head h2 {
  margin: 0;
  font-size: var(--font-size-lg);
}
.muted {
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
}
.rows {
  list-style: none;
  margin: var(--space-4) 0 0;
  padding: 0;
}
.session-row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3) var(--space-4);
  border-bottom: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.session-row:hover {
  background: var(--color-surface-hover);
}
.session-row.focused {
  background: var(--color-accent-bg);
}
.pr {
  font-size: var(--font-size-md);
}
.badge {
  display: inline-block;
  padding: 1px var(--space-3);
  border-radius: var(--radius-md);
  font-size: var(--font-size-xs);
  line-height: 1.5;
}
.badge.kind {
  background: var(--color-accent-badge-bg);
  color: var(--color-accent);
}
.badge.status {
  background: var(--color-neutral-bg);
  color: inherit;
}
/* Distinct status styling: in-flight states tinted, terminal states colored. */
.badge.status-running,
.badge.status-starting {
  background: var(--color-success-bg);
  color: var(--color-success);
}
.badge.status-interrupting {
  background: var(--color-warn-bg-strong);
  color: var(--color-warn);
}
.badge.status-done {
  background: var(--color-neutral-bg);
  color: var(--color-text-muted);
}
.badge.status-failed {
  background: var(--color-danger-bg);
  color: var(--color-danger);
}
</style>
