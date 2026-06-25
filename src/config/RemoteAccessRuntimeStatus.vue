<script setup lang="ts">
// Runtime status panel for the configured listeners (AB#1225 PR1). Fetches
// `get_listener_runtime_status` on mount and whenever `refreshKey` increments (the
// parent drives re-fetch after a config save). One row per status: name + kind badge +
// state badge + secondary message text.
//
// The `assertNever`穷尽 carrier for ListenerState lives in listenerStateLabel.ts
// (imported below) so the vitest contract test can drive it without mounting the
// component — mirrors the eventTypeLabel / inboxStatusLabel pattern (src/types.ts).
import { computed, watch } from "vue";
import type { Listener, ListenerRuntimeStatus } from "./types";
import { getListenerRuntimeStatus } from "./api";
// listenerStateLabel / listenerKindLabel are extracted to their own module so the vitest
// contract test can import them directly without mounting this component (same pattern as
// eventTypeLabel in types.ts).
import { listenerKindLabel, listenerStateLabel } from "./listenerStateLabel";
import { ref } from "vue";

const props = defineProps<{
  // Increment this to trigger a re-fetch. The parent should bump it after a
  // successful config save so the status reflects any newly-added listeners.
  refreshKey?: number;
  // The current draft listeners from RemoteAccessManager (F14): used to display the
  // listener's name alongside its runtime id, and to distinguish empty-state cases (F13).
  listeners?: Listener[];
}>();

const statuses = ref<ListenerRuntimeStatus[]>([]);
const loading = ref(false);
const fetchError = ref<string | null>(null);

async function refresh() {
  loading.value = true;
  fetchError.value = null;
  try {
    statuses.value = await getListenerRuntimeStatus();
  } catch (e) {
    fetchError.value = e instanceof Error ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

// Single refresh trigger (F17): `immediate: true` fires on mount AND on every
// subsequent refreshKey change, replacing the previous dual onMounted+watch pattern.
// One code path → no risk of double-fetch or a missed initial load.
watch(() => props.refreshKey, refresh, { immediate: true });

// Resolve a status row's display name: prefer the matching draft listener's name,
// fall back to the raw id when not found or name is empty (F14).
function displayName(id: string): string {
  const match = props.listeners?.find((l) => l.id === id);
  return match?.name || id;
}

// Empty-state variant (F13): distinguish between no listeners configured at all vs.
// listeners present but all disabled (so the status list is empty because the backend
// only returns ENABLED listeners).
const emptyStateKind = computed<"none" | "all-disabled">(() => {
  const ls = props.listeners ?? [];
  if (ls.length === 0) return "none";
  if (ls.every((l) => !l.enabled)) return "all-disabled";
  // Backend returned nothing even though some listeners are enabled — treat as "none"
  // for the most conservative empty state message.
  return "none";
});
</script>

<template>
  <div class="runtime-status" role="region" aria-label="监听器运行时状态">
    <!-- Visible section heading (F16) -->
    <h3 class="section-heading">当前运行状态</h3>

    <p v-if="loading" class="muted">加载中…</p>
    <p v-else-if="fetchError" class="error">{{ fetchError }}</p>
    <div v-else-if="statuses.length === 0" class="empty">
      <template v-if="emptyStateKind === 'all-disabled'">
        所有监听器已停用，启用后在此显示运行状态
      </template>
      <template v-else>
        还没有监听器，请在下方「监听器」添加
      </template>
    </div>
    <ul v-else class="status-list">
      <li
        v-for="s in statuses"
        :key="s.id"
        class="status-row"
        :class="`state-${s.state}`"
        :data-state="s.state"
      >
        <!-- F14: display name from draft.listeners, fallback to id -->
        <span class="row-name">{{ displayName(s.id) }}</span>
        <!-- F15: localized kind label instead of raw wire value -->
        <span class="kind-badge">{{ listenerKindLabel(s.kind) }}</span>
        <span class="state-badge">
          {{ listenerStateLabel(s.state) }}
          <template v-if="s.state === 'bound' && s.boundPort != null">
            :{{ s.boundPort }}
          </template>
        </span>
        <span v-if="s.message" class="message">{{ s.message }}</span>
      </li>
    </ul>
  </div>
</template>

<style scoped>
.section-heading {
  margin: 0 0 var(--space-3) 0;
  font-size: var(--font-size-sm);
  font-weight: 600;
  color: var(--color-text);
}
.runtime-status {
  width: 100%;
}
.muted {
  margin: 0;
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
}
.error {
  margin: 0;
  font-size: var(--font-size-sm);
  color: var(--color-danger);
}
.empty {
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
}
.status-list {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.status-row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-2) var(--space-3);
  font-size: var(--font-size-sm);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
}
.row-name {
  font-weight: 500;
  color: var(--color-text);
  min-width: 6rem;
}
.kind-badge {
  padding: 0 var(--space-2);
  font-size: var(--font-size-xs, 0.75rem);
  color: var(--color-text-muted);
  background: var(--color-surface-hover);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
}
.state-badge {
  font-weight: 500;
}
/* State-specific badge colouring. */
.state-bound .state-badge {
  color: var(--color-success);
}
.state-error .state-badge {
  color: var(--color-danger);
}
.state-blocked-needs-1073 .state-badge,
.state-unsupported .state-badge {
  color: var(--color-warn);
}
.message {
  margin-left: auto;
  color: var(--color-text-muted);
  font-size: var(--font-size-xs, 0.75rem);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 18rem;
}
</style>
