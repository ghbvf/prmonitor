<script setup lang="ts">
// PR poll controls slice view: the manual "立即拉取" trigger, the pause/resume
// toggle, and the last-pull readout. The store owns the command/event plumbing;
// this component subscribes to the `prs:updated` stream on mount and tears the
// subscription down on unmount.
import { computed, onMounted, onUnmounted } from "vue";
import { usePrStore } from "./usePrStore";
import { useProjects } from "../projects";
import { manualPullAllowedForMode, pollingEnabledForMode } from "../types";

const store = usePrStore();
const { activeProjectId, projects } = useProjects();

// The active project's update mode (818), or null when no project is resolved.
const activeMode = computed(
  () => projects.value.find((x) => x.id === activeProjectId.value)?.updateMode ?? null,
);

// Two DISTINCT capability gates (818, F7):
// - manualPullEnabled gates "立即拉取" (one-shot poll-now): allowed for everything except
//   webhook-only — manual mode runs a backend one-shot `discover_once` on demand.
// - periodicPollEnabled gates "暂停/恢复轮询" (the periodic loop): only pull-only / hybrid.
// Both helpers are exhaustive over UpdateMode (assertNever), so a new mode can't slip
// past either gate. Default to false when no project is resolved.
const manualPullEnabled = computed(() =>
  activeMode.value ? manualPullAllowedForMode(activeMode.value) : false,
);
const periodicPollEnabled = computed(() =>
  activeMode.value ? pollingEnabledForMode(activeMode.value) : false,
);

// Backstop poll-status refresh cadence: a fully-idle/failing loop emits no events, so
// re-query the heartbeat/error line on this interval while mounted.
const POLL_STATUS_REFRESH_MS = 10_000;

const lastPulledText = computed(() =>
  store.lastPulledAtActive === null
    ? "从未"
    : new Date(store.lastPulledAtActive).toLocaleTimeString(),
);

// Backend poll-loop diagnostics (#62), distinct from the optimistic `pollingActive`
// toggle: this reflects what the loop ACTUALLY did (heartbeat / last success / last
// error), so a running-but-silently-failing loop is visible. `*Epoch` fields are
// UNIX seconds — *1000 for new Date().
const poll = computed(() => store.pollStatusActive);

// Format an epoch-secs value as a local time, or「从未」when absent.
function epochTime(epoch: number | null | undefined): string {
  return epoch == null ? "从未" : new Date(epoch * 1000).toLocaleTimeString();
}

const lastSuccessText = computed(() => epochTime(poll.value?.lastSuccessEpoch));

// Friendlier interval readout: whole minutes render as「N 分钟」, anything else as raw
// seconds.
function formatInterval(secs: number): string {
  return secs >= 60 && secs % 60 === 0 ? `${secs / 60} 分钟` : `${secs}s`;
}

// Show the failure line only when there's an error AND it's the latest signal: no
// success yet, or the error epoch is newer than the last success (a recovered loop
// shouldn't keep nagging about a stale error).
const showError = computed(() => {
  const p = poll.value;
  if (!p || p.lastErrorEpoch == null) return false;
  return p.lastSuccessEpoch == null || p.lastErrorEpoch > p.lastSuccessEpoch;
});

// "运行中但长时间未成功": the loop claims running but its progress has stalled — a
// subtle warning even with no hard error recorded. Gated against the first-cycle
// false positive: a just-started loop with no success yet is NOT stalled until it's
// been running past one interval (use lastStartedEpoch to tell "never started a
// cycle" from "first cycle overran").
const stalled = computed(() => {
  const p = poll.value;
  if (!p || !p.running) return false;
  const nowSecs = Date.now() / 1000;
  if (p.lastSuccessEpoch == null) {
    // No success yet: not stalled until a cycle has actually been in flight longer
    // than one interval. No lastStartedEpoch → never entered a cycle → not stalled.
    if (p.lastStartedEpoch == null) return false;
    return nowSecs - p.lastStartedEpoch > Math.max(p.intervalSecs, 60);
  }
  // Stale success: flag only once the last success is older than ~2 intervals, so a
  // fresh success isn't mistaken for a stall.
  const ageSecs = nowSecs - p.lastSuccessEpoch;
  return ageSecs > Math.max(p.intervalSecs * 2, 60);
});

