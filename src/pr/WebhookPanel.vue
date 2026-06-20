<script setup lang="ts">
// Webhook receiver + Cloudflare tunnel control panel (#9). Self-contained: local
// refs over the start_webhook/stop_webhook/webhook_status commands — no Pinia. The
// persisted webhook config (port/secret) is edited+saved by the surrounding
// SettingsView form; this panel only starts/stops the tunnel and surfaces the
// public URL to paste into the GitHub repo webhook settings.
//
// Slice boundary (F5): this panel lives in the `pr` slice but the webhook config is
// `config` slice state. Rather than reach into config's Pinia store (a runtime
// pr→config edge), the composition root (App.vue) passes the SAVED config, the live
// SettingsView draft, and the save-in-flight flag down as props. The only remaining
// cross-slice reference is a TYPE-only `import type { AppConfig }` — erased at
// compile time, so it creates no runtime dependency edge (the slice-boundary test
// in src/slice-boundary.test.ts allows type-only imports for exactly this reason).
import { computed, onMounted, onUnmounted, ref } from "vue";
import {
  startWebhook,
  stopWebhook,
  webhookDeliveries,
  webhookStatus,
} from "./api";
import type { DeliveryStatus, WebhookDelivery, WebhookStatus } from "./types";
import { assertNever } from "../types";
import type { AppConfig } from "../config/types";

// `savedConfig` = the persisted config the backend will actually read on start;
// `draft` = the live SettingsView edit buffer; `saving` = a save in flight.
const props = defineProps<{
  savedConfig: AppConfig | null;
  draft: AppConfig;
  saving: boolean;
}>();

const status = ref<WebhookStatus | null>(null);
// True while a command is in flight, to disable the action buttons.
const busy = ref(false);
const error = ref<string | null>(null);
const copied = ref(false);
// Recent webhook deliveries (#62) — the receiver's diagnostic ring (oldest→newest);
// reversed for most-recent-first display. Self-contained local ref, no Pinia.
const deliveries = ref<WebhookDelivery[]>([]);
// Delivery fetch state, kept SEPARATE from the panel-wide `error`: `run()` resets
// `error` to null on every start/stop/refresh, and these fire concurrently in
// onMounted, so sharing one ref clobbers it. `deliveryLoading` also drives the
// loading-vs-empty distinction and disables the refresh button while a fetch is in
// flight (prevents overlapping refreshes).
const deliveryError = ref<string | null>(null);
const deliveryLoading = ref(false);

// Are there unsaved webhook-field edits? `start_webhook` reads the PERSISTED config,
// so any draft change that hasn't been saved would NOT take effect — gating start on
// this enforces the "save before start" coupling.
const webhookDirty = computed(() => {
  const s = props.savedConfig;
  const d = props.draft;
  if (!s) return false;
  return (
    s.webhookEnabled !== d.webhookEnabled ||
    s.webhookPort !== d.webhookPort ||
    s.webhookSecret !== d.webhookSecret ||
    s.cloudflaredBin !== d.cloudflaredBin ||
    s.webhookTunnelMode !== d.webhookTunnelMode ||
    s.webhookTunnelCommand !== d.webhookTunnelCommand ||
    s.webhookPublicUrl !== d.webhookPublicUrl
  );
});

// Gate the start button on the SAVED `webhookEnabled` (not the draft): the backend
// reads the persisted config, so when disabled it would reject the start, and
// disabling the button surfaces that up-front. Also block while there are unsaved
// webhook edits (`webhookDirty`) or a save is in flight (`saving`) — starting then
// would silently use stale persisted values (finding F6). The hint below tells the
// user to save first.
const enabled = computed(
  () => props.savedConfig?.webhookEnabled === true && !webhookDirty.value && !props.saving,
);

// Tunnel mode (#9) drives mode-aware copy. Read the SAVED config (start_webhook
// honors the persisted mode), defaulting to "quick" (the AppConfig default) when
// config isn't loaded yet. quick = App starts a Cloudflare Quick Tunnel; command =
// App runs the configured tunnel command; listener = App only listens, tunnel is
// managed externally.
const mode = computed(() => props.savedConfig?.webhookTunnelMode ?? "quick");
const port = computed(() => props.savedConfig?.webhookPort ?? null);

// cloudflared is only App's concern in quick mode; command/listener manage tunnels
// externally, so don't nag about a missing cloudflared there.
const showCloudflaredWarn = computed(
  () => mode.value === "quick" && status.value !== null && !status.value.cloudflaredInstalled,
);

// Action label per mode (start), and the running-state header noun.
const startLabel = computed(() => {
  if (status.value?.running) {
    return mode.value === "listener" ? "重启监听" : "重启隧道";
  }
  switch (mode.value) {
    case "quick":
      return "启动 Webhook 隧道";
    case "command":
      return "启动隧道（自定义命令）";
    case "listener":
      return "启动监听";
    default:
      // Exhaustive: a new WebhookTunnelMode arm fails to compile here (#50 G8).
      return assertNever(mode.value);
  }
});

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

