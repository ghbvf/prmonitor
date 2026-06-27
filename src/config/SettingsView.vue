<script setup lang="ts">
// Settings page (#34, multi-project #35): a grouped editor over the persisted
// AppConfig. Left nav lists a "项目" (projects) section plus the GLOBAL_GROUPS
// (webhook) groups; the right pane renders the active section. The projects section
// hosts ProjectsManager (the per-project list/editor); webhook fields render via
// ConfigField. Reuses the useConfigStore draft/hydrate/save flow — edits land on a
// local reactive `draft` (the full AppConfig), committed via store.save(); the draft
// re-hydrates whenever the store's config arrives/changes.
import { onMounted, reactive, ref, watch } from "vue";
import { useConfigStore } from "./useConfigStore";
import type { AppConfig } from "./types";
import { DEFAULT_NOTIFICATION_SETTINGS, DEFAULT_OUTBOX_CONFIG } from "./defaults";
import { GLOBAL_GROUPS, type FieldDef, type GlobalFieldKey } from "./fields";
import ConfigField from "./ConfigField.vue";
import NotificationChannelsManager from "./NotificationChannelsManager.vue";
import ProjectsManager from "./ProjectsManager.vue";
import RemoteAccessManager from "./RemoteAccessManager.vue";
// The webhook control panel lives in the `pr` slice; mounting it here would be a
// config→pr edge. Instead we expose a `webhook` scoped slot (saved config + live
// draft + saving flag) and let the composition root (App.vue) fill it — keeping
// this slice free of any pr import (F5).

const store = useConfigStore();

// `saved` lets the composition root reschedule (poll interval may have changed).
// Returning to the monitor view is owned by the App.vue app-bar toggle, so this
// view no longer renders its own back control (avoids a duplicate "返回监控", #68).
const emit = defineEmits<{ saved: [] }>();

// The synthetic nav id for the projects section (not a GLOBAL_GROUPS id). The
// per-project authors csv state now lives inside each ProjectCard, so SettingsView
// no longer carries a top-level authorsInput.
const PROJECTS_NAV_ID = "projects";

// The synthetic nav id for the Remote Access section (AB#1064; not a GLOBAL_GROUPS id).
// Like PROJECTS_NAV_ID it routes to a dedicated array-of-objects editor
// (RemoteAccessManager) instead of the scalar GLOBAL_GROUPS ConfigField path.
const REMOTE_ACCESS_NAV_ID = "remoteAccess";

const NOTIFICATIONS_NAV_ID = "notifications";

// Bumped on each successful save so RemoteAccessRuntimeStatus re-fetches live state
// (AB#1225 PR1). Forwarded through RemoteAccessManager → RemoteAccessRuntimeStatus via
// the `refreshKey` prop chain.
const remoteAccessRefreshKey = ref(0);

// Editable draft (decoupled from the store). The full multi-project AppConfig:
// `projects`/`activeProjectId` are edited by ProjectsManager; the webhook fields by
// the GLOBAL_GROUPS form below.
const draft = reactive<AppConfig>({
  projects: [],
  activeProjectId: "",
  webhookEnabled: false,
  webhookPort: 8787,
  webhookSecret: "",
  cloudflaredBin: "cloudflared",
  webhookTunnelMode: "quick",
  webhookTunnelCommand: "",
  webhookPublicUrl: "",
  localApiToken: "",
  // No settings-panel control yet (AB#1182): the draft carries the loaded value through a save
  // round-trip (hydrate overwrites it) so saving settings never wipes a configured TTL.
  outbox: { ...DEFAULT_OUTBOX_CONFIG },
  notifications: { channels: DEFAULT_NOTIFICATION_SETTINGS.channels.map((c) => ({ ...c })) },
  listeners: [],
  tunnels: [],
});

