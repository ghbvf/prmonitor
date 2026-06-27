<script setup lang="ts">
import { reactive } from "vue";
import { notificationTestSend } from "./api";
import type { AppConfig, NotificationChannel } from "./types";
import { NOTIFICATION_KINDS, type NotificationKind } from "../types.generated";
import { DEFAULT_NOTIFICATION_CHANNEL } from "./defaults";

const props = defineProps<{ draft: AppConfig }>();
const emit = defineEmits<{ edit: [] }>();

const testState = reactive<Record<string, { loading: boolean; message: string | null; error: string | null }>>({});

type ChannelFormKind = "none" | "webhook" | "telegram" | "email";

const KIND_META: Record<NotificationKind, { label: string; form: ChannelFormKind; signedWebhook: boolean }> = {
  desktop: { label: "Desktop", form: "none", signedWebhook: false },
  email: { label: "Email", form: "email", signedWebhook: false },
  slack: { label: "Slack", form: "webhook", signedWebhook: false },
  telegram: { label: "Telegram", form: "telegram", signedWebhook: false },
  weChatWork: { label: "企业微信", form: "webhook", signedWebhook: false },
  feishu: { label: "飞书", form: "webhook", signedWebhook: true },
  dingTalk: { label: "钉钉", form: "webhook", signedWebhook: true },
};

function nextId(kind: NotificationKind): string {
  return `${kind}-${crypto.randomUUID().slice(0, 8)}`;
}

function newChannel(kind: NotificationKind = "slack"): NotificationChannel {
  return {
    ...DEFAULT_NOTIFICATION_CHANNEL,
    id: nextId(kind),
    name: KIND_META[kind].label,
    kind,
    enabled: false,
  };
}

function addChannel() {
  props.draft.notifications.channels.push(newChannel());
  emit("edit");
}

function removeChannel(id: string) {
  props.draft.notifications.channels = props.draft.notifications.channels.filter((c) => c.id !== id);
  emit("edit");
}

function onKind(channel: NotificationChannel, event: Event) {
  const oldDefaultName = KIND_META[channel.kind].label;
  const nextKind = (event.target as HTMLSelectElement).value as NotificationKind;
  const shouldSyncName = !channel.name.trim() || channel.name.trim() === oldDefaultName;
  channel.kind = nextKind;
  if (shouldSyncName) channel.name = KIND_META[nextKind].label;
  emit("edit");
}

function setText(channel: NotificationChannel, key: keyof NotificationChannel, event: Event) {
  (channel as Record<string, unknown>)[key] = (event.target as HTMLInputElement).value;
  emit("edit");
}

function setNumber(channel: NotificationChannel, key: keyof NotificationChannel, event: Event) {
  const value = (event.target as HTMLInputElement).valueAsNumber;
  (channel as Record<string, unknown>)[key] = Number.isNaN(value) ? 0 : value;
  emit("edit");
}

function setEnabled(channel: NotificationChannel, event: Event) {
  channel.enabled = (event.target as HTMLInputElement).checked;
  emit("edit");
}

async function testSend(channel: NotificationChannel) {
  testState[channel.id] = { loading: true, message: null, error: null };
  try {
    testState[channel.id] = {
      loading: false,
      message: await notificationTestSend({ ...channel }),
      error: null,
    };
  } catch (err) {
    testState[channel.id] = {
      loading: false,
      message: null,
      error: (err as { message?: string })?.message ?? String(err),
    };
  }
}
</script>

