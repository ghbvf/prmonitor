<script setup lang="ts">
// Review streaming panel: starts/stops a review for the selected PR and renders
// the streamed deltas. The selected PR is passed down by the composition root
// (App.vue) so the pr slice and review slice stay decoupled.
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { useProjects } from "../projects";
import type { PullRequestView } from "../types";
import ReviewStream from "./ReviewStream.vue";
import { useReviewStore } from "./useReviewStore";

const { activeProjectId } = useProjects();

const props = defineProps<{ selectedPr: PullRequestView | null }>();

const {
  items,
  running,
  finalStatus,
  error,
  activePr,
  activeThreadId,
  listenerReady,
  listenerError,
  start,
  stop,
  sendMessage,
  init,
} = useReviewStore();

// Follow-up chat composer (#chat). Local UI state: the draft text and whether the
// composer is collapsed to a thin header bar.
const draft = ref("");
const collapsed = ref(false);

// The scrollable conversation viewport (middle zone). Auto-scroll follows new streamed
// output, but ONLY while the user is already pinned to the bottom — if they scrolled up
// to read history, streaming must not yank them back down.
const streamScroll = ref<HTMLElement | null>(null);
const stickToBottom = ref(true);
// Sub-pixel rounding + the gap below the last item mean "at bottom" is rarely exactly 0;
// treat anything within this margin of the bottom as still pinned.
const STICK_THRESHOLD_PX = 24;

function onStreamScroll() {
  const el = streamScroll.value;
  if (!el) return;
  stickToBottom.value =
    el.scrollHeight - el.scrollTop - el.clientHeight <= STICK_THRESHOLD_PX;
}

// Follow streamed deltas to the bottom when pinned. `deep` catches in-place text appends
// onto existing item objects (not just pushes); `flush: "post"` runs after the DOM (and
// thus scrollHeight) reflects the new content.
watch(
  items,
  () => {
    if (!stickToBottom.value) return;
    const el = streamScroll.value;
    if (el) el.scrollTop = el.scrollHeight;
  },
  { deep: true, flush: "post" },
);

// When a session becomes focused (freshly started or picked from the list), auto-expand
// the composer so its chat isn't hidden behind a prior manual collapse, and re-pin to the
// bottom so the new session's stream follows. Only on the null → non-null edge — a manual
// collapse during an active session is preserved.
watch(activeThreadId, (id) => {
  if (id != null) {
    collapsed.value = false;
    stickToBottom.value = true;
  }
});

// The composer is usable only when a session is focused, no turn is running (the
// input FREEZES during a review/follow-up turn), and the event listener is attached
// (a reply could otherwise be missed). Mirrors the store's `sendMessage` guard.
const canChat = computed(
  () =>
    activeThreadId.value != null && !running.value && listenerReady.value,
);

function onSend() {
  const text = draft.value.trim();
  if (!text || !canChat.value || activeThreadId.value == null) return;
  // `sendMessage` re-checks the same guard, so a race that flipped `running` between
  // the click and here is still safe (it no-ops). Clear the draft optimistically.
  sendMessage(activeProjectId.value, activeThreadId.value, text);
  draft.value = "";
}

// Enter sends; Shift+Enter inserts a newline (default textarea behavior, so we only
// intercept the bare Enter). A composing IME Enter (keyCode 229 / isComposing) must
// NOT send — it's committing a candidate.
function onKeydown(e: KeyboardEvent) {
  if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
    e.preventDefault();
    onSend();
  }
}

// Friendly label for the codex turn status ("interrupted" / "completed" / ...).
const finalLabel = computed(() => {
  switch (finalStatus.value) {
    case "completed":
      return "完成 / completed";
    case "interrupted":
      return "已停止 / interrupted";
    case "failed":
      return "失败 / failed";
    default:
      return finalStatus.value ?? "";
  }
});

// Distinguish start-time failures (never got a session / stream) from mid-run
// errors (had a thread, streamed items, or a prior finalStatus). Shown only when
// `error` is set and `finalStatus` is absent (see status line template order).
const errorStatusLabel = computed(() => {
  if (
    activeThreadId.value != null ||
    items.value.length > 0 ||
    finalStatus.value != null
  ) {
    return "运行失败 / 会话出错";
  }
  return "启动失败 / failed";
});

// Attach the streamed-event listener for the panel's lifetime. Mirrors PollControls'
// mount/unmount pattern. Codex/claude availability is probed by the always-mounted
// StatusBar (gated to the engines actually in use), so this panel no longer probes
// codex — a passive probe lazily spawns the resident codex app-server, which a
// claude-only config must never trigger.
let unlisten: Awaited<ReturnType<typeof init>> | null = null;
onMounted(async () => {
  unlisten = await init();
});
onUnmounted(() => unlisten?.());

function onStart() {
  if (props.selectedPr)
    start(activeProjectId.value, props.selectedPr.number, props.selectedPr.kind);
}
</script>

