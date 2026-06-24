<script setup lang="ts">
// Remote-access manager (AB#1064): the declarative editor surface inside Settings for the
// `listeners` and `tunnels` config resources. Owns the add/delete operations over the
// SettingsView AppConfig draft's `listeners`/`tunnels` arrays; each item's fields are
// edited by a ListenerCard / TunnelCard. Mirrors ProjectsManager — edits land on the
// draft in place (the parent's reactive AppConfig), committed by SettingsView's existing
// save flow alongside the projects + webhook fields.
//
// Each item renders as a collapsible row: an always-visible header (chevron + name + the
// two-step delete confirm) over the card with the detailed fields (shown only when
// expanded). The add/delete draft mutations live in remoteAccessOps.ts so they're
// unit-testable without a component harness. There is NO active-id selection here (unlike
// projects), so the header carries only name + delete.
import { nextTick, ref, watch } from "vue";
import type { AppConfig } from "./types";
import type { ListenerFieldKey, TunnelFieldKey } from "./fields";
import {
  addListenerToDraft,
  deleteListenerFromDraft,
  addTunnelToDraft,
  deleteTunnelFromDraft,
  updateListenerField,
  updateTunnelField,
} from "./remoteAccessOps";
import ListenerCard from "./ListenerCard.vue";
import TunnelCard from "./TunnelCard.vue";

// The live AppConfig draft (reactive, owned by SettingsView). We mutate `listeners` and
// `tunnels` in place; SettingsView persists the whole draft on save.
const props = defineProps<{ draft: AppConfig }>();
const emit = defineEmits<{ edit: [] }>();

// Which cards are expanded. The two lists share one Set but keys are prefixed
// ("listener:"/"tunnel:") so a listener and a tunnel that happen to share an id never
// collide. Vue tracks Set .add/.delete/.has reactively inside a ref.
const expanded = ref(new Set<string>());
function isExpanded(key: string): boolean {
  return expanded.value.has(key);
}
function toggleExpand(key: string) {
  if (expanded.value.has(key)) expanded.value.delete(key);
  else expanded.value.add(key);
}

// Two-step in-app delete confirmation (window.confirm is unreliable in the Tauri webview —
// see ProjectsManager). At most one row is pending across BOTH lists; the prefixed key
// disambiguates which row armed it.
const pendingDeleteKey = ref<string | null>(null);
function requestDelete(key: string) {
  pendingDeleteKey.value = key;
  // Move focus onto 确认 so keyboard users land on the confirmation (the 删除 button they
  // activated just unmounted). Keyed by the prefixed row key, unique across both lists.
  nextTick(() => document.getElementById(`confirm-yes-${key}`)?.focus());
}
function cancelDelete() {
  pendingDeleteKey.value = null;
}
// Disarm a pending confirm when the draft re-hydrates: SettingsView REPLACES the
// listeners/tunnels array references on save/load, while our own delete splices in place
// (same reference) and won't trip this — so it only clears a strip left armed across a
// save, never our own edit. Mirrors ProjectsManager's pendingDelete reset watch.
watch([() => props.draft.listeners, () => props.draft.tunnels], () => {
  pendingDeleteKey.value = null;
});

// ----- Listeners -----
function addListener() {
  const l = addListenerToDraft(props.draft);
  expanded.value.add(`listener:${l.id}`);
  emit("edit");
}
function confirmDeleteListener(id: string) {
  if (deleteListenerFromDraft(props.draft, id)) {
    expanded.value.delete(`listener:${id}`);
    emit("edit");
  }
  pendingDeleteKey.value = null;
}
// Apply a single field edit onto the matching draft listener, indexed by stable `id` (not
// array position) so a concurrent reorder/delete can't misroute the write. The by-id
// routing lives in updateListenerField (remoteAccessOps.ts) so it's unit-testable.
function onUpdateListener(
  id: string,
  key: ListenerFieldKey,
  value: string | number | boolean | string[],
) {
  if (updateListenerField(props.draft, id, key, value)) emit("edit");
}
function onListenerName(id: string, e: Event) {
  onUpdateListener(id, "name", (e.target as HTMLInputElement).value);
}

// ----- Tunnels -----
function addTunnel() {
  const t = addTunnelToDraft(props.draft);
  expanded.value.add(`tunnel:${t.id}`);
  emit("edit");
}
function confirmDeleteTunnel(id: string) {
  if (deleteTunnelFromDraft(props.draft, id)) {
    expanded.value.delete(`tunnel:${id}`);
    emit("edit");
  }
  pendingDeleteKey.value = null;
}
function onUpdateTunnel(
  id: string,
  key: TunnelFieldKey,
  value: string | number | boolean | string[],
) {
  if (updateTunnelField(props.draft, id, key, value)) emit("edit");
}
function onTunnelName(id: string, e: Event) {
  onUpdateTunnel(id, "name", (e.target as HTMLInputElement).value);
}
</script>