<template>
  <div class="notifications">
    <div class="toolbar">
      <button type="button" class="add" @click="addChannel">+ 添加渠道</button>
    </div>

    <div v-if="draft.notifications.channels.length === 0" class="empty">
      暂无通知渠道
    </div>

    <section
      v-for="channel in draft.notifications.channels"
      :key="channel.id"
      class="channel"
    >
      <header class="channel-head">
        <label class="enabled">
          <input type="checkbox" :checked="channel.enabled" @change="setEnabled(channel, $event)" />
          <span>启用</span>
        </label>
        <input
          class="name"
          type="text"
          :value="channel.name"
          placeholder="渠道名称"
          @input="setText(channel, 'name', $event)"
        />
        <select :value="channel.kind" @change="onKind(channel, $event)">
          <option v-for="kind in NOTIFICATION_KINDS" :key="kind" :value="kind">
            {{ KIND_META[kind].label }}
          </option>
        </select>
        <button type="button" class="link" :disabled="testState[channel.id]?.loading" @click="testSend(channel)">
          {{ testState[channel.id]?.loading ? "测试中..." : "测试" }}
        </button>
        <button type="button" class="delete" @click="removeChannel(channel.id)">删除</button>
      </header>

      <div class="grid">
        <label>
          <span>ID</span>
          <input type="text" :value="channel.id" @input="setText(channel, 'id', $event)" />
        </label>
        <label>
          <span>超时（秒）</span>
          <input type="number" min="1" :value="channel.timeoutSecs" @input="setNumber(channel, 'timeoutSecs', $event)" />
        </label>

        <template v-if="KIND_META[channel.kind].form === 'webhook'">
          <label class="wide">
            <span>Webhook URL</span>
            <input type="password" :value="channel.webhookUrl" @input="setText(channel, 'webhookUrl', $event)" />
          </label>
          <label v-if="KIND_META[channel.kind].signedWebhook">
            <span>签名 Secret</span>
            <input type="password" :value="channel.webhookSecret" @input="setText(channel, 'webhookSecret', $event)" />
          </label>
        </template>

        <template v-if="KIND_META[channel.kind].form === 'telegram'">
          <label>
            <span>Bot Token</span>
            <input type="password" :value="channel.telegramBotToken" @input="setText(channel, 'telegramBotToken', $event)" />
          </label>
          <label>
            <span>Chat ID</span>
            <input type="text" :value="channel.telegramChatId" @input="setText(channel, 'telegramChatId', $event)" />
          </label>
        </template>

        <template v-if="KIND_META[channel.kind].form === 'email'">
          <label>
            <span>SMTP Host</span>
            <input type="text" :value="channel.smtpHost" @input="setText(channel, 'smtpHost', $event)" />
          </label>
          <label>
            <span>SMTP Port</span>
            <input type="number" min="1" :value="channel.smtpPort" @input="setNumber(channel, 'smtpPort', $event)" />
          </label>
          <label>
            <span>用户名</span>
            <input type="text" :value="channel.smtpUsername" @input="setText(channel, 'smtpUsername', $event)" />
          </label>
          <label>
            <span>密码</span>
            <input type="password" :value="channel.smtpPassword" @input="setText(channel, 'smtpPassword', $event)" />
          </label>
          <label>
            <span>From</span>
            <input type="text" :value="channel.smtpFrom" @input="setText(channel, 'smtpFrom', $event)" />
          </label>
          <label>
            <span>To</span>
            <input type="text" :value="channel.smtpTo" @input="setText(channel, 'smtpTo', $event)" />
          </label>
        </template>
      </div>

      <p v-if="testState[channel.id]?.error" class="error">{{ testState[channel.id]?.error }}</p>
      <p v-else-if="testState[channel.id]?.message" class="ok">{{ testState[channel.id]?.message }}</p>
    </section>
  </div>
</template>

<style scoped>
.notifications {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.toolbar {
  display: flex;
  justify-content: flex-start;
}
.add,
.link,
.delete {
  padding: var(--space-2) var(--space-3);
  font: inherit;
  font-size: var(--font-size-sm);
  cursor: pointer;
}
.empty {
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
}
.channel {
  padding: var(--space-4);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
  background: var(--color-surface);
}
.channel-head {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.enabled {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  flex: none;
}
.name {
  min-width: 140px;
  flex: 1;
}
.grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: var(--space-4);
  margin-top: var(--space-4);
}
.grid label {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
}
.grid span {
  font-size: var(--font-size-xs);
  color: var(--color-text-muted);
}
.grid input,
.channel-head input,
.channel-head select {
  padding: var(--space-2) var(--space-3);
  font: inherit;
}
.wide {
  grid-column: 1 / -1;
}
.error,
.ok {
  margin: var(--space-3) 0 0;
  font-size: var(--font-size-sm);
}
.error {
  color: var(--color-danger);
}
.ok {
  color: var(--color-success);
}
</style>
