<script setup lang="ts">
import { onMounted, ref } from "vue";
import { assertNever } from "../types";
import type { MessagingEventStatus } from "../types.generated";
import { useMessagingStore } from "./useMessagingStore";

const store = useMessagingStore();
const rawOpen = ref<Set<number>>(new Set());

function toggleRaw(id: number) {
  if (rawOpen.value.has(id)) {
    rawOpen.value.delete(id);
  } else {
    rawOpen.value.add(id);
    void store.fetchRaw(id);
  }
  rawOpen.value = new Set(rawOpen.value);
}

function statusLabel(status: MessagingEventStatus): string {
  switch (status) {
    case "received":
      return "已接收 / Received";
    case "processed":
      return "已处理 / Processed";
    case "failed":
      return "失败 / Failed";
    default:
      return assertNever(status);
  }
}

function statusTone(status: MessagingEventStatus): string {
  switch (status) {
    case "received":
      return "pending";
    case "processed":
      return "ok";
    case "failed":
      return "fail";
    default:
      return assertNever(status);
  }
}

function replayDisabled(entry: { id: number; status: MessagingEventStatus }): boolean {
  return entry.status === "processed" || Boolean(store.replayLoading[entry.id]);
}

onMounted(() => {
  void store.refresh();
});
</script>

<template>
  <section class="messaging-panel">
    <header class="head">
      <h2>Messaging</h2>
      <button type="button" class="refresh" :disabled="store.loading" @click="store.refresh()">
        {{ store.loading ? "刷新中…" : "刷新 / refresh" }}
      </button>
    </header>

    <p v-if="store.error" class="error banner">
      <span>{{ store.error }}</span>
      <button type="button" class="dismiss" aria-label="关闭 / dismiss" @click="store.error = null">x</button>
    </p>

    <p v-if="store.loading && store.entries.length === 0" class="muted">加载中… / loading</p>
    <p v-else-if="!store.loading && store.entries.length === 0" class="muted">暂无消息事件 / No messaging events yet.</p>

    <ul class="entries">
      <li v-for="entry in store.entries" :key="entry.id" class="entry">
        <div class="row-head">
          <span class="badge type">{{ entry.event.provider }}</span>
          <span class="badge status" :class="statusTone(entry.status)">
            {{ statusLabel(entry.status) }}
          </span>
          <span class="title">{{ entry.event.text || "(empty)" }}</span>
        </div>

        <div class="meta">
          <span class="badge project">{{ entry.event.integrationId }}</span>
          <span>{{ entry.event.conversationId }}</span>
          <span>{{ entry.event.eventId }}</span>
        </div>

        <p v-if="entry.status === 'failed' && entry.error" class="error fail-reason">
          失败：{{ entry.error }}
        </p>
        <p v-if="entry.reply" class="reply">
          回复：#{{ entry.reply.outboxId }} · {{ entry.reply.kind }} · {{ entry.reply.status ?? "pending" }}
          <span v-if="entry.reply.error">· {{ entry.reply.error }}</span>
        </p>

        <div class="actions">
          <button type="button" class="link" :aria-expanded="rawOpen.has(entry.id)" @click="toggleRaw(entry.id)">
            {{ rawOpen.has(entry.id) ? "隐藏摘要 / hide raw" : "查看摘要 / view raw" }}
          </button>
          <button
            type="button"
            class="link"
            :disabled="replayDisabled(entry)"
            @click="store.replay(entry.id)"
          >
            {{ store.replayLoading[entry.id] ? "重放中… / replaying" : "重放 / replay" }}
          </button>
        </div>

        <template v-if="rawOpen.has(entry.id)">
          <p v-if="store.rawLoading[entry.id]" class="muted raw-loading">加载中… / loading</p>
          <pre v-else-if="store.rawCache[entry.id] !== undefined" class="raw">{{ store.rawCache[entry.id] }}</pre>
          <p v-else-if="store.rawError[entry.id]" class="error raw-error">{{ store.rawError[entry.id] }}</p>
        </template>
      </li>
    </ul>
  </section>
</template>

<style scoped>
.messaging-panel {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
  min-width: 0;
}
.head {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.head h2 {
  margin: 0;
}
.refresh,
.link,
.dismiss {
  padding: var(--space-2) var(--space-3);
  font: inherit;
  font-size: var(--font-size-sm);
  cursor: pointer;
}
.banner {
  display: flex;
  justify-content: space-between;
  gap: var(--space-3);
}
.entries {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  padding: 0;
  margin: 0;
  list-style: none;
}
.entry {
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
  padding: var(--space-4);
  background: var(--color-surface);
}
.row-head,
.meta,
.actions {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-2);
}
.title {
  min-width: 0;
  overflow-wrap: anywhere;
}
.meta {
  margin-top: var(--space-2);
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
}
.reply {
  margin: var(--space-2) 0 0;
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
  overflow-wrap: anywhere;
}
.badge {
  padding: 2px 6px;
  border-radius: var(--radius-sm);
  background: var(--color-neutral-bg);
  font-size: var(--font-size-xs);
}
.status.ok {
  color: var(--color-success);
}
.status.fail,
.error {
  color: var(--color-danger);
}
.status.pending {
  color: var(--color-text-muted);
}
.raw {
  overflow: auto;
  max-height: 260px;
  padding: var(--space-3);
  background: var(--color-neutral-bg);
  border-radius: var(--radius-sm);
}
.muted {
  color: var(--color-text-muted);
}
</style>