<template>
  <div class="remote-access">
    <!-- Archive-only notice (AB#1064 / AB#1073): the runtime is deferred this round, so the
         config only persists — nothing binds/enforces yet. Shown once above both sections so
         the user isn't misled into thinking an enabled listener actually listens. Styled like
         ProjectCard.vue's `risk-banner` for visual consistency. -->
    <p class="notice-banner" role="alert">
      ⚠️ 远程访问仍在开发中：当前配置仅保存存档、尚未生效——启用的监听器不会绑定端口，鉴权也不会执行。（AB#1064 / AB#1073）
    </p>

    <!-- Listeners -->
    <section class="resource">
      <div class="resource-head">
        <p class="lead">监听器：声明 App 暴露的绑定端点（本地 API / 远程面板 / 事件入站 / 终端）。</p>
        <button type="button" class="add" @click="addListener">+ 添加监听器</button>
      </div>

      <div v-if="draft.listeners.length > 0" class="list">
        <div
          v-for="l in draft.listeners"
          :key="l.id"
          class="list-item"
          :class="{ disabled: !l.enabled }"
        >
          <header class="row-head">
            <button
              type="button"
              class="chevron"
              :aria-expanded="isExpanded(`listener:${l.id}`)"
              :aria-controls="`listener-card-${l.id}`"
              :aria-label="(isExpanded(`listener:${l.id}`) ? '收起' : '展开') + '监听器 ' + (l.name || '新监听器')"
              :title="isExpanded(`listener:${l.id}`) ? '收起' : '展开'"
              @click="toggleExpand(`listener:${l.id}`)"
            >
              {{ isExpanded(`listener:${l.id}`) ? "▾" : "▸" }}
            </button>
            <input
              class="name-input"
              type="text"
              :value="l.name"
              placeholder="新监听器"
              :aria-label="`监听器 ${l.name || '新监听器'} 的名称`"
              @input="onListenerName(l.id, $event)"
            />
            <template v-if="pendingDeleteKey === `listener:${l.id}`">
              <span class="confirm-text">确认删除？</span>
              <button
                :id="`confirm-yes-listener:${l.id}`"
                type="button"
                class="confirm-yes"
                :aria-label="`确认删除监听器 ${l.name || '新监听器'}`"
                @click="confirmDeleteListener(l.id)"
                @keydown.escape="cancelDelete"
              >
                确认
              </button>
              <button
                type="button"
                class="confirm-no"
                :aria-label="`取消删除监听器 ${l.name || '新监听器'}`"
                @click="cancelDelete"
                @keydown.escape="cancelDelete"
              >
                取消
              </button>
            </template>
            <button
              v-else
              type="button"
              class="delete"
              :aria-label="`删除监听器 ${l.name || '新监听器'}`"
              @click="requestDelete(`listener:${l.id}`)"
            >
              删除
            </button>
          </header>
          <ListenerCard
            v-show="isExpanded(`listener:${l.id}`)"
            :id="`listener-card-${l.id}`"
            :listener="l"
            @update="(key, value) => onUpdateListener(l.id, key, value)"
            @edit="emit('edit')"
          />
        </div>
      </div>

      <div v-else class="empty-state">
        <p class="empty-title">还没有监听器</p>
        <p class="empty-hint">点击下方按钮创建第一个监听器。</p>
        <button type="button" class="add" @click="addListener">+ 添加监听器</button>
      </div>
    </section>

    <!-- Tunnels -->
    <section class="resource">
      <div class="resource-head">
        <p class="lead">隧道：把监听器经公网 URL 发布出去（quick / command / listener）。</p>
        <button type="button" class="add" @click="addTunnel">+ 添加隧道</button>
      </div>

      <div v-if="draft.tunnels.length > 0" class="list">
        <div
          v-for="t in draft.tunnels"
          :key="t.id"
          class="list-item"
          :class="{ disabled: !t.enabled }"
        >
          <header class="row-head">
            <button
              type="button"
              class="chevron"
              :aria-expanded="isExpanded(`tunnel:${t.id}`)"
              :aria-controls="`tunnel-card-${t.id}`"
              :aria-label="(isExpanded(`tunnel:${t.id}`) ? '收起' : '展开') + '隧道 ' + (t.name || '新隧道')"
              :title="isExpanded(`tunnel:${t.id}`) ? '收起' : '展开'"
              @click="toggleExpand(`tunnel:${t.id}`)"
            >
              {{ isExpanded(`tunnel:${t.id}`) ? "▾" : "▸" }}
            </button>
            <input
              class="name-input"
              type="text"
              :value="t.name"
              placeholder="新隧道"
              :aria-label="`隧道 ${t.name || '新隧道'} 的名称`"
              @input="onTunnelName(t.id, $event)"
            />
            <template v-if="pendingDeleteKey === `tunnel:${t.id}`">
              <span class="confirm-text">确认删除？</span>
              <button
                :id="`confirm-yes-tunnel:${t.id}`"
                type="button"
                class="confirm-yes"
                :aria-label="`确认删除隧道 ${t.name || '新隧道'}`"
                @click="confirmDeleteTunnel(t.id)"
                @keydown.escape="cancelDelete"
              >
                确认
              </button>
              <button
                type="button"
                class="confirm-no"
                :aria-label="`取消删除隧道 ${t.name || '新隧道'}`"
                @click="cancelDelete"
                @keydown.escape="cancelDelete"
              >
                取消
              </button>
            </template>
            <button
              v-else
              type="button"
              class="delete"
              :aria-label="`删除隧道 ${t.name || '新隧道'}`"
              @click="requestDelete(`tunnel:${t.id}`)"
            >
              删除
            </button>
          </header>
          <TunnelCard
            v-show="isExpanded(`tunnel:${t.id}`)"
            :id="`tunnel-card-${t.id}`"
            :tunnel="t"
            :listeners="draft.listeners"
            @update="(key, value) => onUpdateTunnel(t.id, key, value)"
            @edit="emit('edit')"
          />
        </div>
      </div>

      <div v-else class="empty-state">
        <p class="empty-title">还没有隧道</p>
        <p class="empty-hint">点击下方按钮创建第一个隧道。</p>
        <button type="button" class="add" @click="addTunnel">+ 添加隧道</button>
      </div>
    </section>
  </div>
</template>

<style scoped>
.remote-access {
  display: flex;
  flex-direction: column;
  gap: var(--space-6);
  width: 100%;
}
/* Archive-only notice (AB#1064): copied from ProjectCard.vue's `.risk-banner` so the
   "config-only, not yet live" warning reads consistently with the CLI-polling warning. */
.notice-banner {
  margin: 0;
  padding: var(--space-3) var(--space-4);
  font-size: var(--font-size-sm);
  color: var(--color-warn);
  background: var(--color-warn-bg);
  border: 1px solid var(--color-warn-border);
  border-radius: var(--radius-sm);
}
.resource {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.resource-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
}
.lead {
  margin: 0;
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
}
.add {
  flex-shrink: 0;
  padding: var(--space-3) var(--space-5);
  font: inherit;
  font-size: var(--font-size-sm);
  color: var(--color-surface);
  background: var(--color-accent);
  border: none;
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.list {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.list-item {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
/* Disabled (未启用) items dim their header so "won't run" reads at a glance. */
.list-item.disabled .row-head {
  opacity: 0.55;
}
.row-head {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-2) var(--space-3);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
}
.chevron {
  flex-shrink: 0;
  width: 1.6rem;
  padding: var(--space-1) 0;
  font: inherit;
  color: var(--color-text-muted);
  background: none;
  border: none;
  cursor: pointer;
}
.name-input {
  flex: 1;
  min-width: 0;
  padding: var(--space-2) var(--space-3);
  font: inherit;
  font-size: var(--font-size-md);
  color: var(--color-text);
  background: var(--color-surface);
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-sm);
}
.name-input:focus {
  outline: none;
  border-color: var(--color-accent);
}
.delete {
  flex-shrink: 0;
  padding: var(--space-2) var(--space-4);
  font: inherit;
  font-size: var(--font-size-sm);
  color: var(--color-danger);
  background: none;
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.delete:hover {
  background: var(--color-surface-hover);
}
.confirm-text {
  flex-shrink: 0;
  font-size: var(--font-size-sm);
  color: var(--color-danger);
}
.confirm-yes,
.confirm-no {
  flex-shrink: 0;
  padding: var(--space-2) var(--space-4);
  font: inherit;
  font-size: var(--font-size-sm);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.confirm-yes {
  color: var(--color-surface);
  background: var(--color-danger);
  border: 1px solid var(--color-danger);
}
.confirm-yes:hover {
  opacity: 0.85;
}
.confirm-no {
  color: var(--color-text);
  background: none;
  border: 1px solid var(--color-border-strong);
}
.confirm-no:hover {
  background: var(--color-surface-hover);
}
.confirm-yes:focus-visible,
.confirm-no:focus-visible {
  outline: 2px solid var(--color-accent);
  outline-offset: 2px;
}
.empty-state {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-6);
  text-align: center;
  color: var(--color-text-muted);
  background: var(--color-surface);
  border: 1px dashed var(--color-border-strong);
  border-radius: var(--radius-md);
}
.empty-title {
  margin: 0;
  font-size: var(--font-size-md);
  color: var(--color-text);
}
.empty-hint {
  margin: 0;
  font-size: var(--font-size-sm);
}
</style>
