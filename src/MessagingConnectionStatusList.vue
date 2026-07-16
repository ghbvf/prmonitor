<script setup lang="ts">
import type { MessagingConnectionState, MessagingConnectionStatus, MessagingProviderKind } from "./types.generated";

defineProps<{
  statuses: MessagingConnectionStatus[];
  loading?: boolean;
  error?: string | null;
  stale?: boolean;
  lastUpdatedAt?: number | null;
}>();

const emit = defineEmits<{ refresh: [] }>();

const STATE_LABELS: Record<MessagingConnectionState, string> = {
  disabled: "已禁用 / Disabled",
  connecting: "连接中 / Connecting",
  connected: "已连接 / Connected",
  reconnecting: "重连中 / Reconnecting",
  error: "错误 / Error",
  stopped: "已停止 / Stopped",
};

const PROVIDER_LABELS: Record<MessagingProviderKind, string> = {
  feishu: "飞书",
  weChatWork: "企业微信",
  dingTalk: "钉钉",
};

function stateLabel(state: MessagingConnectionState): string {
  return STATE_LABELS[state];
}

function providerLabel(provider: MessagingProviderKind): string {
  return PROVIDER_LABELS[provider] ?? provider;
}

function timestamp(epoch: number | null): string {
  if (epoch == null) return "—";
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "short",
    timeStyle: "medium",
  }).format(new Date(epoch * 1000));
}

function snapshotTimestamp(milliseconds: number | null | undefined): string {
  if (milliseconds == null) return "未知 / Unknown";
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "short",
    timeStyle: "medium",
  }).format(new Date(milliseconds));
}
</script>

<template>
  <section class="connection-health" aria-label="消息长连接状态">
    <header>
      <h3>消息长连接 / Messaging long connection</h3>
      <button type="button" :disabled="loading" @click="emit('refresh')">
        {{ loading ? "刷新中…" : "刷新" }}
      </button>
    </header>
    <p v-if="error" class="status-error">{{ error }}</p>
    <p v-if="stale && statuses.length > 0" class="stale-notice">
      已过期 / Stale · 快照更新于 {{ snapshotTimestamp(lastUpdatedAt) }}
    </p>
    <p v-if="loading && statuses.length === 0" class="empty">加载消息长连接状态…</p>
    <p v-else-if="!loading && statuses.length === 0" class="empty">暂无消息长连接。</p>
    <ul v-else class="status-list">
      <li v-for="item in statuses" :key="item.integrationId" class="status-row">
        <div class="status-heading">
          <strong>{{ item.integrationId }}</strong>
          <span class="provider">{{ providerLabel(item.provider) }}</span>
          <span class="state" :class="stale ? 'stale' : item.status">{{ stateLabel(item.status) }}</span>
          <span v-if="item.reconnectCount > 0" class="reconnects">重连 {{ item.reconnectCount }} 次</span>
        </div>
        <dl>
          <div>
            <dt>最后连接</dt>
            <dd>{{ timestamp(item.lastConnectedAtEpoch) }}</dd>
          </div>
          <div>
            <dt>最后事件</dt>
            <dd>{{ timestamp(item.lastEventAtEpoch) }}</dd>
          </div>
        </dl>
        <p v-if="item.lastError" class="status-error">{{ item.lastError }}</p>
      </li>
    </ul>
  </section>
</template>

<style scoped>
.connection-health {
  padding: var(--space-4);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
  background: var(--color-surface);
}
header,
.status-heading,
dl,
dl div {
  display: flex;
  align-items: center;
}
header {
  justify-content: space-between;
  gap: var(--space-3);
}
h3,
p,
dl {
  margin: 0;
}
h3 {
  font-size: var(--font-size-md);
}
button {
  padding: var(--space-1) var(--space-3);
  font: inherit;
  cursor: pointer;
}
.status-list {
  display: grid;
  gap: var(--space-3);
  padding: 0;
  margin: var(--space-3) 0 0;
  list-style: none;
}
.status-row {
  padding-top: var(--space-3);
  border-top: 1px solid var(--color-border);
}
.status-heading {
  flex-wrap: wrap;
  gap: var(--space-2);
}
.state,
.provider,
.reconnects {
  padding: 2px 6px;
  border-radius: var(--radius-sm);
  color: var(--color-text-muted);
  background: var(--color-neutral-bg);
  font-size: var(--font-size-xs);
}
.state.connected {
  color: var(--color-success);
}
.state.error,
.status-error {
  color: var(--color-danger);
}
.state.stale,
.stale-notice {
  color: var(--color-warning, var(--color-text-muted));
}
dl {
  flex-wrap: wrap;
  gap: var(--space-4);
  margin-top: var(--space-2);
  color: var(--color-text-muted);
  font-size: var(--font-size-xs);
}
dl div {
  gap: var(--space-1);
}
dd {
  margin: 0;
}
.status-error,
.stale-notice,
.empty {
  margin-top: var(--space-2);
  overflow-wrap: anywhere;
  font-size: var(--font-size-sm);
}
.empty {
  color: var(--color-text-muted);
}
</style>
