<script setup lang="ts">
// Review-sessions list for the SELECTED PR (#67): the independent session panel
// (sessions stay OUT of the PR box per the chosen layout). Lists that PR's sessions —
// running AND finished — sourced from the DURABLE store (`getPrSessions`, #70) so they
// survive a restart, overlaid with the live in-memory `sessions` for real-time status.
// Clicking one points the focused panel at it (and hydrates its persisted history). The
// composition root (App.vue) passes the selected PR down, keeping pr/review decoupled.
import { computed, ref, watch } from "vue";
import { getPrSessions } from "./api";
import { useReviewStore } from "./useReviewStore";
import type { ReviewSession, SessionStatus } from "./types";

const props = defineProps<{ projectId: string; prNumber: number | null }>();
const { sessions, activeThreadId, focus } = useReviewStore();

// Durable sessions for the selected PR (#70): persisted, so a PR's session list is
// restored after a restart (the in-memory `sessions` is empty then).
const durable = ref<ReviewSession[]>([]);
const loading = ref(false);
const loadError = ref<string | null>(null);

// `clear` = a PR/project SWITCH: blank the old PR's list synchronously (no stale flash,
// review F4) and show a loading state. A background reload (live `sessions` changed) keeps
// the current list visible and swaps it in on resolve — no flicker mid-stream.
async function loadDurable(clear: boolean) {
  loadError.value = null;
  if (props.prNumber == null) {
    durable.value = [];
    return;
  }
  if (clear) {
    durable.value = [];
    loading.value = true;
  }
  try {
    durable.value = await getPrSessions(props.projectId, props.prNumber);
  } catch (err) {
    console.error("加载 PR 会话列表失败", err);
    loadError.value = (err as { message?: string })?.message ?? String(err);
    if (clear) durable.value = [];
  } finally {
    loading.value = false;
  }
}

// PR/project switch → clear + load. Live session set changed (a session started /
// transitioned) → background reload, no clear.
watch(() => [props.projectId, props.prNumber], () => loadDurable(true), {
  immediate: true,
});
watch(sessions, () => loadDurable(false));

// Merge durable + live for the selected PR: a matching live session overrides the
// durable row (its status is real-time), and a brand-new live session not yet in the
// durable snapshot still appears. Sorted by `threadId` for a stable order.
const visibleSessions = computed<ReviewSession[]>(() => {
  if (props.prNumber == null) return [];
  const byThread = new Map<string, ReviewSession>();
  for (const s of durable.value) byThread.set(s.threadId, s);
  for (const s of sessions.value) {
    if (s.projectId === props.projectId && s.prNumber === props.prNumber) {
      byThread.set(s.threadId, s);
    }
  }
  return [...byThread.values()].sort((a, b) =>
    a.threadId.localeCompare(b.threadId),
  );
});

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

    <p v-if="prNumber == null" class="muted">
      选择一个 PR 查看其会话 / Select a PR to see its sessions
    </p>

    <p v-else-if="loadError" class="error">加载会话失败 / {{ loadError }}</p>

    <p v-else-if="loading && visibleSessions.length === 0" class="muted">
      加载中… / loading
    </p>

    <p v-else-if="visibleSessions.length === 0" class="muted">
      该 PR 暂无会话 / No sessions for this PR
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
.error {
  color: var(--color-danger);
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
