<script setup lang="ts">
// App-shell status strip (composition layer, sibling of App.vue): surfaces the
// status of every source/engine tool the ENABLED projects actually use — gh / az
// (pr store) and codex / claude / cursor (review store) — deduped across projects. Living at
// the shell layer (not inside a slice) is what makes reading across both slices
// legitimate, exactly as App.vue does its cross-slice wiring. "选了什么显示什么": a tool
// appears only when an enabled project selects it.
import { computed, watch } from "vue";
import { useConfigStore } from "./config/useConfigStore";
import { assertNever, statusBarSourceTool } from "./types";
import type { EngineKind, SourceTool } from "./types";
import { usePrStore } from "./pr/usePrStore";
import { useReviewStore } from "./review/useReviewStore";

const store = usePrStore();
const configStore = useConfigStore();
// Destructure the refs so the template auto-unwraps them (the review store is a plain
// factory object, not a Pinia store, so `review.codex` would stay a Ref).
const {
  codex,
  claude,
  cursor,
  refreshCodexStatus,
  refreshClaudeStatus,
  refreshCursorStatus,
  startCodexServer,
  stopCodexServer,
  startCursorServer,
  stopCursorServer,
} = useReviewStore();

type DotClass = "ok" | "warn" | "idle";

// Shared placeholder text + status-dot helpers so the gh/az and engine items
// can't drift in wording or color logic.
const CHECKING = "检查中… / checking";
const STARTING = "初始化中… / starting";
function authDot(s: { authenticated: boolean } | null): DotClass {
  return s == null ? "idle" : s.authenticated ? "ok" : "warn";
}
function availDot(s: { available: boolean } | null): DotClass {
  return s == null ? "idle" : s.available ? "ok" : "warn";
}

const enabledProjects = computed(
  () => configStore.config?.projects.filter((p) => p.enabled) ?? [],
);

// Deduped source tools surfaced by the enabled projects (webhook-only → none).
const usedSourceTools = computed<SourceTool[]>(() => {
  const set = new Set<SourceTool>();
  for (const p of enabledProjects.value) {
    const tool = statusBarSourceTool(p.sourceKind, p.updateMode);
    if (tool) set.add(tool);
  }
  return [...set];
});

// Deduped review engines used by the enabled projects. Unlike usedSourceTools this is
// NOT mode-gated: a webhook-only project still surfaces its engine (an inbound webhook
// can auto-trigger a review), so the engine status stays relevant even with no poll loop.
const usedEngineKinds = computed<EngineKind[]>(() => {
  const set = new Set<EngineKind>();
  for (const p of enabledProjects.value) set.add(p.engineKind);
  return [...set];
});

interface SourceItem {
  tool: SourceTool;
  label: string;
  dot: DotClass;
  text: string;
  retriable: boolean; // gh/az have a live probe to re-run on click; bitbucket has none
}

// One view-model per used source tool. gh / az read live auth status from the pr
// store (null → idle "检查中…" so a cold start shows no false warning). Bitbucket has
// no probeable CLI/health endpoint — surface a neutral, config-derived item.
const sourceItems = computed<SourceItem[]>(() =>
  usedSourceTools.value.map((tool) => {
    switch (tool) {
      case "gh": {
        const s = store.gh;
        return { tool, label: "gh", dot: authDot(s), text: s?.message ?? CHECKING, retriable: true };
      }
      case "az": {
        const s = store.az;
        return { tool, label: "az", dot: authDot(s), text: s?.message ?? CHECKING, retriable: true };
      }
      case "bitbucket":
        return { tool, label: "Bitbucket", dot: "idle", text: "REST · 按需调用", retriable: false };
      default:
        return assertNever(tool);
    }
  }),
);

interface EngineItem {
  kind: EngineKind;
  label: string;
  dot: DotClass;
  text: string;
  // codex / cursor are resident servers with a 启动/停止 toggle; the label reflects the
  // user intent (desiredRunning). claude is one-shot (no server) → no button (null).
  buttonLabel: string | null;
}

function residentEngineItem(
  kind: "codex" | "cursor",
  label: string,
  s: { available: boolean; desiredRunning: boolean; message: string } | null,
): EngineItem {
  // idle when null OR user-stopped (desiredRunning false); else ok/warn by reach.
  const dot: DotClass =
    s == null
      ? "idle"
      : s.desiredRunning === false
        ? "idle"
        : s.available
          ? "ok"
          : "warn";
  return {
    kind,
    label,
    dot,
    text: s == null ? STARTING : s.message,
    buttonLabel: s == null ? null : s.desiredRunning === false ? "启动" : "停止",
  };
}

const engineItems = computed<EngineItem[]>(() =>
  usedEngineKinds.value.map((kind) => {
    switch (kind) {
      case "codex":
        return residentEngineItem(kind, "codex", codex.value);
      case "cursor":
        return residentEngineItem(kind, "cursor", cursor.value);
      case "claude": {
        const s = claude.value;
        return {
          kind,
          label: "claude",
          dot: availDot(s),
          text: s == null ? STARTING : s.message,
          buttonLabel: null,
        };
      }
      default:
        return assertNever(kind);
    }
  }),
);

