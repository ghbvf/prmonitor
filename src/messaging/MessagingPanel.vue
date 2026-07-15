<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { assertNever, outboxKindLabel, outboxStatusLabel } from "../types";
import type { MessagingEventStatus } from "../types.generated";
import { useMessagingStore } from "./useMessagingStore";
import FeishuConnectionStatusList from "../FeishuConnectionStatusList.vue";

const store = useMessagingStore();
const rawOpen = ref<Set<number>>(new Set());
const sendDraft = ref({
  integrationId: "",
  conversationId: "",
  text: "",
});
const enabledIntegrations = computed(() => store.integrations);
const selectedIntegration = computed(() => enabledIntegrations.value.find((item) => item.id === sendDraft.value.integrationId) ?? null);
const allowedConversationIds = computed(() => selectedIntegration.value?.allowedConversationIds ?? []);
const normalizedConversationId = computed(() => sendDraft.value.conversationId.trim());
const conversationAllowed = computed(() => allowedConversationIds.value.includes(normalizedConversationId.value));
const sendDisabled = computed(() =>
  store.sendLoading ||
  !sendDraft.value.integrationId.trim() ||
  !normalizedConversationId.value ||
  !sendDraft.value.text.trim() ||
  !conversationAllowed.value,
);

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
  void store.refreshIntegrations();
  void store.refreshConnectionStatuses();
  void store.refresh();
  void store.refreshSends();
});

watch(enabledIntegrations, (items) => {
  if (!sendDraft.value.integrationId && items[0]) {
    sendDraft.value.integrationId = items[0].id;
  }
}, { immediate: true });

watch(selectedIntegration, (integration) => {
  if (!integration) {
    sendDraft.value.conversationId = "";
    return;
  }
  if (!integration.allowedConversationIds.includes(normalizedConversationId.value)) {
    sendDraft.value.conversationId = integration.allowedConversationIds[0] ?? "";
  }
});

function sendTest() {
  void store.send({
    integrationId: sendDraft.value.integrationId.trim(),
    conversationId: normalizedConversationId.value,
    text: sendDraft.value.text.trim(),
    requestId: crypto.randomUUID(),
  });
}
</script>

<template>
  <section class="messaging-panel">
    <header class="head">
      <h2>Messaging</h2>
      <button type="button" class="refresh" :disabled="store.loading" @click="store.refresh()">
        {{ store.loading ? "刷新中…" : "刷新 / refresh" }}
      </button>
      <button type="button" class="refresh" :disabled="store.sendsLoading" @click="store.refreshSends()">
        {{ store.sendsLoading ? "发送日志刷新中…" : "发送日志 / sends" }}
      </button>
    </header>

    <p v-if="store.error" class="error banner">
      <span>{{ store.error }}</span>
      <button type="button" class="dismiss" aria-label="关闭 / dismiss" @click="store.error = null">x</button>
    </p>

    <FeishuConnectionStatusList
      :statuses="store.connectionStatuses"
      :loading="store.connectionStatusesLoading"
      :error="store.connectionStatusesError"
      :stale="store.connectionStatusesStale"
      :last-updated-at="store.connectionStatusesLastUpdatedAt"
      @refresh="store.refreshConnectionStatuses()"
    />

    <form class="send-form" @submit.prevent="sendTest">
      <select v-model="sendDraft.integrationId">
        <option value="" disabled>integration</option>
        <option v-for="integration in enabledIntegrations" :key="integration.id" :value="integration.id">
          {{ integration.name || integration.id }} · {{ integration.kind }}
        </option>
      </select>
      <input v-model="sendDraft.conversationId" type="text" list="messaging-conversation-options" placeholder="conversation id" />
      <datalist id="messaging-conversation-options">
        <option v-for="id in allowedConversationIds" :key="id" :value="id" />
      </datalist>
      <input v-model="sendDraft.text" type="text" placeholder="message text" />
      <button
        type="submit"
        class="refresh"
        :disabled="sendDisabled"
      >
        {{ store.sendLoading ? "发送中…" : "发送消息 / send" }}
      </button>
    </form>
    <p v-if="store.sendError" class="error banner">
      <span>{{ store.sendError }}</span>
      <button type="button" class="dismiss" aria-label="关闭 / dismiss" @click="store.sendError = null">x</button>
    </p>

    <section class="send-log">
      <h3>发送日志 / Send log</h3>
      <p v-if="store.sendsLoading && store.sends.length === 0" class="muted">加载中… / loading</p>
      <p v-else-if="!store.sendsLoading && store.sends.length === 0" class="muted">暂无发送记录 / No sends yet.</p>
      <ul class="entries compact">
        <li v-for="entry in store.sends" :key="entry.id" class="entry send-entry">
          <div class="row-head">
            <span class="badge type">{{ outboxKindLabel(entry.kind) }}</span>
            <span class="badge status" :class="entry.status === 'done' ? 'ok' : entry.status === 'dead' ? 'fail' : 'pending'">
              {{ outboxStatusLabel(entry.status) }}
            </span>
            <span class="title">#{{ entry.id }} · {{ entry.summary }}</span>
          </div>
          <p v-if="entry.lastError" class="error fail-reason">{{ entry.lastError }}</p>
        </li>
      </ul>
    </section>

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
.send-form {
  display: grid;
  grid-template-columns: minmax(140px, 1fr) minmax(140px, 1fr) minmax(180px, 2fr) auto;
  gap: var(--space-2);
  align-items: center;
}
.send-form input,
.send-form select {
  min-width: 0;
  padding: var(--space-2) var(--space-3);
  font: inherit;
}
.send-log h3 {
  margin: 0 0 var(--space-2);
  font-size: var(--font-size-md);
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
.entries.compact .entry {
  padding: var(--space-3);
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
@media (max-width: 760px) {
  .send-form {
    grid-template-columns: 1fr;
  }
}
</style>
