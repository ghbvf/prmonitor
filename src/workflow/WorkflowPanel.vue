<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import {
  assertNever,
  skillKeyLabel,
  workflowStatusLabel,
  workflowStepLabel,
  type WorkflowInstance,
  type WorkflowStatus,
} from "../types";
import { useWorkflowStore } from "./useWorkflowStore";

const props = defineProps<{ activeProjectId?: string }>();
const store = useWorkflowStore();
const scope = ref<"all" | "current">("all");
const rawOpen = ref<Set<number>>(new Set());

function onScopeChange(event: Event) {
  scope.value =
    (event.target as HTMLSelectElement).value === "current" ? "current" : "all";
  syncScope();
  void store.refresh();
}

function syncScope() {
  store.projectId =
    scope.value === "current" && props.activeProjectId ? props.activeProjectId : null;
}

function toggleRaw(id: number) {
  if (rawOpen.value.has(id)) {
    rawOpen.value.delete(id);
  } else {
    rawOpen.value.add(id);
    void store.fetchRaw(id);
  }
  rawOpen.value = new Set(rawOpen.value);
}

function fmtTime(epoch: number): string {
  if (!epoch) return "—";
  return new Date(epoch * 1000).toLocaleString();
}

function statusTone(status: WorkflowStatus): string {
  switch (status) {
    case "done":
      return "ok";
    case "failed":
      return "fail";
    case "waiting":
      return "waiting";
    case "pending":
    case "running":
      return "active";
    default:
      return assertNever(status);
  }
}

function trace(instance: WorkflowInstance): string {
  const parts = [];
  const thread = instance.state.reviewThreadId;
  const outbox = instance.state.notificationOutboxIds;
  if (thread) parts.push(`thread ${thread}`);
  if (outbox?.length) parts.push(`outbox ${outbox.join(",")}`);
  return parts.join(" · ");
}

function workflowIdentity(entry: WorkflowInstance): string {
  const input = entry.input;
  const parts = [];
  parts.push(`PR #${input.prNumber}`);
  if (input.reference) parts.push(input.reference);
  if (input.skillKey) parts.push(skillKeyLabel(input.skillKey));
  return parts.length ? parts.join(" · ") : `#${entry.id}`;
}

function showsNextWake(status: WorkflowStatus): boolean {
  return status === "pending" || status === "running" || status === "waiting";
}

let unlisten: Awaited<ReturnType<typeof store.init>> | null = null;
onMounted(async () => {
  syncScope();
  unlisten = await store.init();
});
onUnmounted(() => unlisten?.());
</script>

