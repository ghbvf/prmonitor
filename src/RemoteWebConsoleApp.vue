<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import ReviewStream from "./review/ReviewStream.vue";
import type { ReviewEvent, TrackedPrView } from "./types";
import type {
  ReviewReceiptId,
  ReviewReceiptSnapshot,
  ReviewReceiptStatus,
} from "./types.generated";
import type { ReviewSession, StreamItem } from "./review/types";
import type { UnlistenFn } from "./transport";
import {
  getRemoteClaudeStatus,
  getRemoteCodexStatus,
  getRemoteCursorStatus,
  getRemoteReviewReceipt,
  getRemotePrs,
  getRemotePrSessions,
  getRemoteSessionHistory,
  listRemoteReviewSessions,
  createExternalRequestId,
  onRemotePrEvent,
  onRemoteReviewEvent,
  remoteConsoleSnapshot,
  requestRemoteReview,
  stopRemoteReview,
  type RemoteConsoleSnapshot,
} from "./remoteConsole/api";
import {
  invalidateReceiptState,
  mergeSessionsByThread,
  pollReceiptOnce,
  RequestIdLifecycle,
} from "./remoteConsole/receiptLifecycle";

const snapshot = ref<RemoteConsoleSnapshot | null>(null);
const activeProjectId = ref("");
const prs = ref<Record<string, TrackedPrView[]>>({});
const sessions = ref<ReviewSession[]>([]);
const selectedPrNumber = ref<number | null>(null);
const focusedThreadId = ref<string | null>(null);
const streams = ref<Record<string, StreamItem[]>>({});
const loading = ref(true);
const busy = ref(false);
const error = ref<string | null>(null);
const codexStatus = ref("");
const claudeStatus = ref("");
const cursorStatus = ref("");
const receipt = ref<ReviewReceiptSnapshot | null>(null);
const activeReceiptId = ref<ReviewReceiptId | null>(null);
const receiptPollingStopped = ref(false);
const unlisteners: UnlistenFn[] = [];
let receiptTimer: ReturnType<typeof setTimeout> | null = null;
const MAX_RECEIPT_POLL_FAILURES = 5;
const requestIds = new RequestIdLifecycle(createExternalRequestId);

const projects = computed(() => snapshot.value?.projects ?? []);
const activeProject = computed(
  () => projects.value.find((p) => p.id === activeProjectId.value) ?? projects.value[0] ?? null,
);

function blockedResumeLabel(engineKind: string | undefined): string {
  switch (engineKind) {
    case "cursor":
      return "Blocked — resume Cursor to continue";
    case "claude":
      return "Blocked — resume Claude to continue";
    case "codex":
      return "Blocked — resume Codex to continue";
    default:
      return "Blocked — resume review engine to continue";
  }
}

const receiptStatusLabels = computed<Record<ReviewReceiptStatus, string>>(() => ({
  received: "Received",
  queued: "Queued",
  blocked: blockedResumeLabel(activeProject.value?.engineKind),
  starting: "Starting",
  running: "Running",
  interrupting: "Interrupting",
  done: "Done",
  failed: "Failed",
}));
const activePrs = computed(() => prs.value[activeProject.value?.id ?? ""] ?? []);
const selectedPr = computed(
  () => activePrs.value.find((pr) => pr.number === selectedPrNumber.value) ?? activePrs.value[0] ?? null,
);
const activeSessions = computed(() =>
  sessions.value
    .filter((s) => s.projectId === activeProject.value?.id)
    .filter((s) => selectedPr.value === null || s.prNumber === selectedPr.value.number)
    .sort((a, b) => b.createdAtEpoch - a.createdAtEpoch),
);
const focusedSession = computed(
  () => activeSessions.value.find((s) => s.threadId === focusedThreadId.value) ?? activeSessions.value[0] ?? null,
);
const focusedItems = computed(() =>
  focusedSession.value ? streams.value[focusedSession.value.threadId] ?? [] : [],
);

function toMessage(err: unknown): string {
  return (err as { message?: string })?.message ?? String(err);
}

function setError(err: unknown) {
  error.value = toMessage(err);
}

