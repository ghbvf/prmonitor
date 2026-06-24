<script setup lang="ts">
// Action outbox view (AB#1066): lists the outbound actions the rule engine enqueued,
// newest first, with the action's kind/summary, a status badge, the retry counters
// (attempt count / next-attempt time), the last error, a "查看原始" disclosure of the raw
// action payload, and a "重试" action on dead-lettered rows. Self-manages its
// `outbox:updated` listener over the panel's lifetime (mirrors InboxPanel's
// onMounted/onUnmounted), so App.vue mounts it without central wiring.
import { onMounted, onUnmounted, ref } from "vue";
import { assertNever, outboxKindLabel, outboxStatusLabel } from "../types";
import type { OutboxStatus } from "../types";
import { useOutboxStore } from "./useOutboxStore";

const store = useOutboxStore();

// Which entries have their raw payload disclosed; toggled per id.
const rawOpen = ref<Set<number>>(new Set());

function toggleRaw(id: number) {
  if (rawOpen.value.has(id)) {
    rawOpen.value.delete(id);
  } else {
    rawOpen.value.add(id);
    void store.fetchRaw(id); // cache-once; no-op if already loaded/in-flight
  }
  // Reassign so the reactive ref re-renders (Set mutation isn't tracked deeply).
  rawOpen.value = new Set(rawOpen.value);
}

// Map a status to a tone class for the badge — drives the design-token color below. The
// visible TEXT comes from `outboxStatusLabel` (the types.ts carrier); this only picks the
// tone, but it is itself an `assertNever`-exhaustive switch (Medium — `assertNever`穷尽,
// same carrier class as `outboxStatusLabel`): adding an OutboxStatus without a tone arm
// here is a COMPILE error, so a new status can't silently render with no badge color.
function statusTone(s: OutboxStatus): string {
  switch (s) {
    case "done":
      return "ok";
    case "dead":
      return "fail";
    case "pending":
      return "pending";
    default:
      return assertNever(s);
  }
}

// Render an epoch timestamp (seconds) as a readable local string. 0 / falsy = no scheduled
// next attempt (e.g. a done/dead entry), so show a dash rather than the Unix epoch.
function fmtTime(epoch: number): string {
  if (!epoch) return "—";
  return new Date(epoch * 1000).toLocaleString();
}

// Attach the push-stream listener for the panel's lifetime (mirrors InboxPanel). init()
// subscribes BEFORE the snapshot read, so no update racing the mount is dropped.
let unlisten: Awaited<ReturnType<typeof store.init>> | null = null;
onMounted(async () => {
  unlisten = await store.init();
});
onUnmounted(() => unlisten?.());
</script>

<template>
  <section class="outbox-panel">
    <header class="head">
      <h2>Outbox</h2>
      <button
        type="button"
        class="refresh"
        :disabled="store.loading"
        @click="store.refresh()"
      >
        {{ store.loading ? "刷新中…" : "刷新 / refresh" }}
      </button>
    </header>

    <p v-if="store.error" class="error banner">
      <span>{{ store.error }}</span>
      <button
        type="button"
        class="dismiss"
        aria-label="关闭 / dismiss"
        @click="store.error = null"
      >
        ✕
      </button>
    </p>

    <p v-if="store.loading && store.entries.length === 0" class="muted">
      加载中… / loading
    </p>

    <p v-else-if="!store.loading && store.entries.length === 0" class="muted">
      暂无动作 / No actions yet.
    </p>

    <ul class="entries">
      <li v-for="entry in store.entries" :key="entry.id" class="entry">
        <div class="row-head">
          <span class="badge type">{{ outboxKindLabel(entry.kind) }}</span>
          <span class="badge status" :class="statusTone(entry.status)">
            <!-- A `pending` entry that has already failed at least once is actively
                 retrying; surface the attempt count so the row reads as "in retry" rather
                 than a fresh queue. Terminal/first-pass states show the plain label. -->
            {{
              entry.status === "pending" && entry.attemptCount > 0
                ? `重试中 (${entry.attemptCount})`
                : outboxStatusLabel(entry.status)
            }}
          </span>
          <span class="title" :title="entry.summary">{{ entry.summary }}</span>
        </div>

        <div class="meta">
          <!-- The outbox is cross-project by design (it lists every project's outbound
               actions), so surface which project each row belongs to (mirrors InboxPanel). -->
          <span class="badge project">{{ entry.projectId }}</span>
          <span class="attempts">尝试 {{ entry.attemptCount }} 次</span>
          <span v-if="entry.status === 'pending'" class="next-attempt">
            下次 {{ fmtTime(entry.nextAttemptAt) }}
          </span>
        </div>

        <p v-if="entry.lastError" class="error fail-reason">
          失败：{{ entry.lastError }}
        </p>

        <div class="actions">
          <button
            type="button"
            class="link"
            :aria-expanded="rawOpen.has(entry.id)"
            :aria-label="`查看动作 #${entry.id} 原始 / view raw payload`"
            @click="toggleRaw(entry.id)"
          >
            {{ rawOpen.has(entry.id) ? "隐藏原始 / hide raw" : "查看原始 / view raw" }}
          </button>
          <!-- Retry is offered only on a DEAD-letter entry: a pending entry is already
               being retried by the backend loop, and a done entry is terminal-success. -->
          <button
            v-if="entry.status === 'dead'"
            type="button"
            class="link"
            :disabled="store.retryLoading[entry.id]"
            :aria-label="`重试动作 #${entry.id} / retry action`"
            @click="store.retry(entry.id)"
          >
            {{ store.retryLoading[entry.id] ? "重试中… / retrying" : "重试 / retry" }}
          </button>
        </div>

        <template v-if="rawOpen.has(entry.id)">
          <p v-if="store.rawLoading[entry.id]" class="muted raw-loading">
            加载中… / loading
          </p>
          <pre v-else-if="store.rawCache[entry.id] !== undefined" class="raw">{{ store.rawCache[entry.id] }}</pre>
          <!-- Raw fetch failed (per-entry rawError, kept off the panel banner): show the
               reason inline and offer a retry, instead of leaving a blank disclosure. -->
          <p v-else-if="store.rawError[entry.id]" class="error raw-error">
            <span>原始加载失败：{{ store.rawError[entry.id] }}</span>
            <button type="button" class="link" @click="store.fetchRaw(entry.id)">
              重试 / retry
            </button>
          </p>
        </template>
      </li>
    </ul>
  </section>