// Pull the delivery diagnostics ring (#62). Tolerates a rejected command via the
// dedicated `deliveryError` ref + `toMessage` pattern — a failed fetch must not crash
// the panel nor blank the tunnel controls, and must NOT clobber the panel-wide
// `error` (which `run()` owns). `deliveryLoading` is toggled via try/finally.
async function loadDeliveries() {
  deliveryLoading.value = true;
  deliveryError.value = null;
  try {
    deliveries.value = await webhookDeliveries();
  } catch (e) {
    deliveryError.value = toMessage(e);
  } finally {
    deliveryLoading.value = false;
  }
}

// Most-recent-first view: the backend returns the ring oldest→newest, so reverse a
// shallow copy for display.
const recentDeliveries = computed(() => [...deliveries.value].reverse());

// Short Chinese labels for each DeliveryStatus. Keyed by the full union so adding a
// backend arm without a label fails type-checking here (Record over the union).
const statusLabels: Record<DeliveryStatus, string> = {
  unauthorized: "签名校验失败",
  badPayload: "载荷无效",
  ignored: "已忽略",
  wrongRepo: "仓库不匹配",
  noTriggerLabel: "无触发标签",
  notOpen: "PR 非 open",
  gated: "被拦截",
  dispatched: "已派发",
  listUpdated: "已更新列表",
};

// Tone class per status, so dispatched/listUpdated read as success, the skip/gate
// outcomes as warn, and the hard failures as danger.
function statusTone(s: DeliveryStatus): "ok" | "warn" | "danger" {
  switch (s) {
    case "dispatched":
    case "listUpdated":
      return "ok";
    case "unauthorized":
    case "badPayload":
    case "wrongRepo":
      return "danger";
    case "ignored":
    case "noTriggerLabel":
    case "notOpen":
    case "gated":
      return "warn";
    default:
      // Exhaustive: a new DeliveryStatus arm fails to compile here (#62).
      return assertNever(s);
  }
}