async function refreshStatuses() {
  const [codex, claude, cursor] = await Promise.allSettled([
    getRemoteCodexStatus(),
    getRemoteClaudeStatus(),
    getRemoteCursorStatus(),
  ]);
  codexStatus.value =
    codex.status === "fulfilled" ? formatResident(codex.value) : toMessage(codex.reason);
  claudeStatus.value =
    claude.status === "fulfilled" ? claude.value.message : toMessage(claude.reason);
  cursorStatus.value =
    cursor.status === "fulfilled" ? formatResident(cursor.value) : toMessage(cursor.reason);
}

/** Codex / Cursor resident status: message plus desiredRunning + available. */
function formatResident(status: {
  available: boolean;
  desiredRunning: boolean;
  message: string;
}): string {
  const intent = status.desiredRunning ? "desired" : "stopped";
  const reach = status.available ? "available" : "unavailable";
  return `${status.message} · ${intent}/${reach}`;
}

async function refreshProject(projectId: string) {
  const [list, liveSessions] = await Promise.all([
    getRemotePrs(projectId),
    listRemoteReviewSessions(),
  ]);
  prs.value = { ...prs.value, [projectId]: list };
  sessions.value = liveSessions;
  const nextPr = selectedPrNumber.value ?? list[0]?.number ?? null;
  selectedPrNumber.value = nextPr;
  if (nextPr !== null) {
    await loadPrSessions(projectId, nextPr);
  }
}

async function loadPrSessions(projectId: string, prNumber: number) {
  const durable = await getRemotePrSessions(projectId, prNumber);
  sessions.value = mergeSessionsByThread(sessions.value, durable);
}

function invalidateActiveReceipt() {
  const state = {
    receipt: receipt.value,
    activeReceiptId: activeReceiptId.value,
    receiptPollingStopped: receiptPollingStopped.value,
  };
  invalidateReceiptState(state, stopReceiptPolling);
  receipt.value = state.receipt;
  activeReceiptId.value = state.activeReceiptId;
  receiptPollingStopped.value = state.receiptPollingStopped;
  requestIds.selectionChanged();
}

async function hydrateFocusedHistory() {
  const session = focusedSession.value;
  if (!session || streams.value[session.threadId]?.length) return;
  try {
    const history = await getRemoteSessionHistory(
      session.projectId,
      session.prNumber,
      session.threadId,
    );
    streams.value = { ...streams.value, [session.threadId]: history };
  } catch (err) {
    error.value = toMessage(err);
    streams.value = { ...streams.value, [session.threadId]: [] };
  }
}

function mergeSessionTerminal(event: Extract<ReviewEvent, { kind: "turnCompleted" }>) {
  sessions.value = sessions.value.map((s) =>
    s.threadId === event.threadId
      ? { ...s, status: event.status === "failed" ? "failed" : "done", commentUrl: event.commentUrl }
      : s,
  );
}

function appendStreamItem(threadId: string, item: StreamItem) {
  const current = streams.value[threadId] ?? [];
  const idx = current.findIndex((existing) => existing.itemId === item.itemId && existing.kind === item.kind);
  const next =
    idx === -1
      ? [...current, item]
      : current.map((existing, i) =>
          i === idx ? { ...existing, text: existing.text + item.text } : existing,
        );
  streams.value = { ...streams.value, [threadId]: next };
}

function applyReviewEvent(event: ReviewEvent) {
  if (event.kind === "messageDelta") {
    appendStreamItem(event.threadId, { itemId: event.itemId, kind: "message", text: event.text });
  } else if (event.kind === "reasoningDelta") {
    appendStreamItem(event.threadId, { itemId: event.itemId, kind: "reasoning", text: event.text });
  } else if (event.kind === "turnCompleted") {
    mergeSessionTerminal(event);
  } else if (event.kind === "error") {
    appendStreamItem(event.threadId, {
      itemId: `error-${Date.now()}`,
      kind: "message",
      text: event.message,
    });
  } else if (event.kind === "dispatchError") {
    error.value = event.message;
  }
}

async function selectProject(projectId: string) {
  busy.value = true;
  error.value = null;
  try {
    invalidateActiveReceipt();
    activeProjectId.value = projectId;
    selectedPrNumber.value = null;
    focusedThreadId.value = null;
    await refreshProject(projectId);
    await hydrateFocusedHistory();
  } catch (err) {
    error.value = toMessage(err);
  } finally {
    busy.value = false;
  }
}

