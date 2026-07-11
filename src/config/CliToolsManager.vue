<script setup lang="ts">
import { computed, onMounted, ref, watch } from "vue";
import { CLI_TOOLS } from "../types";
import type { CliResolutionSource, CliTool } from "../types";
import type { AppConfig, CliToolProbeStatus, CliToolsConfig } from "./types";
import { probeCliTools } from "./api";
import {
  CLI_RESOLUTION_SOURCE_LABELS,
  CLI_TOOL_META,
  cliProbeSnapshotMatches,
  cliToolsPathErrors,
  cloneCliTools,
  probeStatusesByTool,
} from "./cliTools";

const props = withDefaults(
  defineProps<{ draft: AppConfig; refreshKey?: number }>(),
  { refreshKey: 0 },
);
const emit = defineEmits<{ edit: [] }>();

const statuses = ref<Partial<Record<CliTool, CliToolProbeStatus>>>({});
const probedSnapshot = ref<CliToolsConfig | null>(null);
const probing = ref(false);
const probeError = ref<string | null>(null);
let latestProbe = 0;
const pathErrors = computed(() => cliToolsPathErrors(props.draft.cliTools));
const stale = computed(
  () =>
    probedSnapshot.value !== null &&
    !cliProbeSnapshotMatches(probedSnapshot.value, props.draft.cliTools),
);

function pathFor(tool: CliTool): string {
  return props.draft.cliTools[CLI_TOOL_META[tool].pathKey];
}

function sourceLabel(source: CliResolutionSource | null): string {
  return source === null ? "" : CLI_RESOLUTION_SOURCE_LABELS[source];
}

function onPathInput(tool: CliTool, event: Event) {
  props.draft.cliTools[CLI_TOOL_META[tool].pathKey] = (event.target as HTMLInputElement).value;
  probeError.value = null;
  emit("edit");
}

function toMessage(error: unknown): string {
  return (error as { message?: string })?.message ?? String(error);
}

async function probe(refreshPath: boolean) {
  const request = ++latestProbe;
  probing.value = true;
  probeError.value = null;
  const snapshot = cloneCliTools(props.draft.cliTools);
  const probeDraft = cloneCliTools(snapshot);
  for (const tool of CLI_TOOLS) {
    if (pathErrors.value[tool]) probeDraft[CLI_TOOL_META[tool].pathKey] = "";
  }
  try {
    const result = await probeCliTools(probeDraft, refreshPath);
    if (request !== latestProbe) return;
    statuses.value = probeStatusesByTool(result);
    probedSnapshot.value = snapshot;
  } catch (error) {
    if (request !== latestProbe) return;
    probeError.value = toMessage(error);
  } finally {
    if (request === latestProbe) probing.value = false;
  }
}

function customPathUnavailable(tool: CliTool): boolean {
  const pathKey = CLI_TOOL_META[tool].pathKey;
  return (
    pathFor(tool) !== "" &&
    probedSnapshot.value?.[pathKey] === pathFor(tool) &&
    statuses.value[tool]?.available === false
  );
}

onMounted(() => void probe(false));
watch(
  () => props.refreshKey,
  (next, previous) => {
    if (next !== previous) void probe(false);
  },
);
</script>

<template>
  <section class="cli-tools-manager">
    <header class="manager-header">
      <div>
        <h3>第三方 CLI</h3>
        <p>留空时自动查找。桌面环境找不到命令时，可填写可执行文件的绝对路径。</p>
      </div>
      <button type="button" class="secondary" :disabled="probing" @click="probe(true)">
        {{ probing ? "探测中…" : "重新探测" }}
      </button>
    </header>

    <p v-if="stale" class="notice" role="status" aria-live="polite">
      路径已修改，当前探测结果已过期。
    </p>
    <p v-if="probeError" class="error" role="alert">{{ probeError }}</p>

    <div class="tool-list">
      <div v-for="tool in CLI_TOOLS" :key="tool" class="tool-row">
        <label :for="`cli-path-${tool}`">{{ CLI_TOOL_META[tool].label }}</label>
        <input
          :id="`cli-path-${tool}`"
          type="text"
          :value="pathFor(tool)"
          :placeholder="`自动探测 ${tool}`"
          :aria-invalid="Boolean(pathErrors[tool]) || customPathUnavailable(tool)"
          :aria-describedby="`cli-feedback-${tool}`"
          spellcheck="false"
          @input="onPathInput(tool, $event)"
        />
        <div :id="`cli-feedback-${tool}`" class="tool-feedback" role="status" aria-live="polite">
          <p v-if="pathErrors[tool]" class="error" role="alert">{{ pathErrors[tool] }}</p>
          <template v-else-if="statuses[tool]">
            <p
              :class="statuses[tool]?.available ? 'available' : 'error'"
              :role="statuses[tool]?.available ? undefined : 'alert'"
            >
              {{ statuses[tool]?.available ? "可用" : "不可用" }}
              <span v-if="statuses[tool]?.source">
                · {{ sourceLabel(statuses[tool]!.source) }}
              </span>
            </p>
            <p v-if="statuses[tool]?.resolvedPath" class="resolved-path">
              实际路径：{{ statuses[tool]?.resolvedPath }}
            </p>
            <p v-if="statuses[tool]?.message" class="message">{{ statuses[tool]?.message }}</p>
            <p
              v-if="statuses[tool]?.pendingRestart && statuses[tool]?.available"
              class="notice"
            >
              新路径将在相关进程重启后生效。
            </p>
          </template>
          <p v-else class="message">尚未探测</p>
        </div>
      </div>
    </div>
  </section>
</template>

<style scoped>
.cli-tools-manager {
  display: flex;
  flex-direction: column;
  gap: var(--space-5);
  max-width: 720px;
}
.manager-header {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: var(--space-5);
}
.manager-header h3,
.manager-header p,
.tool-row p {
  margin: 0;
}
.manager-header h3 {
  font-size: var(--font-size-lg);
}
.manager-header p,
.message,
.resolved-path {
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
}
.tool-list {
  display: flex;
  flex-direction: column;
  gap: var(--space-4);
}
.tool-row {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
  padding: var(--space-4);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
}
.tool-row label {
  font-size: var(--font-size-sm);
  font-weight: 600;
}
.tool-row input {
  width: 100%;
  box-sizing: border-box;
  padding: var(--space-3);
  font: inherit;
  font-family: var(--font-mono);
  color: var(--color-text);
  background: var(--color-bg);
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-sm);
}
.tool-feedback {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.resolved-path {
  overflow-wrap: anywhere;
  font-family: var(--font-mono);
}
.secondary {
  flex-shrink: 0;
  padding: var(--space-2) var(--space-4);
  font: inherit;
  color: var(--color-text);
  background: var(--color-surface);
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.secondary:disabled {
  opacity: 0.6;
  cursor: not-allowed;
}
.available {
  color: var(--color-success);
  font-size: var(--font-size-sm);
}
.error {
  margin: 0;
  color: var(--color-danger);
  font-size: var(--font-size-sm);
}
.notice {
  margin: 0;
  color: var(--color-warning, var(--color-text-muted));
  font-size: var(--font-size-sm);
}
</style>
