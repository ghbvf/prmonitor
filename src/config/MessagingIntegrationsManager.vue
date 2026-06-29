<script setup lang="ts">
import type { AppConfig, MessagingIntegration } from "./types";
import { MESSAGING_PROVIDER_KINDS, type MessagingProviderKind } from "../types.generated";
import { DEFAULT_MESSAGING_INTEGRATION } from "./defaults";
import { normalizeStringList } from "./remoteAccessOps";

const props = defineProps<{ draft: AppConfig }>();
const emit = defineEmits<{ edit: [] }>();

const KIND_META: Record<MessagingProviderKind, { label: string; form: "feishu" | "weChatWork" | "dingTalk" }> = {
  feishu: { label: "飞书", form: "feishu" },
  weChatWork: { label: "企业微信", form: "weChatWork" },
  dingTalk: { label: "钉钉", form: "dingTalk" },
};

const CONVERSATION_META: Record<MessagingProviderKind, { label: string; placeholder: string }> = {
  feishu: { label: "允许 chat_id", placeholder: "oc_xxx, oc_yyy" },
  weChatWork: { label: "允许 userId/chatId/roomId", placeholder: "zhangsan, chat_xxx, room_xxx" },
  dingTalk: { label: "允许 default/openConversationId", placeholder: "default, cid_xxx" },
};

function nextId(kind: MessagingProviderKind): string {
  return `${kind}-${crypto.randomUUID().slice(0, 8)}`;
}

function newIntegration(kind: MessagingProviderKind = "feishu"): MessagingIntegration {
  return {
    ...DEFAULT_MESSAGING_INTEGRATION,
    id: nextId(kind),
    name: KIND_META[kind].label,
    kind,
    allowedConversationIds: [],
  };
}

function addIntegration() {
  props.draft.messaging.integrations.push(newIntegration());
  emit("edit");
}

function removeIntegration(id: string) {
  props.draft.messaging.integrations = props.draft.messaging.integrations.filter((item) => item.id !== id);
  emit("edit");
}

function onKind(integration: MessagingIntegration, event: Event) {
  const oldDefaultName = KIND_META[integration.kind].label;
  const nextKind = (event.target as HTMLSelectElement).value as MessagingProviderKind;
  const shouldSyncName = !integration.name.trim() || integration.name.trim() === oldDefaultName;
  integration.kind = nextKind;
  if (shouldSyncName) integration.name = KIND_META[nextKind].label;
  emit("edit");
}

function setText(integration: MessagingIntegration, key: keyof MessagingIntegration, event: Event) {
  (integration as Record<string, unknown>)[key] = (event.target as HTMLInputElement).value;
  emit("edit");
}

function setNumber(integration: MessagingIntegration, key: keyof MessagingIntegration, event: Event) {
  const value = (event.target as HTMLInputElement).valueAsNumber;
  (integration as Record<string, unknown>)[key] = Number.isNaN(value) ? 0 : value;
  emit("edit");
}

function setEnabled(integration: MessagingIntegration, event: Event) {
  integration.enabled = (event.target as HTMLInputElement).checked;
  emit("edit");
}

function setRequireMention(integration: MessagingIntegration, event: Event) {
  integration.requireMention = (event.target as HTMLInputElement).checked;
  emit("edit");
}

function setAllowed(integration: MessagingIntegration, event: Event) {
  integration.allowedConversationIds = normalizeStringList((event.target as HTMLInputElement).value);
  emit("edit");
}
</script>