// Toggle a resident engine server from its status item's button. Reads the LIVE
// desired-running intent (not a captured flag) so it can't act on a stale label.
function onEngineButton(kind: EngineKind) {
  switch (kind) {
    case "codex":
      if (codex.value?.desiredRunning === false) startCodexServer();
      else stopCodexServer();
      return;
    case "cursor":
      if (cursor.value?.desiredRunning === false) startCursorServer();
      else stopCursorServer();
      return;
    case "claude":
      return; // one-shot; no resident server
    default:
      return assertNever(kind);
  }
}

// Manually re-probe a tool's status (the status items are clickable). The watcher only
// fires when the used-set changes, so after the user fixes auth/install externally (e.g.
// `az login`, install/login claude) this is how a stale failure gets re-checked without
// an app restart. Resident engines' passive probes return "已停止" without spawning when
// the user explicitly stopped them, so a re-probe is safe.
function retrySource(tool: SourceTool) {
  if (tool === "gh") void store.refreshGhStatus();
  else if (tool === "az") void store.refreshAzStatus();
  // bitbucket has no live probe (retriable=false; not clickable in the template)
}
function retryEngine(kind: EngineKind) {
  switch (kind) {
    case "codex":
      void refreshCodexStatus();
      return;
    case "claude":
      void refreshClaudeStatus();
      return;
    case "cursor":
      void refreshCursorStatus();
      return;
    default:
      return assertNever(kind);
  }
}

// Probe only the tools actually in use, and re-probe when the used set changes (e.g.
// a config save flips a project's source/engine). Mirrors the old `ghRequired` watch.
// Probing a resident engine ONLY when that engine's project is enabled is deliberate:
// a passive `get_*_status` lazily SPAWNS the resident server, so a claude-only config
// must never trigger codex/cursor. gh/az re-probe freely (the store's loading guard
// coalesces); engines probe only when still null, so a user's 启动/停止 result (or
// another updater) is never clobbered.
watch(
  usedSourceTools,
  (tools) => {
    if (tools.includes("gh")) void store.refreshGhStatus();
    if (tools.includes("az")) void store.refreshAzStatus();
  },
  { immediate: true },
);
watch(
  usedEngineKinds,
  (kinds) => {
    if (kinds.includes("codex") && codex.value == null) void refreshCodexStatus();
    if (kinds.includes("claude") && claude.value == null) void refreshClaudeStatus();
    if (kinds.includes("cursor") && cursor.value == null) void refreshCursorStatus();
  },
  { immediate: true },
);

// A successful config save replaces these values in the shared Pinia store without changing the
// engine set. Re-probe the affected engine so a stale module-level status cannot survive a CLI path
// change. The resident-engine probe reads the lifecycle snapshot first, so this never kills or
// restarts an already-running app-server; a cold manager uses the newly configured path.
watch(
  () =>
    [
      configStore.config?.cliTools.codexPath,
      configStore.config?.cliTools.claudePath,
      configStore.config?.cliTools.agentPath,
    ] as const,
  ([codexPath, claudePath, agentPath], [previousCodexPath, previousClaudePath, previousAgentPath]) => {
    const kinds = usedEngineKinds.value;
    if (codexPath !== previousCodexPath && kinds.includes("codex")) {
      void refreshCodexStatus();
    }
    if (claudePath !== previousClaudePath && kinds.includes("claude")) {
      void refreshClaudeStatus();
    }
    if (agentPath !== previousAgentPath && kinds.includes("cursor")) {
      void refreshCursorStatus();
    }
  },
);
</script>

<template>
  <footer class="status-bar">
    <span
      v-for="item in sourceItems"
      :key="item.tool"
      class="item"
      :class="{ clickable: item.retriable }"
      :title="item.retriable ? '点击刷新状态' : undefined"
      @click="item.retriable && retrySource(item.tool)"
    >
      <span class="dot" :class="item.dot"></span>
      <span class="text">{{ item.label }} — {{ item.text }}</span>
    </span>

    <span
      v-for="item in engineItems"
      :key="item.kind"
      class="item clickable"
      title="点击刷新状态"
      @click="retryEngine(item.kind)"
    >
      <span class="dot" :class="item.dot"></span>
      <span class="text">{{ item.label }} — {{ item.text }}</span>
      <button
        v-if="item.buttonLabel"
        type="button"
        class="codex-btn"
        @click.stop="onEngineButton(item.kind)"
      >
        {{ item.buttonLabel }}
      </button>
    </span>
  </footer>
</template>

<style scoped>
.status-bar {
  display: flex;
  flex-wrap: wrap; /* up to 6 tool items (gh/az/bitbucket + codex/claude/cursor) wrap on a narrow window instead of overflowing; `gap` supplies the row gap */
  align-items: center;
  gap: var(--space-3) var(--space-8);
  padding: var(--space-3) var(--space-8);
  border-top: 1px solid var(--color-border-strong);
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
}
.item {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.item.clickable {
  cursor: pointer;
}
.dot {
  width: 8px;
  height: 8px;
  border-radius: var(--radius-full);
  background: var(--color-text-muted);
}
.dot.ok {
  background: var(--color-success);
}
.dot.warn {
  background: var(--color-warn-dot);
}
.dot.idle {
  background: var(--color-text-muted);
}
.codex-btn {
  margin-left: var(--space-3);
  padding: 0 var(--space-3);
  border: 1px solid var(--color-border-strong);
  border-radius: var(--radius-sm);
  background: transparent;
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
  line-height: 1.6;
  cursor: pointer;
}
.codex-btn:hover {
  color: var(--color-text);
  border-color: var(--color-text-muted);
}
</style>