async function selectPr(number: number) {
  busy.value = true;
  error.value = null;
  try {
    invalidateActiveReceipt();
    selectedPrNumber.value = number;
    focusedThreadId.value = null;
    if (!activeProject.value) return;
    await loadPrSessions(activeProject.value.id, number);
    await hydrateFocusedHistory();
  } catch (err) {
    error.value = toMessage(err);
  } finally {
    busy.value = false;
  }
}

async function focusSession(threadId: string) {
  error.value = null;
  try {
    focusedThreadId.value = threadId;
    await hydrateFocusedHistory();
  } catch (err) {
    error.value = toMessage(err);
  }
}

function stopReceiptPolling() {
  if (receiptTimer !== null) clearTimeout(receiptTimer);
  receiptTimer = null;
}

async function refreshReceipt(
  receiptId: ReviewReceiptId,
  failureCount = 0,
): Promise<void> {
  try {
    const result = await pollReceiptOnce(
      receiptId,
      getRemoteReviewReceipt,
      listRemoteReviewSessions,
    );
    const next = result.receipt;
    if (activeReceiptId.value !== receiptId) return;
    receipt.value = next;
    receiptPollingStopped.value = false;
    error.value = null;
    if (next.threadId) {
      const firstSeen = !sessions.value.some((session) => session.threadId === next.threadId);
      if (firstSeen && activeProject.value && selectedPr.value) {
        try {
          await loadPrSessions(activeProject.value.id, selectedPr.value.number);
        } catch (sessionError) {
          error.value = `Session refresh failed: ${toMessage(sessionError)}`;
        }
      }
      focusedThreadId.value = next.threadId;
      await hydrateFocusedHistory();
    }
    if (next.status === "done" || next.status === "failed") {
      stopReceiptPolling();
      if (result.sessions) sessions.value = mergeSessionsByThread(sessions.value, result.sessions);
      if (result.sessionRefreshError) {
        error.value = `Session refresh failed: ${toMessage(result.sessionRefreshError)}`;
      }
      if (next.error) error.value = next.error;
      return;
    }
    receiptTimer = setTimeout(() => void refreshReceipt(receiptId), 1000);
  } catch (err) {
    if (activeReceiptId.value !== receiptId) return;
    stopReceiptPolling();
    const nextFailure = failureCount + 1;
    if (nextFailure < MAX_RECEIPT_POLL_FAILURES) {
      const delayMs = Math.min(1000 * 2 ** (nextFailure - 1), 8000);
      error.value = `Receipt #${receiptId} 查询暂时失败，${delayMs / 1000}s 后重试：${toMessage(err)}`;
      receiptTimer = setTimeout(
        () => void refreshReceipt(receiptId, nextFailure),
        delayMs,
      );
      return;
    }
    receiptPollingStopped.value = true;
    error.value = `Receipt #${receiptId} 查询失败：${toMessage(err)}`;
  }
}

function retryReceiptPolling() {
  if (activeReceiptId.value === null) return;
  stopReceiptPolling();
  receiptPollingStopped.value = false;
  error.value = null;
  void refreshReceipt(activeReceiptId.value);
}

async function run(kind: "review" | "check") {
  if (!activeProject.value || !selectedPr.value) return;
  busy.value = true;
  error.value = null;
  stopReceiptPolling();
  receipt.value = null;
  activeReceiptId.value = null;
  receiptPollingStopped.value = false;
  try {
    const requestId = requestIds.forOperation(
      activeProject.value.id,
      selectedPr.value.number,
      kind,
    );
    const accepted = await requestRemoteReview(
      activeProject.value.id,
      selectedPr.value.number,
      kind,
      requestId,
    );
    requestIds.accepted();
    activeReceiptId.value = accepted.receiptId;
    await refreshReceipt(accepted.receiptId);
  } catch (err) {
    error.value = toMessage(err);
  } finally {
    busy.value = false;
  }
}

