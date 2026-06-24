<script setup lang="ts">
// Event inbox view (AB#1065): lists the inbound webhook events the backend retained,
// newest first, with the normalized event's type/title/repo/number, a status badge, the
// failure reason, a "View raw" disclosure of the original payload, and a "Replay" action.
// Self-manages its `inbox:updated` listener over the panel's lifetime (mirrors
// ReviewPanel's onMounted/onUnmounted), so App.vue mounts it without central wiring.
import { onMounted, onUnmounted, ref } from "vue";
import { assertNever, eventTypeLabel, inboxStatusLabel } from "../types";
import type { InboxStatus } from "../types";
import { useInboxStore } from "./useInboxStore";

const store = useInboxStore();

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
// visible TEXT comes from `inboxStatusLabel` (the types.ts carrier); this only picks the
// tone, but it is itself an `assertNever`-exhaustive switch (Medium — `assertNever`穷尽,
// same carrier class as `inboxStatusLabel`): adding an InboxStatus without a tone arm here
// is a COMPILE error, so a new status can't silently render with no badge color.
function statusTone(s: InboxStatus): string {
  switch (s) {
    case "processed":
      return "ok";
    case "failed":
      return "fail";
    case "received":
      return "pending";
    default:
      return assertNever(s);
  }
}

// Attach the push-stream listener for the panel's lifetime (mirrors ReviewPanel). init()
// subscribes BEFORE the snapshot read, so no update racing the mount is dropped.
let unlisten: Awaited<ReturnType<typeof store.init>> | null = null;
onMounted(async () => {
  unlisten = await store.init();
});
onUnmounted(() => unlisten?.());
</script>

<template>
  <section class="inbox-panel">
    <header class="head">
      <h2>Inbox</h2>
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
      暂无事件 / No events yet.
    </p>

    <ul class="entries">
      <li v-for="entry in store.entries" :key="entry.id" class="entry">
        <div class="row-head">
          <span class="badge type">{{ eventTypeLabel(entry.event.eventType) }}</span>
          <span class="badge status" :class="statusTone(entry.status)">
            {{ inboxStatusLabel(entry.status) }}
          </span>
          <span class="title" :title="entry.event.title">{{ entry.event.title }}</span>
        </div>

        <div class="meta">
          <!-- The inbox is cross-project by design (it lists every project's inbound
               events), so surface which project each row belongs to (pr-review F4). -->
          <span class="badge project">{{ entry.event.projectId }}</span>
          <span class="repo">{{ entry.event.repo }}</span>
          <span v-if="entry.event.number != null" class="number">
            #{{ entry.event.number }}
          </span>
        </div>

        <p v-if="entry.status === 'failed' && entry.error" class="error fail-reason">
          失败：{{ entry.error }}
        </p>

        <div class="actions">
          <button
            type="button"
            class="link"
            :aria-expanded="rawOpen.has(entry.id)"
            :aria-label="`查看事件 #${entry.id} 原文 / view raw payload`"
            @click="toggleRaw(entry.id)"
          >
            {{ rawOpen.has(entry.id) ? "隐藏原文 / hide raw" : "查看原文 / view raw" }}
          </button>
          <button
            type="button"
            class="link"
            :disabled="store.replayLoading[entry.id]"
            :aria-label="`重放事件 #${entry.id} / replay event`"
            @click="store.replay(entry.id)"
          >
            {{ store.replayLoading[entry.id] ? "重放中… / replaying" : "重放 / replay" }}
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
            <span>原文加载失败：{{ store.rawError[entry.id] }}</span>
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
.inbox-panel {
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
/* Per-row project identity chip (pr-review F4): neutral tone so it reads as metadata,
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
