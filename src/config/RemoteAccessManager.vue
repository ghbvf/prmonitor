<script setup lang="ts">
import { ref } from "vue";
import type { AppConfig, RemoteCapability, RemoteEntrypoint } from "./types";
import RemoteAccessRuntimeStatus from "./RemoteAccessRuntimeStatus.vue";
import {
  addEntrypointToDraft,
  addRouteToEntrypoint,
  addTunnelToDraft,
  deleteEntrypointFromDraft,
  deleteRouteFromEntrypoint,
  deleteTunnelFromDraft,
  normalizeStringList,
} from "./remoteAccessOps";

const props = defineProps<{ draft: AppConfig; refreshKey?: number }>();
const emit = defineEmits<{ edit: [] }>();
type PendingDelete =
  | { kind: "entrypoint"; id: string }
  | { kind: "route"; entrypointId: string; id: string }
  | { kind: "tunnel"; id: string };

const pendingDelete = ref<PendingDelete | null>(null);

function csv(values: string[]): string {
  return values.join(", ");
}

function addEntrypoint() {
  pendingDelete.value = null;
  addEntrypointToDraft(props.draft);
  emit("edit");
}

function deleteEntrypoint(id: string) {
  pendingDelete.value = { kind: "entrypoint", id };
}

function confirmDeleteEntrypoint(id: string) {
  if (deleteEntrypointFromDraft(props.draft, id)) emit("edit");
  pendingDelete.value = null;
}

function addRoute(entrypoint: RemoteEntrypoint, capability: RemoteCapability) {
  pendingDelete.value = null;
  addRouteToEntrypoint(entrypoint, capability);
  emit("edit");
}

function deleteRoute(entrypoint: RemoteEntrypoint, id: string) {
  pendingDelete.value = { kind: "route", entrypointId: entrypoint.id, id };
}

function confirmDeleteRoute(entrypoint: RemoteEntrypoint, id: string) {
  if (deleteRouteFromEntrypoint(entrypoint, id)) emit("edit");
  pendingDelete.value = null;
}

function addTunnel() {
  pendingDelete.value = null;
  addTunnelToDraft(props.draft);
  emit("edit");
}

function deleteTunnel(id: string) {
  pendingDelete.value = { kind: "tunnel", id };
}

function confirmDeleteTunnel(id: string) {
  if (deleteTunnelFromDraft(props.draft, id)) emit("edit");
  pendingDelete.value = null;
}

function cancelDelete() {
  pendingDelete.value = null;
}

function pendingEntrypoint(id: string): boolean {
  return pendingDelete.value?.kind === "entrypoint" && pendingDelete.value.id === id;
}

function pendingRoute(entrypointId: string, id: string): boolean {
  return (
    pendingDelete.value?.kind === "route" &&
    pendingDelete.value.entrypointId === entrypointId &&
    pendingDelete.value.id === id
  );
}

function pendingTunnel(id: string): boolean {
  return pendingDelete.value?.kind === "tunnel" && pendingDelete.value.id === id;
}

function setSourcePolicyMode(entrypoint: RemoteEntrypoint, mode: RemoteEntrypoint["sourcePolicy"]["mode"]) {
  entrypoint.sourcePolicy.mode = mode;
  if (mode !== "custom") entrypoint.sourcePolicy.allow = [];
  touch();
}

function touch() {
  emit("edit");
}
</script>