async function stop(session: ReviewSession) {
  busy.value = true;
  error.value = null;
  try {
    await stopRemoteReview(session.threadId);
    sessions.value = await listRemoteReviewSessions();
  } catch (err) {
    error.value = toMessage(err);
  } finally {
    busy.value = false;
  }
}

onMounted(async () => {
  try {
    const reviewUnlisten = await onRemoteReviewEvent(applyReviewEvent, {
      onClosed: setError,
    });
    unlisteners.push(reviewUnlisten);
    const prUnlisten = await onRemotePrEvent(
      (event) => {
        if (event.kind === "updated") {
          prs.value = { ...prs.value, [event.projectId]: event.prs };
        } else {
          error.value = event.message;
        }
      },
      {
        onClosed: setError,
      },
    );
    unlisteners.push(prUnlisten);
    const snap = await remoteConsoleSnapshot();
    snapshot.value = snap;
    activeProjectId.value = snap.activeProjectId || snap.projects[0]?.id || "";
    await Promise.all([
      refreshStatuses(),
      activeProjectId.value ? refreshProject(activeProjectId.value) : Promise.resolve(),
    ]);
    await hydrateFocusedHistory();
  } catch (err) {
    error.value = toMessage(err);
    for (const unlisten of unlisteners.splice(0)) unlisten();
  } finally {
    loading.value = false;
  }
});

onUnmounted(() => {
  stopReceiptPolling();
  for (const unlisten of unlisteners.splice(0)) unlisten();
});
</script>

<template>
  <main class="remote-console">
    <header class="topbar">
      <div>
        <strong>prmonitor</strong>
        <small v-if="snapshot">v{{ snapshot.appVersion }}</small>
      </div>
      <div class="status">
        <span>Codex: {{ codexStatus || "unknown" }}</span>
        <span>Claude: {{ claudeStatus || "unknown" }}</span>
        <span>Cursor: {{ cursorStatus || "unknown" }}</span>
      </div>
    </header>

    <p v-if="error" class="error">{{ error }}</p>
    <p v-if="loading" class="empty">Loading remote console...</p>

    <section v-else class="layout">
      <aside class="sidebar">
        <label>
          Project
          <select :value="activeProjectId" @change="selectProject(($event.target as HTMLSelectElement).value)">
            <option v-for="project in projects" :key="project.id" :value="project.id">
              {{ project.name || project.repo }}
            </option>
          </select>
        </label>
        <div class="project-meta" v-if="activeProject">
          <span>{{ activeProject.repo }}</span>
          <span>{{ activeProject.sourceKind }} / {{ activeProject.engineKind }}</span>
        </div>
        <div class="pr-list">
          <button
            v-for="pr in activePrs"
            :key="pr.number"
            type="button"
            :class="{ selected: pr.number === selectedPr?.number }"
            @click="selectPr(pr.number)"
          >
            <span>#{{ pr.number }}</span>
            <strong>{{ pr.title }}</strong>
            <small>{{ pr.kind }} · {{ pr.presence }}</small>
          </button>
        </div>
      </aside>

      <section class="content">
        <div v-if="selectedPr" class="pr-header">
          <div>
            <h1>#{{ selectedPr.number }} {{ selectedPr.title }}</h1>
            <a :href="selectedPr.url" target="_blank" rel="noreferrer">Open PR</a>
          </div>
          <div class="actions">
            <button type="button" :disabled="busy" @click="run('review')">Review</button>
            <button type="button" :disabled="busy" @click="run('check')">Check</button>
          </div>
        </div>
        <p v-else class="empty">No PRs available for this project.</p>

        <div v-if="activeReceiptId !== null" class="receipt" :data-status="receipt?.status ?? 'loading'">
          <strong>Receipt #{{ activeReceiptId }}</strong>
          <span>{{ receipt ? receiptStatusLabels[receipt.status] : "Waiting for status" }}</span>
          <a v-if="receipt?.commentUrl" :href="receipt.commentUrl" target="_blank" rel="noreferrer">Comment</a>
          <small v-if="receipt?.threadId">Thread {{ receipt.threadId }}</small>
          <small v-if="receipt?.error" class="error">{{ receipt.error }}</small>
          <button v-if="receiptPollingStopped" type="button" @click="retryReceiptPolling">
            Retry receipt lookup
          </button>
        </div>

        <div class="sessions" v-if="activeSessions.length">
          <button
            v-for="session in activeSessions"
            :key="session.threadId"
            type="button"
            :class="{ selected: session.threadId === focusedSession?.threadId }"
            @click="focusSession(session.threadId)"
          >
            <span>{{ session.kind }}</span>
            <strong>{{ session.status }}</strong>
            <small>{{ new Date(session.createdAtEpoch * 1000).toLocaleString() }}</small>
          </button>
        </div>

        <article v-if="focusedSession" class="review-pane">
          <header>
            <div>
              <strong>{{ focusedSession.kind }} session</strong>
              <small>{{ focusedSession.threadId }}</small>
            </div>
            <div class="actions">
              <a v-if="focusedSession.commentUrl" :href="focusedSession.commentUrl" target="_blank" rel="noreferrer">Comment</a>
              <button
                v-if="['starting', 'running', 'interrupting'].includes(focusedSession.status)"
                type="button"
                :disabled="busy"
                @click="stop(focusedSession)"
              >
                Stop
              </button>
            </div>
          </header>
          <ReviewStream :items="focusedItems" />
        </article>
      </section>
    </section>
  </main>