// Hold the resolved UnlistenFn so onUnmounted can invoke it.
// init() subscribes (awaits the listener registration) then baselines the list
// from the backend snapshot, closing the startup lost-event race (#27 F3).
// As an async action, init() flattens its inner Promise<UnlistenFn>, so awaiting
// it yields the UnlistenFn itself.
let unlisten: Awaited<ReturnType<typeof store.init>> | null = null;
// Backstop refresh: the store refreshes poll status on each prs:updated event and on
// toggle, but a fully-idle/failing loop emits nothing — poll the backend every ~10s
// while mounted so the heartbeat/error line keeps up to date. Cleared on unmount.
let pollTimer: ReturnType<typeof setInterval> | null = null;
onMounted(async () => {
  unlisten = await store.init();
  pollTimer = setInterval(() => {
    if (activeProjectId.value) store.refreshPollStatus(activeProjectId.value);
  }, POLL_STATUS_REFRESH_MS);
});
onUnmounted(() => {
  unlisten?.();
  if (pollTimer !== null) clearInterval(pollTimer);
});
</script>

<template>
  <section class="poll-controls">
    <div class="actions">
      <!-- 立即拉取: one-shot poll-now — allowed for all modes except webhook-only (manual
           runs a backend one-shot discover_once). Gated on manualPullEnabled, NOT the
           periodic gate. The pollingActive guard only applies once periodic polling is
           on (manual has no running loop to pause), so don't block manual on it. -->
      <button
        type="button"
        :disabled="
          !manualPullEnabled ||
          store.loadingActive ||
          (periodicPollEnabled && !store.pollingActive)
        "
        @click="store.pollNow(activeProjectId)"
      >
        {{ store.loadingActive ? "拉取中…" : "立即拉取" }}
      </button>
      <!-- 暂停/恢复轮询: periodic loop control — only pull-only / hybrid have a loop. -->
      <button
        type="button"
        :disabled="!periodicPollEnabled || store.loadingActive"
        @click="store.toggle()"
      >
        {{ store.pollingActive ? "暂停轮询" : "恢复轮询" }}
      </button>
    </div>
    <p v-if="!periodicPollEnabled" class="muted">
      当前项目未启用 CLI 定时轮询（仅 pull/hybrid 模式有定时拉取）。
    </p>
    <p class="muted">上次拉取：{{ lastPulledText }}</p>
    <div v-if="poll" class="poll-status">
      <p class="muted status-line">
        轮询循环：
        <span v-if="poll.running" class="running">运行中</span>
        <span v-else class="paused">已暂停</span>
        <span class="hint">· 间隔 {{ formatInterval(poll.intervalSecs) }}</span>
      </p>
      <p class="muted">最近成功：{{ lastSuccessText }}</p>
      <p v-if="stalled" class="warn">运行中但长时间未成功，请检查认证 / 网络。</p>
      <p v-if="showError" class="error">
        最近失败：{{ poll.lastErrorMessage ?? "未知错误" }}
      </p>
    </div>
  </section>
</template>

<style scoped>
.poll-controls {
  margin-bottom: var(--space-8);
}
.actions {
  display: flex;
  gap: var(--space-4);
}
.actions button {
  padding: var(--space-2) var(--space-4);
  font-size: var(--font-size-sm);
}
.muted {
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
  margin: var(--space-4) 0 0;
}
.poll-status {
  margin-top: var(--space-2);
}
.poll-status p {
  margin: var(--space-2) 0 0;
}
.status-line {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.running {
  color: var(--color-success);
  font-weight: 600;
}
.paused {
  color: var(--color-text-muted);
}
.hint {
  color: var(--color-text-muted);
  font-size: var(--font-size-xs);
}
.warn {
  color: var(--color-warn);
  font-size: var(--font-size-sm);
  margin: var(--space-2) 0 0;
}
.error {
  color: var(--color-danger);
  font-size: var(--font-size-sm);
  margin: var(--space-2) 0 0;
}
</style>