function hydrate(cfg: AppConfig) {
  // Deep-copy projects so card edits never mutate the store's config object before a
  // save (the store replaces `config` only on a successful setConfig).
  draft.projects = cfg.projects.map((p) => ({ ...p, authors: [...p.authors] }));
  draft.activeProjectId = cfg.activeProjectId;
  draft.webhookEnabled = cfg.webhookEnabled;
  draft.webhookPort = cfg.webhookPort;
  draft.webhookSecret = cfg.webhookSecret;
  draft.cloudflaredBin = cfg.cloudflaredBin;
  draft.webhookTunnelMode = cfg.webhookTunnelMode;
  draft.webhookTunnelCommand = cfg.webhookTunnelCommand;
  draft.webhookPublicUrl = cfg.webhookPublicUrl;
  draft.localApiToken = cfg.localApiToken;
  // Preserve the loaded outbox policy verbatim (AB#1182): no UI edits it, but `save()` persists the
  // whole draft, so a missed copy here would silently reset the TTL on every settings save. Fall
  // back to the default if an older cached config lacks the field.
  draft.outbox = { ...(cfg.outbox ?? DEFAULT_OUTBOX_CONFIG) };
  draft.notifications = {
    ...(cfg.notifications ?? DEFAULT_NOTIFICATION_SETTINGS),
    channels: (cfg.notifications?.channels ?? DEFAULT_NOTIFICATION_SETTINGS.channels).map((c) => ({
      ...c,
    })),
  };
  // Deep-copy the remote-access resources (AB#1064) so card edits never mutate the store's
  // config object before a save — same reason as the projects deep-copy above. A listener's
  // allowedOrigins is a string[], so clone it too (mirrors the authors clone).
  draft.listeners = cfg.listeners.map((l) => ({
    ...l,
    allowedOrigins: [...l.allowedOrigins],
  }));
  draft.tunnels = cfg.tunnels.map((t) => ({ ...t }));
}

// Populate the draft once the async config lands (and on any later replacement).
watch(
  () => store.config,
  (cfg) => {
    if (cfg) hydrate(cfg);
  },
  { immediate: true },
);

onMounted(() => store.load());

// Default to the projects section.
const activeGroupId = ref(PROJECTS_NAV_ID);

// Read/write a GLOBAL (webhook) field's draft value by key. Only webhook FieldDefs
// reach here (GLOBAL_GROUPS holds the webhook group; `projects`/`activeProjectId` are
// structural, edited by ProjectsManager — not a field group), so every value is a
// string/number/boolean. The cast narrows the over-wide `AppConfig[GlobalFieldKey]`
// (which structurally includes `Project[]`) to the heterogeneous field-value union.
// Webhook fields carry no csv kind, so no authors-style special-casing is needed.
function fieldValue(def: FieldDef): string | number | boolean | string[] {
  return draft[def.key as GlobalFieldKey] as string | number | boolean | string[];
}

function setField(def: FieldDef, value: string | number | boolean | string[]) {
  // Each global FieldDef.kind matches its AppConfig field's value type (number kinds
  // map to numeric keys, select/text to string keys), so the per-key assignment is
  // type-correct at runtime; the cast bridges the heterogeneous emit signature.
  (draft as Record<GlobalFieldKey, unknown>)[def.key as GlobalFieldKey] = value;
}

// Clear the "已保存。" banner / stale error the moment the user resumes editing.
function onEdit() {
  store.savedOk = false;
  store.error = null;
}

async function onSave() {
  // Projects already hold normalized authors arrays (each ProjectCard emits string[]),
  // so the whole draft persists as-is. Spread to a plain object so the store keeps a
  // detached snapshot (not the live reactive draft).
  await store.save({
    ...draft,
    projects: draft.projects.map((p) => ({ ...p, authors: [...p.authors] })),
    // Detach the remote-access resources too (AB#1064): clone each object and the
    // listener's allowedOrigins array so the store snapshot isn't the live reactive draft.
    notifications: {
      ...draft.notifications,
      channels: draft.notifications.channels.map((c) => ({ ...c })),
    },
    listeners: draft.listeners.map((l) => ({
      ...l,
      allowedOrigins: [...l.allowedOrigins],
    })),
    tunnels: draft.tunnels.map((t) => ({ ...t })),
  });
  if (store.savedOk) {
    emit("saved");
    remoteAccessRefreshKey.value += 1;
  }
  // On a failed save, route to the page that owns the offending field.
  else if (store.error) {
    if (
      store.error.startsWith("notificationChannelId") ||
      store.error.startsWith("notificationTimeoutSecs") ||
      store.error.startsWith("notificationWebhookUrl") ||
      store.error.startsWith("telegramBotToken") ||
      store.error.startsWith("telegramChatId") ||
      store.error.startsWith("smtpHost") ||
      store.error.startsWith("smtpPort") ||
      store.error.startsWith("smtpFrom") ||
      store.error.startsWith("smtpTo")
    ) {
      activeGroupId.value = NOTIFICATIONS_NAV_ID;
    } else if (
      store.error.startsWith("publicUrl") ||
      store.error.startsWith("port") ||
      store.error.startsWith("bindHost") ||
      store.error.startsWith("targetListenerId") ||
      store.error.startsWith("auth ") ||
      store.error.startsWith("authToken") ||
      store.error.startsWith("terminalRead") ||
      store.error.startsWith("command")
    ) {
      activeGroupId.value = REMOTE_ACCESS_NAV_ID;
    }
  }
}
</script>