<template>
  <div class="messaging">
    <div class="toolbar">
      <button type="button" class="add" @click="addIntegration">+ 添加集成</button>
    </div>

    <div v-if="draft.messaging.integrations.length === 0" class="empty">
      暂无消息集成
    </div>

    <section
      v-for="integration in draft.messaging.integrations"
      :key="integration.id"
      class="integration"
    >
      <header class="integration-head">
        <label class="enabled">
          <input type="checkbox" :checked="integration.enabled" @change="setEnabled(integration, $event)" />
          <span>启用</span>
        </label>
        <input
          class="name"
          type="text"
          :value="integration.name"
          placeholder="集成名称"
          @input="setText(integration, 'name', $event)"
        />
        <select :value="integration.kind" @change="onKind(integration, $event)">
          <option v-for="kind in MESSAGING_PROVIDER_KINDS" :key="kind" :value="kind">
            {{ KIND_META[kind].label }}
          </option>
        </select>
        <button type="button" class="delete" @click="removeIntegration(integration.id)">删除</button>
      </header>

      <div class="grid">
        <label>
          <span>ID</span>
          <input type="text" :value="integration.id" @input="setText(integration, 'id', $event)" />
        </label>
        <label>
          <span>超时（秒）</span>
          <input type="number" min="1" :value="integration.timeoutSecs" @input="setNumber(integration, 'timeoutSecs', $event)" />
        </label>
        <label class="wide">
          <span>{{ CONVERSATION_META[integration.kind].label }}</span>
          <input
            type="text"
            :value="integration.allowedConversationIds.join(', ')"
            :placeholder="CONVERSATION_META[integration.kind].placeholder"
            @input="setAllowed(integration, $event)"
          />
        </label>
        <label class="check">
          <input type="checkbox" :checked="integration.requireMention" @change="setRequireMention(integration, $event)" />
          <span>群聊必须 @Bot</span>
        </label>

        <template v-if="KIND_META[integration.kind].form === 'feishu'">
          <label>
            <span>Verification Token</span>
            <input type="password" :value="integration.verificationToken" @input="setText(integration, 'verificationToken', $event)" />
          </label>
          <label>
            <span>Encrypt Key</span>
            <input type="password" :value="integration.encryptKey" @input="setText(integration, 'encryptKey', $event)" />
          </label>
          <label>
            <span>App ID</span>
            <input type="password" :value="integration.appId" @input="setText(integration, 'appId', $event)" />
          </label>
          <label>
            <span>App Secret</span>
            <input type="password" :value="integration.appSecret" @input="setText(integration, 'appSecret', $event)" />
          </label>
          <label>
            <span>Bot Open ID</span>
            <input type="text" :value="integration.botOpenId" placeholder="ou_xxx" @input="setText(integration, 'botOpenId', $event)" />
          </label>
        </template>
        <template v-else-if="KIND_META[integration.kind].form === 'weChatWork'">
          <label>
            <span>Token</span>
            <input type="password" :value="integration.verificationToken" @input="setText(integration, 'verificationToken', $event)" />
          </label>
          <label>
            <span>Encoding AES Key</span>
            <input type="password" :value="integration.encryptKey" @input="setText(integration, 'encryptKey', $event)" />
          </label>
          <label>
            <span>Corp ID</span>
            <input type="password" :value="integration.appId" @input="setText(integration, 'appId', $event)" />
          </label>
          <label>
            <span>Corp Secret</span>
            <input type="password" :value="integration.appSecret" @input="setText(integration, 'appSecret', $event)" />
          </label>
          <label>
            <span>Agent ID</span>
            <input type="text" :value="integration.botOpenId" placeholder="1000002" @input="setText(integration, 'botOpenId', $event)" />
          </label>
        </template>
        <template v-else>
          <label>
            <span>Robot Access Token</span>
            <input type="password" :value="integration.verificationToken" @input="setText(integration, 'verificationToken', $event)" />
          </label>
          <label>
            <span>Callback / Robot Secret</span>
            <input type="password" :value="integration.appSecret" @input="setText(integration, 'appSecret', $event)" />
          </label>
          <label>
            <span>Robot Code</span>
            <input type="text" :value="integration.botOpenId" @input="setText(integration, 'botOpenId', $event)" />
          </label>
        </template>
      </div>
    </section>
  </div>
</template>

<style scoped>
.messaging {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.toolbar {
  display: flex;
  justify-content: flex-start;
}
.add,
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
.integration {
  padding: var(--space-4);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
  background: var(--color-surface);
}
.integration-head {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.enabled,
.check {
  display: flex;
  align-items: center;
  gap: var(--space-2);
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
.integration-head input,
.integration-head select {
  padding: var(--space-2) var(--space-3);
  font: inherit;
}
.wide {
  grid-column: 1 / -1;
}
@media (max-width: 760px) {
  .integration-head,
  .grid {
    grid-template-columns: 1fr;
    display: grid;
  }
  .wide {
    grid-column: auto;
  }
}
</style>