</template>

<style scoped>
.remote-console {
  min-height: 100vh;
  background: var(--color-bg);
  color: var(--color-text);
}
.topbar {
  height: 56px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
  padding: 0 var(--space-5);
  border-bottom: 1px solid var(--color-border);
}
.topbar div {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.status {
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
}
.layout {
  display: grid;
  grid-template-columns: minmax(260px, 340px) minmax(0, 1fr);
  min-height: calc(100vh - 56px);
}
.sidebar {
  border-right: 1px solid var(--color-border);
  padding: var(--space-4);
  overflow: auto;
}
label {
  display: grid;
  gap: var(--space-2);
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
}
select,
button {
  font: inherit;
}
select {
  min-height: 36px;
}
.project-meta {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  margin: var(--space-4) 0;
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
}
.pr-list,
.sessions {
  display: flex;
  flex-direction: column;
  gap: var(--space-2);
}
.receipt {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--space-3);
  margin: var(--space-4) 0;
  padding: var(--space-3);
  border: 1px solid var(--color-border);
  border-radius: 6px;
  background: var(--color-bg-subtle);
}
.receipt small {
  color: var(--color-text-muted);
}
.pr-list button,
.sessions button {
  display: grid;
  gap: var(--space-1);
  text-align: left;
  border: 1px solid var(--color-border);
  background: var(--color-bg);
  color: inherit;
  padding: var(--space-3);
  border-radius: 6px;
  cursor: pointer;
}
.pr-list button.selected,
.sessions button.selected {
  border-color: var(--color-accent);
  background: var(--color-bg-subtle);
}
.pr-list small,
.sessions small {
  color: var(--color-text-muted);
}
.content {
  padding: var(--space-5);
  overflow: auto;
}
.pr-header,
.review-pane > header {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: var(--space-4);
}
h1 {
  margin: 0 0 var(--space-2);
  font-size: 20px;
  line-height: 1.3;
}
.actions {
  display: flex;
  align-items: center;
  gap: var(--space-2);
}
.actions button,
.actions a {
  border: 1px solid var(--color-border-strong);
  background: var(--color-bg);
  color: inherit;
  border-radius: 6px;
  min-height: 34px;
  padding: 0 var(--space-3);
  display: inline-flex;
  align-items: center;
  text-decoration: none;
}
.actions button:disabled {
  opacity: 0.5;
}
.sessions {
  margin: var(--space-5) 0;
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
}
.review-pane {
  border-top: 1px solid var(--color-border);
  padding-top: var(--space-4);
}
.review-pane header small {
  display: block;
  color: var(--color-text-muted);
  margin-top: var(--space-1);
}
.error,
.empty {
  margin: var(--space-5);
  color: var(--color-text-muted);
}
.error {
  color: var(--color-danger);
}
@media (max-width: 760px) {
  .layout {
    grid-template-columns: 1fr;
  }
  .sidebar {
    border-right: 0;
    border-bottom: 1px solid var(--color-border);
    max-height: 42vh;
  }
}
</style>