<template>
  <section class="settings">
    <header class="settings-header">
      <h2>设置</h2>
    </header>

    <p v-if="store.loading && !store.config" class="muted">加载中…</p>
    <p v-else-if="store.error && !store.config" class="error">{{ store.error }}</p>

    <div v-else class="body">
      <nav class="nav">
        <button
          type="button"
          class="nav-item"
          :class="{ active: activeGroupId === PROJECTS_NAV_ID }"
          @click="activeGroupId = PROJECTS_NAV_ID"
        >
          项目
        </button>
        <button
          type="button"
          class="nav-item"
          :class="{ active: activeGroupId === REMOTE_ACCESS_NAV_ID }"
          @click="activeGroupId = REMOTE_ACCESS_NAV_ID"
        >
          远程访问
        </button>
        <button
          type="button"
          class="nav-item"
          :class="{ active: activeGroupId === NOTIFICATIONS_NAV_ID }"
          @click="activeGroupId = NOTIFICATIONS_NAV_ID"
        >
          通知
        </button>
        <button
          v-for="g in GLOBAL_GROUPS"
          :key="g.id"
          type="button"
          class="nav-item"
          :class="{ active: g.id === activeGroupId }"
          @click="activeGroupId = g.id"
        >
          {{ g.title }}
        </button>
      </nav>

      <form class="pane" @submit.prevent="onSave">
        <div v-if="activeGroupId === PROJECTS_NAV_ID" class="group projects-group">
          <ProjectsManager :draft="draft" @edit="onEdit" />
        </div>

        <div
          v-if="activeGroupId === REMOTE_ACCESS_NAV_ID"
          class="group projects-group"
        >
          <RemoteAccessManager :draft="draft" :refresh-key="remoteAccessRefreshKey" @edit="onEdit" />
        </div>

        <div
          v-if="activeGroupId === NOTIFICATIONS_NAV_ID"
          class="group projects-group"
        >
          <NotificationChannelsManager :draft="draft" @edit="onEdit" />
        </div>

        <template v-for="g in GLOBAL_GROUPS" :key="g.id">
          <div v-if="g.id === activeGroupId" class="group">
            <ConfigField
              v-for="def in g.fields"
              :key="def.key"
              :def="def"
              :model-value="fieldValue(def)"
              @update:model-value="setField(def, $event)"
              @edit="onEdit"
            />
          </div>
        </template>

        <slot
          v-if="activeGroupId === 'webhook'"
          name="webhook"
          :saved-config="store.config"
          :draft="draft"
          :saving="store.saving"
        />

        <div class="actions">
          <button type="submit" class="primary" :disabled="store.saving">
            {{ store.saving ? "保存中…" : "保存" }}
          </button>
          <p v-if="store.error" class="error">{{ store.error }}</p>
          <p v-else-if="store.savedOk" class="ok">已保存。</p>
        </div>
      </form>
    </div>
  </section>
</template>

<style scoped>
.settings {
  display: flex;
  flex-direction: column;
  height: 100%;
  background: var(--color-bg);
  color: var(--color-text);
}
.settings-header {
  display: flex;
  align-items: center;
  padding: var(--space-4) var(--space-6);
  border-bottom: 1px solid var(--color-border);
}
.settings-header h2 {
  margin: 0;
  font-size: var(--font-size-lg);
}
.muted {
  padding: var(--space-6);
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
}
.body {
  display: flex;
  flex: 1;
  min-height: 0;
}
.nav {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  width: 160px;
  flex-shrink: 0;
  padding: var(--space-4);
  border-right: 1px solid var(--color-border);
}
.nav-item {
  text-align: left;
  background: none;
  border: none;
  padding: var(--space-3) var(--space-4);
  font: inherit;
  font-size: var(--font-size-md);
  color: var(--color-text);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.nav-item:hover {
  background: var(--color-surface-hover);
}
.nav-item.active {
  background: var(--color-accent-bg);
  color: var(--color-accent);
}
.pane {
  display: flex;
  flex-direction: column;
  flex: 1;
  gap: var(--space-6);
  padding: var(--space-6);
  overflow-y: auto;
}
.group {
  display: flex;
  flex-direction: column;
  gap: var(--space-5);
  max-width: 480px;
}
/* The projects section hosts a full-width list of cards — let it use the pane width. */
.projects-group {
  max-width: 720px;
}
.actions {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  margin-top: auto;
}
.primary {
  align-self: flex-start;
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
.error {
  margin: 0;
  font-size: var(--font-size-sm);
  color: var(--color-danger);
}
.ok {
  margin: 0;
  font-size: var(--font-size-sm);
  color: var(--color-success);
}
</style>