<template>
  <section class="workflow-panel">
    <header class="head">
      <h2>Workflow</h2>
      <select
        class="scope"
        :value="scope"
        aria-label="项目范围 / project scope"
        @change="onScopeChange"
      >
        <option value="all">全部项目 / All</option>
        <option value="current" :disabled="!props.activeProjectId">
          当前项目 / Current
        </option>
      </select>
      <button type="button" class="refresh" :disabled="store.loading" @click="store.refresh()">
        {{ store.loading ? "刷新中…" : "刷新 / refresh" }}
      </button>
    </header>

    <p v-if="store.cycleError" class="error banner">
      <span>后台编排异常 / workflow error：{{ store.cycleError }}</span>
      <button type="button" class="dismiss" @click="store.cycleError = null">✕</button>
    </p>
    <p v-if="store.error" class="error banner">
      <span>{{ store.error }}</span>
      <button type="button" class="dismiss" @click="store.error = null">✕</button>
    </p>
    <p v-if="store.loading && store.entries.length === 0" class="muted">加载中… / loading</p>
    <p v-else-if="!store.loading && store.entries.length === 0" class="muted">
      暂无 workflow / No workflows yet.
    </p>

    <ul class="entries">
      <li v-for="entry in store.entries" :key="entry.id" class="entry">
        <div class="row-head">
          <span class="badge type">{{ entry.type }}</span>
          <span class="badge status" :class="statusTone(entry.status)">
            {{ workflowStatusLabel(entry.status) }}
          </span>
          <span class="title">{{ workflowIdentity(entry) }} · {{ workflowStepLabel(entry.currentStep) }}</span>
        </div>
        <div class="meta">
          <span class="badge project">{{ entry.projectId || "通用 / Global" }}</span>
          <span>尝试 {{ entry.attemptCount }} 次</span>
          <span>更新 {{ fmtTime(entry.updatedAt) }}</span>
          <span v-if="showsNextWake(entry.status)">下次 {{ fmtTime(entry.nextWakeAt) }}</span>
          <span v-else-if="entry.status === 'failed'">需手动重试 / manual retry</span>
        </div>
        <p v-if="trace(entry)" class="trace">{{ trace(entry) }}</p>
        <p v-if="entry.lastError" class="error fail-reason">失败：{{ entry.lastError }}</p>
        <div class="actions">
          <button type="button" class="link" @click="toggleRaw(entry.id)">
            {{ rawOpen.has(entry.id) ? "隐藏原始 / hide raw" : "查看原始 / view raw" }}
          </button>
          <button
            v-if="entry.status === 'failed'"
            type="button"
            class="link"
            :disabled="store.retryLoading[entry.id]"
            @click="store.retry(entry.id)"
          >
            {{ store.retryLoading[entry.id] ? "重试中… / retrying" : "重试 / retry" }}
          </button>
        </div>
        <template v-if="rawOpen.has(entry.id)">
          <p v-if="store.rawLoading[entry.id]" class="muted">加载中… / loading</p>
          <pre v-else-if="store.rawCache[entry.id] !== undefined" class="raw">{{ store.rawCache[entry.id] }}</pre>
          <p v-else-if="store.rawError[entry.id]" class="error raw-error">
            原始 JSON 加载失败：{{ store.rawError[entry.id] }}
            <button type="button" class="link" @click="store.fetchRaw(entry.id)">
              重试 / retry
            </button>
          </p>
        </template>
      </li>
    </ul>
  </section>
</template>

<style scoped>
.workflow-panel {
  padding: var(--space-2);
}
.head,
.row-head,
.meta,
.actions,
.banner {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.head {
  justify-content: space-between;
}
.head h2 {
  margin: 0;
}
.refresh {
  padding: var(--space-2) var(--space-5);
  font: inherit;
  font-size: var(--font-size-sm);
  cursor: pointer;
}
.muted,
.meta,
.trace {
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
}
.error {
  color: var(--color-danger);
  font-size: var(--font-size-sm);
}
.banner {
  justify-content: space-between;
  margin: var(--space-4) 0 0;
}
.dismiss {
  border: none;
  background: transparent;
  color: inherit;
  cursor: pointer;
}
.entries {
  list-style: none;
  margin: var(--space-5) 0 0;
  padding: 0;
}
.entry {
  padding: var(--space-4);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-md);
  background: var(--color-surface);
}
.entry + .entry {
  margin-top: var(--space-3);
}
.title {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.badge {
  display: inline-block;
  padding: 1px var(--space-3);
  border-radius: var(--radius-md);
  font-size: var(--font-size-xs);
  line-height: 1.5;
}
.badge.type,
.badge.project {
  background: var(--color-accent-badge-bg);
  color: var(--color-accent);
}
.status.ok {
  background: var(--color-neutral-bg);
  color: var(--color-text-muted);
}
.status.fail {
  background: var(--color-danger-bg);
  color: var(--color-danger);
}
.status.waiting {
  background: var(--color-warn-bg-strong);
  color: var(--color-warn);
}
.status.active {
  background: var(--color-success-bg);
  color: var(--color-success);
}
.link {
  border: none;
  background: transparent;
  color: var(--color-accent);
  cursor: pointer;
  padding: 0;
  font: inherit;
  font-size: var(--font-size-sm);
}
.raw {
  overflow: auto;
  max-height: 220px;
  padding: var(--space-3);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
  background: var(--color-surface-muted);
  font-size: var(--font-size-xs);
}
</style>
