<script setup lang="ts">
// Webhook receiver + Cloudflare tunnel control panel (#9). Self-contained: local
// refs over the start_webhook/stop_webhook/webhook_status commands — no Pinia. The
// persisted webhook config (port/secret) is edited+saved by the surrounding
// SettingsView form; this panel only starts/stops the tunnel and surfaces the
// public URL to paste into the GitHub repo webhook settings.
import { computed, onMounted, ref } from "vue";
import { startWebhook, stopWebhook, webhookStatus } from "./api";
import type { WebhookStatus } from "./types";
import { useConfigStore } from "../config/useConfigStore";

const status = ref<WebhookStatus | null>(null);
// True while a command is in flight, to disable the action buttons.
const busy = ref(false);
const error = ref<string | null>(null);
const copied = ref(false);

// `start_webhook` reads the PERSISTED config, so gate the button on the SAVED
// `webhookEnabled` (store.config), not the unsaved SettingsView draft — this also
// enforces the "save before start" coupling. When false, the backend would reject
// the start; disabling the button surfaces that up-front instead of as an error.
const store = useConfigStore();
const enabled = computed(() => store.config?.webhookEnabled === true);

// Normalize a rejected invoke into a user-facing string (Tauri rejects with an
// object carrying `message`; fall back to String() for anything else).
function toMessage(e: unknown): string {
  if (e && typeof e === "object" && "message" in e) {
    return String((e as { message: unknown }).message);
  }
  return String(e);
}

async function run(fn: () => Promise<WebhookStatus>) {
  busy.value = true;
  error.value = null;
  try {
    status.value = await fn();
  } catch (e) {
    error.value = toMessage(e);
  } finally {
    busy.value = false;
  }
}

onMounted(() => run(webhookStatus));

function onStart() {
  return run(startWebhook);
}

function onStop() {
  return run(stopWebhook);
}

// Re-query status without a full restart — useful when the tunnel is up but the
// public URL hasn't been resolved yet (cloudflared still establishing it).
function onRefresh() {
  return run(webhookStatus);
}

async function copyUrl() {
  const url = status.value?.publicUrl;
  if (!url) return;
  try {
    await navigator.clipboard.writeText(url);
    copied.value = true;
    setTimeout(() => (copied.value = false), 1500);
  } catch (e) {
    error.value = toMessage(e);
  }
}
</script>

<template>
  <section class="webhook">
    <h3 class="title">Webhook 隧道</h3>

    <p v-if="!enabled" class="warn">
      请先在上方勾选「启用 Webhook」、填好端口/Secret 并<strong>保存</strong>，然后再启动隧道。
    </p>
    <p v-else class="hint">启动前请确认上方 Webhook 配置（端口/Secret）已保存。</p>

    <p v-if="status && !status.cloudflaredInstalled" class="warn">
      未检测到 cloudflared，请先 <code>brew install cloudflared</code>。
    </p>

    <div class="actions">
      <button
        type="button"
        class="primary"
        :disabled="busy || !enabled"
        @click="onStart"
      >
        {{ busy ? "处理中…" : status?.running ? "重启 Webhook 隧道" : "启动 Webhook 隧道" }}
      </button>
      <button
        v-if="status?.running"
        type="button"
        class="ghost"
        :disabled="busy"
        @click="onStop"
      >
        停止
      </button>
    </div>

    <div v-if="status?.running && status.publicUrl" class="url-box">
      <div class="url-row">
        <code class="url">{{ status.publicUrl }}</code>
        <button type="button" class="copy" @click="copyUrl">
          {{ copied ? "已复制" : "复制" }}
        </button>
      </div>
      <p class="hint">
        把上面的 URL 粘贴到 GitHub 仓库 Settings → Webhooks → Add webhook：Payload URL
        填此 URL，Content type 选 application/json，Secret 填与设置里相同的 Webhook
        Secret，events 选 Pull requests。
      </p>
    </div>

    <div v-else-if="status?.running && !status.publicUrl" class="url-box">
      <p class="hint">公网 URL 尚未解析（cloudflared 可能仍在建立隧道）。</p>
      <button type="button" class="copy" :disabled="busy" @click="onRefresh">
        重新查询状态
      </button>
    </div>

    <p v-if="status?.message" class="msg">{{ status.message }}</p>
    <p v-if="error" class="error">{{ error }}</p>
  </section>
</template>

<style scoped>
.webhook {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
  max-width: 480px;
  padding: var(--space-5);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-md);
}
.title {
  margin: 0;
  font-size: var(--font-size-md);
}
.hint {
  margin: 0;
  font-size: var(--font-size-xs);
  color: var(--color-text-muted);
}
.warn {
  margin: 0;
  font-size: var(--font-size-sm);
  color: var(--color-warn);
}
.actions {
  display: flex;
  gap: var(--space-3);
}
.primary {
  padding: var(--space-3) var(--space-6);
  font: inherit;
  font-size: var(--font-size-md);
  color: var(--color-surface);
  background: var(--color-accent);
  border: none;
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.primary:disabled {
  opacity: 0.6;
  cursor: not-allowed;
}
.ghost {
  padding: var(--space-3) var(--space-6);
  font: inherit;
  font-size: var(--font-size-md);
  color: var(--color-text);
  background: none;
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.ghost:disabled {
  opacity: 0.6;
  cursor: not-allowed;
}
.url-box {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  padding: var(--space-4);
  background: var(--color-neutral-bg);
  border-radius: var(--radius-sm);
}
.url-row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.url {
  flex: 1;
  font-family: var(--font-mono);
  font-size: var(--font-size-sm);
  color: var(--color-accent);
  word-break: break-all;
}
.copy {
  flex-shrink: 0;
  padding: var(--space-2) var(--space-3);
  font: inherit;
  font-size: var(--font-size-sm);
  color: var(--color-accent);
  background: none;
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.msg {
  margin: 0;
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
}
.error {
  margin: 0;
  font-size: var(--font-size-sm);
  color: var(--color-danger);
}
</style>
