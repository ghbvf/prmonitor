<script setup lang="ts">
// Review-sessions list: the concurrent-aware companion to ReviewPanel's single
// focused stream. #8 can auto-trigger several review sessions at once; this lists
// them all — running AND finished (the backend's `list_review_sessions` returns
// both) — from the shared store's `sessions` ref, and lets the user point the
// focused panel at any one. Reads the module-level singleton store — no second
// instance, no props. Mirrors PrList/PrRow's badge + muted conventions.
import { useReviewStore } from "./useReviewStore";
import type { SessionStatus } from "./types";

const { sessions, activeThreadId, focus } = useReviewStore();

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

    <p v-if="sessions.length === 0" class="muted">暂无会话 / No review sessions</p>

    <ul v-else class="rows">
      <li
        v-for="s in sessions"
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
  margin-bottom: 16px;
}
.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}
.head h2 {
  margin: 0;
  font-size: 15px;
}
.muted {
  color: #888;
  font-size: 12px;
}
.rows {
  list-style: none;
  margin: 8px 0 0;
  padding: 0;
}
.session-row {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 6px 8px;
  border-bottom: 1px solid rgba(128, 128, 128, 0.15);
  border-radius: 4px;
  cursor: pointer;
}
.session-row:hover {
  background: rgba(128, 128, 128, 0.08);
}
.session-row.focused {
  background: rgba(37, 99, 235, 0.12);
}
.pr {
  font-size: 13px;
}
.badge {
  display: inline-block;
  padding: 1px 6px;
  border-radius: 8px;
  font-size: 11px;
  line-height: 1.5;
}
.badge.kind {
  background: rgba(37, 99, 235, 0.15);
  color: #2563eb;
}
.badge.status {
  background: rgba(128, 128, 128, 0.18);
  color: inherit;
}
/* Distinct status styling: in-flight states tinted, terminal states colored. */
.badge.status-running,
.badge.status-starting {
  background: rgba(42, 119, 0, 0.15);
  color: #2a7700;
}
.badge.status-interrupting {
  background: rgba(224, 160, 0, 0.18);
  color: #b87900;
}
.badge.status-done {
  background: rgba(128, 128, 128, 0.18);
  color: #888;
}
.badge.status-failed {
  background: rgba(204, 0, 0, 0.12);
  color: #c00;
}
</style>
