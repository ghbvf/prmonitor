<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import ReviewStream from "./review/ReviewStream.vue";
import type { ReviewEvent, TrackedPrView } from "./types";
import type { ReviewSession, StreamItem } from "./review/types";
import type { UnlistenFn } from "./transport";
import {
  getRemoteClaudeStatus,
  getRemoteCodexStatus,
  getRemotePrs,
  getRemotePrSessions,
  getRemoteSessionHistory,
  listRemoteReviewSessions,
  onRemotePrEvent,
  onRemoteReviewEvent,
  remoteConsoleSnapshot,
  startRemoteReview,
  stopRemoteReview,
  type RemoteConsoleSnapshot,
} from "./remoteConsole/api";

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
const unlisteners: UnlistenFn[] = [];

const projects = computed(() => snapshot.value?.projects ?? []);
const activeProject = computed(
  () => projects.value.find((p) => p.id === activeProjectId.value) ?? projects.value[0] ?? null,
);
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
  const [codex, claude] = await Promise.allSettled([
    getRemoteCodexStatus(),
    getRemoteClaudeStatus(),
  ]);
  codexStatus.value =
    codex.status === "fulfilled" ? codex.value.message : toMessage(codex.reason);
  claudeStatus.value =
    claude.status === "fulfilled" ? claude.value.message : toMessage(claude.reason);
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
  const liveById = new Map(sessions.value.map((s) => [s.threadId, s]));
  for (const session of durable) liveById.set(session.threadId, session);
  sessions.value = Array.from(liveById.values());
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

async function run(kind: "review" | "check") {
  if (!activeProject.value || !selectedPr.value) return;
  busy.value = true;
  error.value = null;
  try {
    const threadId = await startRemoteReview(activeProject.value.id, selectedPr.value.number, kind);
    focusedThreadId.value = threadId;
    sessions.value = await listRemoteReviewSessions();
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