</template>

<style scoped>
.outbox-panel {
  padding: var(--space-2);
}
.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
}
.head h2 {
  margin: 0;
}
.refresh {
  padding: var(--space-2) var(--space-5);
  font: inherit;
  font-size: var(--font-size-sm);
  cursor: pointer;
}
.refresh:disabled {
  cursor: default;
  opacity: 0.5;
}
.muted {
  color: var(--color-text-muted);
}
.error {
  margin: var(--space-4) 0 0;
  color: var(--color-danger);
  font-size: var(--font-size-sm);
}
/* The panel-wide error banner carries a dismiss button; lay it out as a row. */
.error.banner {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
}
.dismiss {
  flex: none;
  padding: 0 var(--space-2);
  border: none;
  background: transparent;
  color: inherit;
  font: inherit;
  line-height: 1;
  cursor: pointer;
}
.entries {
  list-style: none;
  margin: var(--space-5) 0 0;
  padding: 0;
}
.entry {
  padding: var(--space-4);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-md);
  background: var(--color-surface);
}
.entry + .entry {
  margin-top: var(--space-4);
}
.row-head {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.title {
  flex: 1;
  min-width: 0;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.badge {
  flex: none;
  display: inline-block;
  padding: 1px var(--space-3);
  border-radius: var(--radius-md);
  font-size: var(--font-size-xs);
  line-height: 1.5;
}
.badge.type {
  background: var(--color-accent-badge-bg);
  color: var(--color-accent);
}
.badge.status.ok {
  background: var(--color-success-bg);
  color: var(--color-success);
}
.badge.status.fail {
  background: var(--color-danger-bg);
  color: var(--color-danger);
}
.badge.status.pending {
  background: var(--color-neutral-bg);
  color: var(--color-text-muted);
}
/* Per-row project identity chip (mirrors InboxPanel): neutral tone so it reads as metadata,
   not a status. */
.badge.project {
  background: var(--color-neutral-bg);
  color: var(--color-text);
}
.meta {
  margin-top: var(--space-2);
  display: flex;
  align-items: center;
  gap: var(--space-3);
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
}
.fail-reason {
  margin: var(--space-3) 0 0;
}
.actions {
  margin-top: var(--space-3);
  display: flex;
  gap: var(--space-4);
}
.link {
  padding: 0;
  font: inherit;
  font-size: var(--font-size-sm);
  color: var(--color-accent);
  background: none;
  border: none;
  cursor: pointer;
}
.link:hover {
  text-decoration: underline;
}
.link:disabled {
  opacity: 0.5;
  cursor: default;
  text-decoration: none;
}
.raw-loading {
  margin: var(--space-3) 0 0;
  font-size: var(--font-size-sm);
}
/* Inline raw-fetch failure + retry (per-entry, separate from the panel banner). */
.raw-error {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.raw {
  margin: var(--space-3) 0 0;
  padding: var(--space-3) var(--space-4);
  border-radius: var(--radius-sm);
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  font-family: var(--font-mono);
  font-size: var(--font-size-xs);
  white-space: pre-wrap;
  word-break: break-all;
  overflow-x: auto;
  max-height: 300px;
  overflow-y: auto;
}
</style>