<template>
  <div class="remote-access">
    <RemoteAccessRuntimeStatus
      :refresh-key="props.refreshKey"
      :entrypoints="draft.remoteAccess.entrypoints"
      :tunnels="draft.remoteAccess.tunnels"
      :messaging-integrations="draft.messaging.integrations"
    />

    <section class="resource">
      <div class="resource-head">
        <h3>Entry Points</h3>
        <button type="button" class="add" @click="addEntrypoint">+ 添加入口</button>
      </div>

      <div v-if="draft.remoteAccess.entrypoints.length > 0" class="list">
        <article
          v-for="entrypoint in draft.remoteAccess.entrypoints"
          :key="entrypoint.id"
          class="item"
          :class="{ disabled: !entrypoint.enabled }"
        >
          <div class="grid">
            <label>
              <span>名称</span>
              <input v-model="entrypoint.name" type="text" @input="touch" />
            </label>
            <label>
              <span>绑定地址</span>
              <input v-model="entrypoint.bindHost" type="text" @input="touch" />
            </label>
            <label>
              <span>端口</span>
              <input v-model.number="entrypoint.port" type="number" min="0" @input="touch" />
            </label>
            <label class="check">
              <input v-model="entrypoint.enabled" type="checkbox" @change="touch" />
              <span>启用</span>
            </label>
            <label>
              <span>来源策略</span>
              <select
                :value="entrypoint.sourcePolicy.mode"
                @change="setSourcePolicyMode(entrypoint, ($event.target as HTMLSelectElement).value as RemoteEntrypoint['sourcePolicy']['mode'])"
              >
                <option value="loopback">loopback</option>
                <option value="lan">lan</option>
                <option value="custom">custom</option>
              </select>
            </label>
            <label v-if="entrypoint.sourcePolicy.mode === 'custom'">
              <span>允许 IP/CIDR</span>
              <input
                :value="csv(entrypoint.sourcePolicy.allow)"
                type="text"
                placeholder="192.168.1.10, 10.0.0.0/8"
                @input="entrypoint.sourcePolicy.allow = normalizeStringList(($event.target as HTMLInputElement).value); touch()"
              />
            </label>
            <label>
              <span>Allowed Origins</span>
              <input
                :value="csv(entrypoint.allowedOrigins)"
                type="text"
                @input="entrypoint.allowedOrigins = normalizeStringList(($event.target as HTMLInputElement).value); touch()"
              />
            </label>
            <label>
              <span>Trusted Proxies</span>
              <input
                :value="csv(entrypoint.trustedProxies)"
                type="text"
                @input="entrypoint.trustedProxies = normalizeStringList(($event.target as HTMLInputElement).value); touch()"
              />
            </label>
          </div>

          <div class="routes-head">
            <strong>Routes</strong>
            <div class="actions">
              <button type="button" @click="addRoute(entrypoint, 'terminal')">+ terminal</button>
              <button type="button" @click="addRoute(entrypoint, 'local-api')">+ local-api</button>
              <button type="button" @click="addRoute(entrypoint, 'messaging')">+ messaging</button>
            </div>
          </div>

          <div class="routes">
            <div v-for="route in entrypoint.routes" :key="route.id" class="route">
              <label>
                <span>名称</span>
                <input v-model="route.name" type="text" @input="touch" />
              </label>
              <label>
                <span>Path</span>
                <input v-model="route.path" type="text" @input="touch" />
              </label>
              <label>
                <span>能力</span>
                <select v-model="route.capability" @change="touch">
                  <option value="terminal">terminal</option>
                  <option value="local-api">local-api</option>
                  <option value="messaging">messaging</option>
                </select>
              </label>
              <label class="check">
                <input v-model="route.enabled" type="checkbox" @change="touch" />
                <span>启用</span>
              </label>
              <label v-if="route.capability === 'terminal'" class="wide">
                <span>Bearer Token</span>
                <input v-model="route.authToken" type="password" @input="touch" />
              </label>
              <div v-if="route.capability === 'terminal'" class="permissions">
                <label class="check"><input v-model="route.terminalRead" type="checkbox" @change="touch" />读</label>
                <label class="check"><input v-model="route.terminalWrite" type="checkbox" @change="touch" />写</label>
                <label class="check"><input v-model="route.terminalCreate" type="checkbox" @change="touch" />创建</label>
                <label class="check"><input v-model="route.terminalAdmin" type="checkbox" @change="touch" />管理</label>
              </div>
              <div v-if="pendingRoute(entrypoint.id, route.id)" class="delete-confirm">
                <button type="button" class="delete danger" autofocus @click="confirmDeleteRoute(entrypoint, route.id)">
                  确认删除
                </button>
                <button type="button" @click="cancelDelete">取消</button>
              </div>
              <button v-else type="button" class="delete" @click="deleteRoute(entrypoint, route.id)">删除</button>
            </div>
          </div>

          <div v-if="pendingEntrypoint(entrypoint.id)" class="delete-confirm entry-delete">
            <button type="button" class="delete danger" autofocus @click="confirmDeleteEntrypoint(entrypoint.id)">
              确认删除入口
            </button>
            <button type="button" @click="cancelDelete">取消</button>
          </div>
          <button v-else type="button" class="delete entry-delete" @click="deleteEntrypoint(entrypoint.id)">
            删除入口
          </button>
        </article>
      </div>
      <p v-else class="empty">还没有 entrypoint。</p>
    </section>

    <section class="resource">
      <div class="resource-head">
        <h3>Tunnels</h3>
        <button type="button" class="add" @click="addTunnel">+ 添加隧道</button>
      </div>
      <div v-if="draft.remoteAccess.tunnels.length > 0" class="list">
        <article v-for="tunnel in draft.remoteAccess.tunnels" :key="tunnel.id" class="item">
          <div class="grid">
            <label>
              <span>名称</span>
              <input v-model="tunnel.name" type="text" @input="touch" />
            </label>
            <label>
              <span>Mode</span>
              <select v-model="tunnel.mode" @change="touch">
                <option value="lan">lan</option>
                <option value="quick">quick</option>
                <option value="command">command</option>
                <option value="listener">listener</option>
              </select>
            </label>
            <label>
              <span>目标入口</span>
              <select v-model="tunnel.targetEntrypointId" @change="touch">
                <option value="">未选择</option>
                <option
                  v-for="entrypoint in draft.remoteAccess.entrypoints"
                  :key="entrypoint.id"
                  :value="entrypoint.id"
                >
                  {{ entrypoint.name || entrypoint.id }}
                </option>
              </select>
            </label>
            <label class="check">
              <input v-model="tunnel.enabled" type="checkbox" @change="touch" />
              <span>启用</span>
            </label>
            <label>
              <span>LAN Bind Host</span>
              <input v-model="tunnel.bindHost" type="text" @input="touch" />
            </label>
            <label>
              <span>LAN Port</span>
              <input v-model.number="tunnel.port" type="number" min="0" @input="touch" />
            </label>
            <label class="wide">
              <span>Command</span>
              <input v-model="tunnel.command" type="text" @input="touch" />
            </label>
            <label class="wide">
              <span>Public URL</span>
              <input v-model="tunnel.publicUrl" type="text" @input="touch" />
            </label>
          </div>
          <div v-if="pendingTunnel(tunnel.id)" class="delete-confirm entry-delete">
            <button type="button" class="delete danger" autofocus @click="confirmDeleteTunnel(tunnel.id)">
              确认删除隧道
            </button>
            <button type="button" @click="cancelDelete">取消</button>
          </div>
          <button v-else type="button" class="delete entry-delete" @click="deleteTunnel(tunnel.id)">
            删除隧道
          </button>
        </article>
      </div>
      <p v-else class="empty">还没有 tunnel。</p>
    </section>
  </div>