<template>
  <section class="review-panel">
    <header class="head">
      <h2>Review</h2>
      <div class="actions">
        <button
          type="button"
          :disabled="selectedPr == null || running || !listenerReady"
          @click="onStart"
        >
          开始 review
        </button>
        <button type="button" :disabled="!running" @click="stop">停止</button>
      </div>
    </header>

    <p class="status">
      <template v-if="activePr != null">
        PR #{{ activePr }} —
        <span v-if="running">运行中… / running</span>
        <span v-else-if="finalStatus">已结束 / {{ finalLabel }}</span>
        <span v-else-if="error">{{ errorStatusLabel }}</span>
        <span v-else>未开始</span>
      </template>
      <span v-else-if="selectedPr">
        已选中 PR #{{ selectedPr.number }}（{{ selectedPr.kind }}）— 点「开始 review」
      </span>
      <span v-else class="muted">Select a PR to review.</span>
    </p>

    <p v-if="error" class="error">{{ error }}</p>
    <p v-if="listenerError" class="error">事件监听注册失败 / {{ listenerError }}</p>
    <p v-else-if="!listenerReady" class="muted">正在连接事件流… / connecting</p>

    <!-- Scrollable conversation: only the stream scrolls; the header/status above and
         the composer below stay pinned to the panel edges (see .stream-scroll styles). -->
    <div ref="streamScroll" class="stream-scroll" @scroll="onStreamScroll">
      <p v-if="running && items.length === 0" class="muted">等待输出… / waiting</p>

      <ReviewStream :items="items" />
    </div>

    <!-- Follow-up chat composer (#chat). Pinned to the panel bottom; collapses to a thin
         header bar via the toggle; frozen (disabled) while a turn is running or no session
         is focused. -->
    <div class="composer" :class="{ collapsed }">
      <div class="composer-head">
        <span class="composer-title">对话 / Chat</span>
        <button
          type="button"
          class="toggle"
          :aria-label="collapsed ? '展开 / expand' : '缩小 / collapse'"
          :title="collapsed ? '展开 / expand' : '缩小 / collapse'"
          @click="collapsed = !collapsed"
        >
          {{ collapsed ? "▲" : "▼" }}
        </button>
      </div>

      <template v-if="!collapsed">
        <!-- No focused session: show ONLY the hint. The disabled input row would be
             visually redundant, so it isn't rendered until a session is focused. -->
        <p v-if="activeThreadId == null" class="muted hint">
          运行 review 后可对话 / Run a review to chat
        </p>
        <div v-else class="composer-body">
          <textarea
            v-model="draft"
            class="composer-input"
            rows="3"
            :disabled="!canChat"
            :placeholder="
              running
                ? '运行中，请稍候… / running…'
                : '输入消息，Enter 发送、Shift+Enter 换行 / Enter to send'
            "
            @keydown="onKeydown"
          ></textarea>
          <button
            type="button"
            class="send"
            :disabled="!canChat || draft.trim().length === 0"
            @click="onSend"
          >
            发送 / Send
          </button>
        </div>
      </template>
    </div>
  </section>
</template>

<style scoped>
/* Three-zone panel: header/status pinned at the top, the conversation scrolls in the
   middle, and the composer is pinned at the bottom. height:100% fills the SplitPane
   bottom .pane's content box (the pane has a definite flex height + small vertical
   padding), so the pane itself stops scrolling and .stream-scroll becomes the only
   visible scrollbar. */
.review-panel {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
}
.stream-scroll {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
}
/* Fixed top zone: never compress, so the controls/status stay legible when the pane is
   dragged short — symmetric with .composer's flex-shrink:0 at the bottom. */
.head,
.status,
.error {
  flex-shrink: 0;
}
.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
}
.head h2 {
  margin: 0;
}
.actions {
  display: flex;
  gap: var(--space-3);
}
.actions button {
  padding: var(--space-2) var(--space-5);
  font: inherit;
  cursor: pointer;
}
.actions button:disabled {
  cursor: default;
  opacity: 0.5;
}
.status {
  margin: var(--space-4) 0 0;
  font-size: var(--font-size-md);
}
.muted {
  color: var(--color-text-muted);
}
.error {
  margin: var(--space-4) 0 0;
  color: var(--color-danger);
  font-size: var(--font-size-sm);
}
.composer {
  margin-top: var(--space-4);
  border-top: 1px solid var(--color-border);
  padding-top: var(--space-3);
  /* Never compress when the conversation grows — stays pinned at the panel bottom. */
  flex-shrink: 0;
}
.composer-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-3);
}
.composer-title {
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
}
.composer .toggle {
  padding: var(--space-1) var(--space-3);
  font: inherit;
  font-size: var(--font-size-sm);
  /* Stabilize the ▲/▼ glyph baseline/height across platform fonts (macOS vs Windows). */
  line-height: 1;
  cursor: pointer;
}
.hint {
  margin: var(--space-3) 0 0;
  font-size: var(--font-size-sm);
}
.composer-body {
  margin-top: var(--space-3);
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}
.composer-input {
  font: inherit;
  padding: var(--space-2) var(--space-3);
  resize: vertical;
  border: 1px solid var(--color-border-strong);
  border-radius: var(--space-2);
  background: var(--color-surface);
  color: var(--color-text);
}
.composer-input:disabled {
  opacity: 0.5;
  cursor: default;
}
.composer .send {
  align-self: flex-end;
  padding: var(--space-2) var(--space-5);
  font: inherit;
  cursor: pointer;
}
.composer .send:disabled {
  opacity: 0.5;
  cursor: default;
}
</style>
