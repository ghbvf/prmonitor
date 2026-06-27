<script setup lang="ts">
import { ref, watch } from "vue";
import { getRemoteAccessRuntimeStatus } from "./api";
import type {
  RemoteAccessRuntimeStatus,
  RemoteEntrypoint,
  RemoteEntrypointState,
  RemoteTunnel,
  RemoteTunnelMode,
  RemoteTunnelState,
} from "./types";

const props = defineProps<{
  refreshKey?: number;
  entrypoints?: RemoteEntrypoint[];
  tunnels?: RemoteTunnel[];
}>();

const status = ref<RemoteAccessRuntimeStatus | null>(null);
const loading = ref(false);
const fetchError = ref<string | null>(null);

async function refresh() {
  loading.value = true;
  fetchError.value = null;
  try {
    status.value = await getRemoteAccessRuntimeStatus();
  } catch (e) {
    fetchError.value = e instanceof Error ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

watch(() => props.refreshKey, refresh, { immediate: true });

function entrypointName(id: string): string {
  const match = props.entrypoints?.find((entrypoint) => entrypoint.id === id);
  return match?.name || id;
}

function tunnelName(id: string): string {
  const match = props.tunnels?.find((tunnel) => tunnel.id === id);
  return match?.name || id;
}

function targetEntrypointName(id: string): string {
  return id ? entrypointName(id) : "未选择";
}

function entrypointStateLabel(state: RemoteEntrypointState): string {
  switch (state) {
    case "bound":
      return "已绑定";
    case "bound-no-auth":
      return "缺少 token";
    case "error":
      return "错误";
  }
}

function tunnelStateLabel(state: RemoteTunnelState): string {
  switch (state) {
    case "running":
      return "运行中";
    case "stopped":
      return "未运行";
    case "error":
      return "错误";
  }
}

function tunnelModeLabel(mode: RemoteTunnelMode): string {
  switch (mode) {
    case "lan":
      return "LAN";
    case "quick":
      return "Quick";
    case "command":
      return "Command";
    case "listener":
      return "Listener";
  }
}
</script>

<template>
  <div class="runtime-status" role="region" aria-label="Remote Access 运行时状态">
    <h3>运行状态</h3>
    <p v-if="loading" class="muted">加载中...</p>
    <p v-else-if="fetchError" class="fetch-error">{{ fetchError }}</p>
    <template v-else-if="status">
      <div v-if="status.entrypoints.length === 0" class="empty">没有启用的 entrypoint。</div>
      <ul v-else class="status-list">
        <li v-for="entrypoint in status.entrypoints" :key="entrypoint.id" class="status-row">
          <strong>{{ entrypointName(entrypoint.id) }}</strong>
          <span class="id">{{ entrypoint.id }}</span>
          <span class="badge" :class="entrypoint.state">{{ entrypointStateLabel(entrypoint.state) }}</span>
          <span v-if="entrypoint.boundPort != null" class="badge">:{{ entrypoint.boundPort }}</span>
          <span class="message">{{ entrypoint.message }}</span>
          <span class="routes">
            {{ entrypoint.routes.map((route) => `${route.path}=${route.capability}`).join(", ") }}
          </span>
        </li>
      </ul>
      <ul v-if="status.tunnels.length > 0" class="status-list tunnels">
        <li v-for="tunnel in status.tunnels" :key="tunnel.id" class="status-row">
          <strong>{{ tunnelName(tunnel.id) }}</strong>
          <span class="id">{{ tunnel.id }}</span>
          <span class="badge">{{ tunnelModeLabel(tunnel.mode) }}</span>
          <span class="badge" :class="tunnel.state">{{ tunnelStateLabel(tunnel.state) }}</span>
          <span class="badge">目标 {{ targetEntrypointName(tunnel.targetEntrypointId) }}</span>
          <span class="message">{{ tunnel.publicUrl || tunnel.message }}</span>
          <details v-if="tunnel.logs.length > 0">
            <summary>日志</summary>
            <pre>{{ tunnel.logs.join("\n") }}</pre>
          </details>
        </li>
      </ul>
    </template>
  </div>
</template>

<style scoped>
.runtime-status {
  width: 100%;
}
h3 {
  margin: 0 0 8px;
  font-size: 14px;
}
.muted,
.empty,
.message,
.routes,
.id {
  color: #64748b;
  font-size: 13px;
}
.fetch-error {
  color: #b91c1c;
  font-size: 13px;
}
.status-list {
  list-style: none;
  padding: 0;
  margin: 0;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.tunnels {
  margin-top: 8px;
}
.status-row {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  align-items: center;
  padding: 8px 10px;
  border: 1px solid #d8dee8;
  border-radius: 8px;
  background: #fff;
}
.badge {
  border: 1px solid #dbe3ef;
  border-radius: 999px;
  padding: 2px 7px;
  font-size: 12px;
  background: #f8fafc;
}
.bound,
.running {
  border-color: #bbf7d0;
  color: #166534;
  background: #f0fdf4;
}
.bound-no-auth,
.stopped {
  border-color: #fde68a;
  color: #92400e;
  background: #fffbeb;
}
.error {
  border-color: #fecaca;
  color: #991b1b;
  background: #fef2f2;
}
details {
  width: 100%;
}
pre {
  max-height: 140px;
  overflow: auto;
  margin: 8px 0 0;
  padding: 8px;
  background: #0f172a;
  color: #e2e8f0;
  border-radius: 6px;
  font-size: 12px;
}
</style>