// Include the date: the 50-cap ring can span midnight, and a time-only stamp makes
// cross-day entries ambiguous.
function deliveryTime(epochSecs: number): string {
  return new Date(epochSecs * 1000).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

// Per-delivery explanatory line: prefer the backend message; otherwise, for a
// listUpdated delivery with no message, explain the #61 core scenario (autoReview off
// → enqueued but not dispatched) so the green status isn't reasonless. Mode-agnostic
// wording (#66 F6): a delivery may carry kind "review" OR "check", so the hardcoded
// "未派发 review" was wrong for check-kind PRs — say "自动任务" instead. Empty string =
// nothing to show.
function deliveryDetail(d: WebhookDelivery): string {
  if (d.message) return d.message;
  if (d.status === "listUpdated") return "autoReview 关闭：已入列表，未派发自动任务";
  return "";
}

// Light backstop refresh cadence while the receiver is up: new deliveries arrive
// server-side with no push channel, so poll the ring on this interval.
const DELIVERY_REFRESH_MS = 5_000;

onMounted(() => {
  run(webhookStatus);
  loadDeliveries();
  deliveryTimer = setInterval(() => {
    if (status.value?.running) loadDeliveries();
  }, DELIVERY_REFRESH_MS);
});

let deliveryTimer: ReturnType<typeof setInterval> | null = null;
onUnmounted(() => {
  if (deliveryTimer !== null) clearInterval(deliveryTimer);
});

async function onStart() {
  await run(startWebhook);
  await loadDeliveries();
}

async function onStop() {
  await run(stopWebhook);
  await loadDeliveries();
}

// Re-query status without a full restart — useful when the tunnel is up but the
// public URL hasn't been resolved yet (cloudflared still establishing it).
function onRefresh() {
  return run(webhookStatus);
}

async function copyUrl() {
  // Copy the full Payload URL (incl. `/webhook`) — the tunnel root alone 404s.
  const url = status.value?.payloadUrl;
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
      请先在上方勾选「启用 Webhook」、填好端口/Secret 并<strong>保存</strong>，然后再启动隧道（有未保存的改动时也需先保存）。
    </p>
    <p v-else class="hint">Webhook 配置已保存，可启动隧道。</p>

    <p v-if="showCloudflaredWarn" class="warn">
      未检测到 cloudflared，请先 <code>brew install cloudflared</code>。
    </p>

    <div class="actions">
      <button
        type="button"
        class="primary"
        :disabled="busy || !enabled"
        @click="onStart"
      >
        {{ busy ? "处理中…" : startLabel }}
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

    <div v-if="status?.running && status.payloadUrl" class="url-box">
      <div class="url-row">
        <code class="url">{{ status.payloadUrl }}</code>
        <button type="button" class="copy" @click="copyUrl">
          {{ copied ? "已复制" : "复制" }}
        </button>
      </div>
      <p class="hint">
        <strong>GitHub：</strong>把上面的完整 URL（已含 /webhook 路径）粘贴到仓库 Settings →
        Webhooks → Add webhook：Payload URL 填此 URL，Content type 选 application/json，
        Secret 填与设置里相同的 Webhook Secret，events 选 Pull requests。
      </p>
      <p class="hint">
        <strong>Azure DevOps：</strong>Project Settings → Service Hooks → 新建 Web Hooks
        订阅，事件选 Pull request created / updated，URL 填此地址；在 HTTP 头加
        <code>Authorization: Bearer &lt;Webhook Secret&gt;</code>（与设置里相同的密钥，Azure 无
        HMAC 签名，靠此请求头鉴权）。
      </p>
    </div>

    <div v-else-if="status?.running && !status.payloadUrl" class="url-box">
      <p v-if="mode === 'quick'" class="hint">公网 URL 尚未解析（cloudflared 可能仍在建立隧道）。</p>
      <p v-else class="hint">
        请在设置中填写「公网 URL」(webhookPublicUrl) 并重启，以显示要粘进 GitHub 的 Payload URL。
      </p>
      <button type="button" class="copy" :disabled="busy" @click="onRefresh">
        重新查询状态
      </button>
    </div>

    <p v-if="status?.running && mode === 'listener'" class="hint">
      App 仅监听 127.0.0.1:{{ port ?? "?" }}，请自行将隧道指向该端口。
    </p>

    <!-- Static reminder (#35): the webhook routes by repo, so adding/removing a project
         changes the route table the running tunnel was started with. Don't try to detect
         project changes here — just inform. -->
    <p v-if="status?.running" class="hint">
      提示：增删项目后需重启隧道以更新路由 / Restart the tunnel after adding/removing
      projects to refresh routing.
    </p>

    <p v-if="status?.message" class="msg">{{ status.message }}</p>
    <p v-if="error" class="error">{{ error }}</p>

    <!-- Recent deliveries diagnostics (#62): "did GitHub reach us, and what did we
         do with it". Most-recent-first; never carries secrets/tokens (backend strips). -->
    <div class="deliveries">
      <div class="deliveries-head">
        <h4 class="sub-title">最近 deliveries</h4>
        <button
          type="button"
          class="copy"
          :disabled="deliveryLoading"
          @click="loadDeliveries"
        >
          刷新
        </button>
      </div>
      <p v-if="deliveryError" class="error">{{ deliveryError }}</p>
      <p v-if="deliveryLoading" class="hint">加载中…</p>
      <p v-else-if="recentDeliveries.length === 0" class="hint">暂无 delivery 记录</p>
      <ul v-else class="delivery-list">
        <li
          v-for="(d, i) in recentDeliveries"
          :key="`${d.receivedAtEpoch}-${i}`"
          class="delivery-row"
        >
          <span class="d-time">{{ deliveryTime(d.receivedAtEpoch) }}</span>
          <span class="d-event">
            {{ d.event }}<template v-if="d.action">/{{ d.action }}</template>
          </span>
          <span v-if="d.repo || d.prNumber !== null" class="d-repo">
            <template v-if="d.repo">{{ d.repo }}</template
            ><template v-if="d.prNumber !== null"> #{{ d.prNumber }}</template>
          </span>
          <span class="badge" :class="`tone-${statusTone(d.status)}`">
            {{ statusLabels[d.status] }}
          </span>
          <span v-if="deliveryDetail(d)" class="d-msg">{{ deliveryDetail(d) }}</span>
        </li>
      </ul>
    </div>
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
.deliveries {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}
.deliveries-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.sub-title {
  margin: 0;
  font-size: var(--font-size-sm);
  color: var(--color-text);
}
.delivery-list {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  max-height: 240px;
  overflow-y: auto;
}
.delivery-row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-2) var(--space-3);
  padding: var(--space-2) var(--space-3);
  font-size: var(--font-size-xs);
  background: var(--color-neutral-bg);
  border-radius: var(--radius-sm);
}
.d-time {
  font-family: var(--font-mono);
  color: var(--color-text-muted);
}
.d-event {
  font-family: var(--font-mono);
  color: var(--color-text);
}
.d-repo {
  color: var(--color-text-muted);
}
.d-msg {
  flex-basis: 100%;
  color: var(--color-text-muted);
  word-break: break-word;
}
.badge {
  padding: 0 var(--space-2);
  border-radius: var(--radius-sm);
  font-size: var(--font-size-xs);
  white-space: nowrap;
}
.tone-ok {
  color: var(--color-success);
  background: var(--color-success-bg);
}
.tone-warn {
  color: var(--color-warn);
  background: var(--color-warn-bg);
}
.tone-danger {
  color: var(--color-danger);
  background: var(--color-danger-bg);
}
</style>