</template>

<style scoped>
.remote-access {
  display: flex;
  flex-direction: column;
  gap: 18px;
}
.resource {
  border-top: 1px solid #e5e7eb;
  padding-top: 16px;
}
.resource-head,
.routes-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  margin-bottom: 12px;
}
h3 {
  margin: 0;
  font-size: 15px;
}
.list {
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.item,
.route {
  border: 1px solid #d8dee8;
  border-radius: 8px;
  padding: 12px;
  background: #fff;
}
.disabled {
  opacity: 0.72;
}
.grid,
.route {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
  gap: 10px;
}
label {
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 12px;
  color: #4b5563;
}
input,
select {
  min-height: 32px;
  border: 1px solid #cbd5e1;
  border-radius: 6px;
  padding: 5px 8px;
  font: inherit;
}
.check {
  flex-direction: row;
  align-items: center;
  min-height: 32px;
}
.check input {
  min-height: 0;
}
.wide {
  grid-column: 1 / -1;
}
.routes {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.permissions,
.actions {
  display: flex;
  flex-wrap: wrap;
  gap: 10px;
  align-items: center;
}
button {
  border: 1px solid #cbd5e1;
  border-radius: 6px;
  background: #f8fafc;
  padding: 6px 10px;
  cursor: pointer;
}
.add {
  background: #eef6ff;
  border-color: #bfdbfe;
}
.delete {
  background: #fff7f7;
  border-color: #fecaca;
}
.danger {
  background: #fee2e2;
  border-color: #fca5a5;
}
.delete-confirm {
  display: flex;
  gap: 8px;
  align-items: center;
}
.entry-delete {
  margin-top: 10px;
}
.empty {
  color: #64748b;
  font-size: 13px;
}
</style>
