<script setup lang="ts">
// Terminal slice root view (#1383): a session picker (window → tab → session, grouped from
// the flat list) + a "new session" action, a connection/error banner, the lazy xterm pane,
// and the touch toolbar. Self-manages its `terminal:event` listener over the panel's
// lifetime (mirrors InboxPanel), so App.vue mounts it without central wiring.
import { computed, defineAsyncComponent, onMounted, onUnmounted } from "vue";
import { useTerminalStore } from "./useTerminalStore";
import { groupSessions } from "./sessionTree";
import { terminalBackendLabel } from "../types";
import MobileToolbar from "./MobileToolbar.vue";

// xterm needs the DOM; lazy-load the pane so the eager bundle (and the browser/SSR build)
// never pulls in the heavy xterm chunk until the terminal view is actually opened.
const XtermPane = defineAsyncComponent(() => import("./XtermPane.vue"));

const store = useTerminalStore();
const tree = computed(() => groupSessions(store.sessions.value));

// The focused session (resolved from the flat list), its backend badge, and whether the
// PTY-only "stop process" control applies (#1372): an iTerm session can't be closed that way,
// so the control shows ONLY for an active webPty session.
const activeSession = computed(
  () =>
    store.sessions.value.find((s) => s.sessionId === store.activeSessionId.value) ?? null,
);
const activeBackendLabel = computed(() =>
  activeSession.value ? terminalBackendLabel(activeSession.value.backend) : null,
);
const canStop = computed(() => activeSession.value?.backend === "webPty");

// Listener lifecycle like InboxPanel: init() subscribes BEFORE the snapshot read, so no
// frame racing the mount is dropped. The returned UnlistenFn detaches on unmount.
//
// `active` guards the async race: if the component unmounts DURING `await store.init()`,
// onUnmounted runs first (unlisten still null, nothing to detach), then init resolves with a
// live listener that would leak. Detecting `!active` after the await tears it down immediately.
let unlisten: Awaited<ReturnType<typeof store.init>> | null = null;
let active = true;
function detachForPageHide() {
  void store.detach({ keepalive: true });
}

onMounted(async () => {
  window.addEventListener("pagehide", detachForPageHide);
  unlisten = await store.init();
  if (!active) unlisten();
});
onUnmounted(() => {
  active = false;
  window.removeEventListener("pagehide", detachForPageHide);
  unlisten?.();
  void store.detach();
});

// Error-banner recovery: re-attach the focused session if one is set (its attach failed),
// otherwise re-list the sessions (the daemon precondition that failed may now be satisfied).
function retry() {
  const id = store.activeSessionId.value;
  if (id) void store.attach(id);
  else void store.refreshSessions();
}

async function retryListener() {
  unlisten?.();
  unlisten = null;
  store.resetListener();
  unlisten = await store.init();
  if (!active) unlisten();
}
</script>

<template>
  <section class="terminal-view">
    <header class="head">
      <h2>终端 / Terminal</h2>
      <span v-if="activeBackendLabel" class="badge">{{ activeBackendLabel }}</span>
      <span class="spacer" />
      <button
        type="button"
        class="new"
        :disabled="!store.listenerReady.value"
        @click="store.createAndAttach('iterm')"
      >
        新建 iTerm / New iTerm
      </button>
      <button
        type="button"
        class="new"
        :disabled="!store.listenerReady.value"
        @click="store.createAndAttach('webPty')"
      >
        新建 Shell / New shell
      </button>
      <button
        v-if="canStop"
        type="button"
        class="new stop"
        :disabled="!store.listenerReady.value || store.stopping.value"
        @click="store.stopSession()"
      >
        停止进程 / Stop process
      </button>
    </header>

    <p v-if="store.listenerError.value" class="banner error">
      <span>事件监听失败：{{ store.listenerError.value }}</span>
      <button type="button" class="banner-action" @click="retryListener()">
        重新连接 / Reconnect
      </button>
    </p>
    <p v-else-if="store.connection.value === 'attaching'" class="banner muted">
      连接中… / attaching
    </p>
    <p
      v-else-if="store.connection.value === 'error' && store.error.value"
      class="banner error"
    >
      <span>{{ store.error.value }}</span>
      <button
        type="button"
        class="banner-action"
        :disabled="!store.listenerReady.value"
        @click="retry()"
      >
        重试 / Retry
      </button>
    </p>
    <p v-else-if="store.connection.value === 'closed'" class="banner muted">
      <span>会话已结束 / Session ended.</span>
      <!-- Two explicit backends (PROD-2): a hardcoded reopen would silently relaunch an ended
           iTerm session as a Shell (and vice versa). Mirrors the header's create controls. -->
      <button
        type="button"
        class="banner-action"
        :disabled="!store.listenerReady.value"
        @click="store.createAndAttach('webPty')"
      >
        新建 Shell / New shell
      </button>
      <button
        type="button"
        class="banner-action"
        :disabled="!store.listenerReady.value"
        @click="store.createAndAttach('iterm')"
      >
        新建 iTerm / New iTerm
      </button>
    </p>

    <div class="body">
      <aside class="picker">
        <p v-if="!store.listenerReady.value" class="muted empty">加载中… / Loading…</p>
        <p v-else-if="tree.length === 0" class="muted empty">暂无会话 / No sessions.</p>
        <ul v-else class="windows">
          <li v-for="w in tree" :key="w.windowId" class="window">
            <p class="group-id">窗口 / Window {{ w.windowId }}</p>
            <ul class="tabs">
              <li v-for="t in w.tabs" :key="t.tabId" class="tab">
                <p class="group-id sub">标签 / Tab {{ t.tabId }}</p>
                <ul class="sessions">
                  <li v-for="s in t.sessions" :key="s.sessionId">
                    <button
                      type="button"
                      class="session"
                      :class="{ active: s.sessionId === store.activeSessionId.value }"
                      :disabled="!store.listenerReady.value"
                      @click="store.attach(s.sessionId)"
                    >
                      <!-- Backend badge (PROD-3): an iTerm "zsh" and a PTY "zsh" are otherwise
                           indistinguishable in the picker. -->
                      <span class="session-title">{{ s.title || s.sessionId }}</span>
                      <span class="session-backend">{{ terminalBackendLabel(s.backend) }}</span>
                    </button>
                  </li>
                </ul>
              </li>
            </ul>
          </li>
        </ul>
      </aside>

      <div class="pane">
        <div v-if="store.activeSessionId.value === null" class="placeholder muted">
          选择或新建一个会话 / Pick or create a session.
        </div>
        <template v-else>
          <div class="screen">
            <XtermPane />
          </div>
          <MobileToolbar @send="store.sendInput" />
        </template>
      </div>
    </div>
  </section>
