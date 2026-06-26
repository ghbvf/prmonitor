<script setup lang="ts">
// Terminal slice root view (#1383): a session picker (window → tab → session, grouped from
// the flat list) + a "new session" action, a connection/error banner, the lazy xterm pane,
// and the touch toolbar. Self-manages its `terminal:event` listener over the panel's
// lifetime (mirrors InboxPanel), so App.vue mounts it without central wiring.
import { computed, defineAsyncComponent, onMounted, onUnmounted } from "vue";
import { useTerminalStore } from "./useTerminalStore";
import { groupSessions } from "./sessionTree";
import MobileToolbar from "./MobileToolbar.vue";

// xterm needs the DOM; lazy-load the pane so the eager bundle (and the browser/SSR build)
// never pulls in the heavy xterm chunk until the terminal view is actually opened.
const XtermPane = defineAsyncComponent(() => import("./XtermPane.vue"));

const store = useTerminalStore();
const tree = computed(() => groupSessions(store.sessions.value));

// Listener lifecycle like InboxPanel: init() subscribes BEFORE the snapshot read, so no
// frame racing the mount is dropped. The returned UnlistenFn detaches on unmount.
//
// `active` guards the async race: if the component unmounts DURING `await store.init()`,
// onUnmounted runs first (unlisten still null, nothing to detach), then init resolves with a
// live listener that would leak. Detecting `!active` after the await tears it down immediately.
let unlisten: Awaited<ReturnType<typeof store.init>> | null = null;
let active = true;
onMounted(async () => {
  unlisten = await store.init();
  if (!active) unlisten();
});
onUnmounted(() => {
  active = false;
  unlisten?.();
});

// Error-banner recovery: re-attach the focused session if one is set (its attach failed),
// otherwise re-list the sessions (the daemon precondition that failed may now be satisfied).
function retry() {
  const id = store.activeSessionId.value;
  if (id) void store.attach(id);
  else void store.refreshSessions();
}
</script>

<template>
  <section class="terminal-view">
    <header class="head">
      <h2>终端 / Terminal</h2>
      <span class="badge">iTerm</span>
      <span class="spacer" />
      <button
        type="button"
        class="new"
        :disabled="!store.listenerReady.value"
        @click="store.createAndAttach()"
      >
        新建会话 / New session
      </button>
    </header>

    <p v-if="store.listenerError.value" class="banner error">
      事件监听注册失败：{{ store.listenerError.value }}
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
      <button
        type="button"
        class="banner-action"
        :disabled="!store.listenerReady.value"
        @click="store.createAndAttach()"
      >
        新建会话 / New session
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
                      {{ s.title || s.sessionId }}
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
  padding: var(--space-2) var(--space-5);
  font: inherit;
  font-size: var(--font-size-sm);
  color: var(--color-accent);
  background: none;
  border: 1px solid var(--color-accent);
  border-radius: var(--radius-sm);
  cursor: pointer;
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
/* Inline recovery CTA — link-style so it sits inside the banner without competing with it. */
.banner-action {
  flex: none;
  margin-left: auto;
  padding: var(--space-1) var(--space-3);
  font: inherit;
  font-size: var(--font-size-sm);
  color: inherit;
  background: none;
  border: 1px solid currentColor;
  border-radius: var(--radius-sm);
  cursor: pointer;
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
  display: block;
  width: 100%;
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
</style>