</template>

<style scoped>
.terminal-view {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
}
.head {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-4) var(--space-6);
  border-bottom: 1px solid var(--color-border-strong);
}
.head h2 {
  margin: 0;
}
.badge {
  display: inline-block;
  padding: 1px var(--space-3);
  border-radius: var(--radius-md);
  font-size: var(--font-size-xs);
  background: var(--color-neutral-bg);
  color: var(--color-text-muted);
}
.spacer {
  flex: 1;
}
.new {
  /* Touch-first: meets the ~44px minimum touch dimension (reused for both create buttons + stop). */
  min-height: 44px;
  padding: var(--space-2) var(--space-5);
  font: inherit;
  font-size: var(--font-size-sm);
  color: var(--color-accent);
  background: none;
  border: 1px solid var(--color-accent);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
/* The PTY-only "stop process" control reads as a destructive action. */
.new.stop {
  color: var(--color-danger);
  border-color: var(--color-danger);
}
/* Gated controls (disabled until the terminal:event listener is ready) read as inert so an
   action can't fire before the listener could catch the resulting `attached` event. */
.new:disabled,
.session:disabled,
.banner-action:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.banner {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  gap: var(--space-3);
  margin: 0;
  padding: var(--space-3) var(--space-6);
  font-size: var(--font-size-sm);
}
.banner.error {
  color: var(--color-danger);
  background: var(--color-danger-bg);
}
.banner.muted {
  color: var(--color-text-muted);
}
/* Inline recovery CTA — link-style so it sits inside the banner without competing with it.
   ≥44px min touch target (PROD-2); the first CTA's margin-left:auto pushes the group right,
   the banner's flex `gap` spaces multiple CTAs (e.g. the closed banner's two reopen buttons). */
.banner-action {
  flex: none;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  min-height: 44px;
  padding: var(--space-1) var(--space-3);
  font: inherit;
  font-size: var(--font-size-sm);
  color: inherit;
  background: none;
  border: 1px solid currentColor;
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.banner-action:first-of-type {
  margin-left: auto;
}
.body {
  display: flex;
  flex: 1;
  min-height: 0;
}
.picker {
  width: var(--sidebar-width);
  flex: none;
  overflow-y: auto;
  padding: var(--space-4);
  border-right: 1px solid var(--color-border-strong);
}
.windows,
.tabs,
.sessions {
  list-style: none;
  margin: 0;
  padding: 0;
}
.tabs {
  margin-left: var(--space-3);
}
.group-id {
  margin: var(--space-3) 0 var(--space-2);
  font-size: var(--font-size-xs);
  color: var(--color-text-muted);
}
.group-id.sub {
  margin-top: var(--space-2);
}
.session {
  display: flex;
  align-items: center;
  gap: var(--space-2);
  width: 100%;
  min-height: 44px; /* touch-friendly picker row (PROD-3) */
  text-align: left;
  padding: var(--space-2) var(--space-3);
  margin-bottom: var(--space-1);
  font: inherit;
  font-size: var(--font-size-sm);
  color: var(--color-text);
  background: none;
  border: 1px solid transparent;
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.session:hover {
  background: var(--color-surface-hover);
}
/* Title takes the row, truncating; the backend badge stays a compact fixed tag on the right. */
.session-title {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.session-backend {
  flex: none;
  padding: 1px var(--space-2);
  border-radius: var(--radius-md);
  font-size: var(--font-size-xs);
  color: var(--color-text-muted);
  background: var(--color-neutral-bg);
}
.session.active {
  color: var(--color-accent);
  border-color: var(--color-accent);
  background: var(--color-accent-badge-bg);
}
.pane {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-height: 0;
}
.placeholder {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
}
.screen {
  flex: 1;
  min-height: 0;
  padding: var(--space-2);
  background: var(--color-bg);
}
.muted {
  color: var(--color-text-muted);
}
.empty {
  padding: var(--space-3);
}

@media (max-width: 640px) {
  .head {
    flex-wrap: wrap;
    gap: var(--space-2);
    padding: var(--space-3);
  }
  .head h2 {
    font-size: var(--font-size-md);
  }
  .new {
    width: 100%;
  }
  .body {
    flex-direction: column;
  }
  .picker {
    width: auto;
    max-height: 34vh;
    border-right: 0;
    border-bottom: 1px solid var(--color-border-strong);
  }
  .screen {
    padding: var(--space-1);
  }
}
</style>
