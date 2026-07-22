// Review slice store. Aggregates the streamed `review:event` units into ordered
// `items` (deltas concatenated per codex `itemId`), tracks the active session and
// its lifecycle, and drives start/stop. `codex` tracks the codex CLI availability
// surfaced in the StatusBar.
//
// Module-level singleton state so every `useReviewStore()` caller shares one
// instance (mirrors the cross-component sharing Pinia gives the pr store): the
// ReviewPanel renders the stream and the App-shell StatusBar reads `codex` off the
// same refs. A fresh ref per call would silo those consumers.
import { ref } from "vue";
import type { ReviewEvent } from "../types";
import {
  getClaudeStatus,
  getCodexStatus,
  getCursorStatus,
  getSessionHistory,
  listReviewSessions,
  onReviewEvent,
  sendReviewMessage,
  startCodex,
  startCursor,
  startReview,
  stopCodex,
  stopCursor,
  stopReview,
} from "./api";
import { useProjects } from "../projects";
import type {
  ClaudeStatus,
  CodexStatus,
  CursorStatus,
  ReviewSession,
  SessionStatus,
  StreamItem,
} from "./types";

// A rejected Tauri invoke throws the AppError object `{ message }`; fall back to
// a stringified form for any non-conforming throw.
function toMessage(err: unknown): string {
  return (err as { message?: string })?.message ?? String(err);
}

const codex = ref<CodexStatus | null>(null);
const claude = ref<ClaudeStatus | null>(null);
const cursor = ref<CursorStatus | null>(null);

// All review sessions the backend currently tracks (#8 auto-trigger can run
// several concurrently). Drives the ReviewSessions list; refreshed event-driven
// (no polling timer — see `refreshSessions` call sites). Keyed/sorted by
// `threadId` for a stable render order.
const sessions = ref<ReviewSession[]>([]);

// App-level auto-trigger (#8) notice, session-less: set from a `dispatchError`
// event (the backend dispatcher hit a bad config, one/more start failures, or a
// ledger-write failure). Keyed per project (#35): the dispatcher runs per
// monitored project, so a bad-config / start-failure notice belongs to the project
// it fired for — App.vue reads `dispatchError[activeProjectId]`. Surfaced in the
// availability banner; dismissed per-project via `clearDispatchError`. Not tied to
// any session, so it survives panel focus changes.
const dispatchError = ref<Record<string, string | null>>({});

// Active session (single-active-panel model): the most recently started review.
const activeThreadId = ref<string | null>(null);
const activePr = ref<number | null>(null);
// `running` between start and the terminal `turnCompleted`; `finalStatus` is the
// codex turn status once it ends ("completed" / "interrupted" / "failed").
const running = ref(false);
const finalStatus = ref<string | null>(null);
const error = ref<string | null>(null);
const items = ref<StreamItem[]>([]);
// The `review:event` listener must be attached before a review can be started,
// or the active session's earliest deltas (and even its terminal event) would be
// missed. `listenerReady` gates the start button; `listenerError` surfaces a
// failed registration.
const listenerReady = ref(false);
const listenerError = ref<string | null>(null);

// While a start is in flight the session id is unknown, so an incoming event
// can't yet be attributed. Buffer events for that window and, once `startReview`
// resolves, replay only our own (a concurrent session's events must not pollute
// this panel). `null` means "not currently buffering".
let inFlightBuffer: ReviewEvent[] | null = null;

// Monotonic counter for optimistic user-bubble ids. The id is sent verbatim to
// `send_review_message` so the backend persists the user turn under the SAME id —
// reopen-history dedup (focus() merges history + live by itemId) then collapses the
// optimistic bubble and its persisted twin into one. Counter + epoch keeps it
// unique within and across sessions without a uuid dep.
let userItemCounter = 0;

// Hydrate codex availability. Tolerates a rejected command by surfacing an
// unavailable status rather than throwing.
async function refreshCodexStatus() {
  try {
    codex.value = await getCodexStatus();
  } catch (err) {
    const message = toMessage(err);
    console.error("codex 状态获取失败", err);
    // A failed probe doesn't mean the user stopped it: keep desiredRunning true.
    codex.value = {
      available: false,
      desiredRunning: true,
      message: message || "codex 状态获取失败",
    };
  }
}

// Hydrate claude availability. Tolerates a rejected command by surfacing an
// unavailable status rather than throwing (mirrors refreshCodexStatus).
async function refreshClaudeStatus() {
  try {
    claude.value = await getClaudeStatus();
  } catch (err) {
    const message = toMessage(err);
    console.error("claude 状态获取失败", err);
    claude.value = {
      available: false,
      message: message || "claude 状态获取失败",
    };
  }
}

// Hydrate cursor ACP availability. Tolerates a rejected command by surfacing an
// unavailable status rather than throwing (mirrors refreshCodexStatus).
async function refreshCursorStatus() {
  try {
    cursor.value = await getCursorStatus();
  } catch (err) {
    const message = toMessage(err);
    console.error("cursor 状态获取失败", err);
    // A failed probe doesn't mean the user stopped it: keep desiredRunning true.
    cursor.value = {
      available: false,
      desiredRunning: true,
      message: message || "cursor 状态获取失败",
    };
  }
}

// Explicitly (re)start the resident codex app-server, reflecting the result in
// `codex` (drives the StatusBar dot + 启动/停止 button). Tolerates a rejected
// command by surfacing an intent-to-run failure status.
async function startCodexServer() {
  try {
    codex.value = await startCodex();
  } catch (err) {
    codex.value = {
      available: false,
      desiredRunning: true,
      message: toMessage(err) || "codex 启动失败",
    };
  }
}

// Explicitly stop the resident codex app-server. On failure still mark it stopped
// (the user's intent) so the StatusBar offers a 启动 action to retry.
async function stopCodexServer() {
  try {
    codex.value = await stopCodex();
  } catch (err) {
    codex.value = {
      available: false,
      desiredRunning: false,
      message: toMessage(err) || "codex 停止失败",
    };
  }
}

// Explicitly (re)start the resident Cursor ACP server, reflecting the result in
// `cursor` (drives the StatusBar dot + 启动/停止 button). Tolerates a rejected
// command by surfacing an intent-to-run failure status.
async function startCursorServer() {
  try {
    cursor.value = await startCursor();
  } catch (err) {
    cursor.value = {
      available: false,
      desiredRunning: true,
      message: toMessage(err) || "cursor 启动失败",
    };
  }
}

// Explicitly stop the resident Cursor ACP server. On failure still mark it stopped
// (the user's intent) so the StatusBar offers a 启动 action to retry.
async function stopCursorServer() {
  try {
    cursor.value = await stopCursor();
  } catch (err) {
    cursor.value = {
      available: false,
      desiredRunning: false,
      message: toMessage(err) || "cursor 停止失败",
    };
  }
}

// Coalesce overlapping refreshes. When N sessions start at once, `applyEvent`
// fires a refresh per unseen threadId / terminal event; without coalescing those
// race N `listReviewSessions` calls whose out-of-order responses could clobber
// newer state. We run at most one refresh at a time and collapse any requests
// arriving mid-flight into a single trailing run, so the last response always
// reflects a list read taken after the latest request.
let refreshInFlight = false;
let refreshQueued = false;

// Refresh the concurrent-session list from the backend. Tolerates a rejected
// command by logging and keeping the prior value (a transient failure shouldn't
// blank the list). Sorted by `threadId` for a stable render order.
async function refreshSessions() {
  // Already running: mark a single trailing refresh and let the active call run it.
  if (refreshInFlight) {
    refreshQueued = true;
    return;
  }
  refreshInFlight = true;
  try {
    do {
      refreshQueued = false;
      const next = await listReviewSessions();
      sessions.value = [...next].sort((a, b) =>
        a.threadId.localeCompare(b.threadId),
      );
    } while (refreshQueued); // a request arrived mid-flight → one more pass.
  } catch (err) {
    console.error("刷新 review 会话列表失败", err);
  } finally {
    refreshInFlight = false;
  }
}

// Dismiss the auto-trigger notice for a project (the banner's ✕). The next
// `dispatchError` event for that project re-sets it.
function clearDispatchError(projectId: string) {
  dispatchError.value[projectId] = null;
}

// Append a streamed delta, concatenating onto the existing item for `itemId`
// (each `itemId` carries exactly one kind) or starting a new one.
function appendDelta(kind: StreamItem["kind"], itemId: string, text: string) {
  const existing = items.value.find((i) => i.itemId === itemId);
  if (existing) existing.text += text;
  else items.value.push({ itemId, kind, text });
}

// Fold one streamed event into the store. Pure w.r.t. the module state; the
// `never` default makes a new `ReviewEvent` variant a compile error (the
// downstream exhaustiveness guard for the events.rs ↔ types.ts contract).
function applyEvent(ev: ReviewEvent) {
  // App-level dispatch notice (#8 auto-trigger): session-less, no threadId — handle
  // FIRST, before the threadId-keyed bookkeeping / attribution below would drop it.
  // The early return narrows `ev` to the session-scoped variants, so the `never`
  // exhaustiveness guard in the switch still covers the remaining four.
  if (ev.kind === "dispatchError") {
    dispatchError.value[ev.projectId] = ev.message;
    return;
  }

  // Sessions-list bookkeeping runs for EVERY event, ahead of the focused-panel
  // attribution below — the concurrent list tracks ALL sessions, not just the
  // focused one, so it must refresh even for events the panel filter drops.
  // Refresh when: a threadId we haven't listed yet appears (a session — likely
  // #8 auto-started — just started, surface it), or a terminal event fires (a
  // session left the running set, flip its badge to done/failed). Fire-and-forget
  // so `applyEvent` stays sync (the `void` keeps the in-flight buffer replay
  // simple and the `never` exhaustiveness guard below meaningful).
  const unseen = !sessions.value.some((s) => s.threadId === ev.threadId);
  if (unseen || ev.kind === "turnCompleted" || ev.kind === "error") {
    void refreshSessions();
  }

  // Start in flight (id not yet known): buffer rather than guess attribution —
  // replayed (our id only) once `startReview` resolves, so a concurrent session's
  // events can't pollute this panel.
  if (inFlightBuffer && activeThreadId.value === null) {
    inFlightBuffer.push(ev);
    return;
  }
  // Single focused panel: the stream renders ONLY the focused session. Drop the
  // event when nothing is focused (else auto-started sessions, which the user never
  // selected, would pile their deltas into the panel) or when it belongs to another
  // session. The manual-start in-flight window (id not yet known) was already
  // handled by the buffer block above, so reaching here with no focus means an
  // unfocused background session — not ours.
  if (!activeThreadId.value || ev.threadId !== activeThreadId.value) return;

  switch (ev.kind) {
    case "messageDelta":
      appendDelta("message", ev.itemId, ev.text);
      break;
    case "reasoningDelta":
      appendDelta("reasoning", ev.itemId, ev.text);
      break;
    case "turnCompleted":
      running.value = false;
      finalStatus.value = ev.status;
      break;
    case "error":
      error.value = ev.message;
      running.value = false;
      break;
    default: {
      const _exhaustive: never = ev;
      void _exhaustive;
    }
  }
}

// Start a review for a PR in the given project (#35). Resets the panel, then
// records the returned session id.
async function start(projectId: string, prNumber: number, extraArgs: string = "") {
  // Single-active MVP: one start at a time. A non-null buffer means a start is
  // already in flight; bail so two overlapping starts can't race the shared buffer.
  if (inFlightBuffer !== null) return;
  items.value = [];
  error.value = null;
  finalStatus.value = null;
  activeThreadId.value = null;
  activePr.value = prNumber;
  running.value = true;
  inFlightBuffer = []; // buffer events until the id is known (see applyEvent).
  try {
    const id = await startReview(projectId, prNumber, extraArgs);
    activeThreadId.value = id;
    // Replay what arrived during the start; applyEvent now drops foreign sessions.
    const buffered = inFlightBuffer;
    inFlightBuffer = null;
    for (const ev of buffered) applyEvent(ev);
  } catch (err) {
    inFlightBuffer = null;
    running.value = false;
    error.value = toMessage(err);
    console.error("启动 review 失败", err);
  } finally {
    // Explicit start resumes a stopped Cursor ACP server (success or fail after
    // resume); refresh so App/StatusBar banners flip off 「已停止」 without waiting
    // for a manual re-probe — including the failure path where start never returns.
    const project = useProjects().projects.value.find((p) => p.id === projectId);
    if (project?.engineKind === "cursor") {
      void refreshCursorStatus();
    }
  }
}

// Interrupt the active session. On success `running` stays true until the
// terminal `turnCompleted` (status `interrupted`) arrives on the stream; on
// failure we clear `running` so the panel doesn't get stuck (the interrupt never
// took, so no terminal event is coming).
async function stop() {
  const id = activeThreadId.value;
  if (!id) return;
  try {
    await stopReview(id);
  } catch (err) {
    running.value = false;
    error.value = toMessage(err);
    console.error("停止 review 失败", err);
  }
}

// Send a follow-up chat message into the focused session (#chat). The AI reply
// streams back on the SAME thread, so `applyEvent` folds its `messageDelta` /
// `reasoningDelta` into `items` exactly like the initial review — no `inFlightBuffer`
// is needed because `activeThreadId` is already known, so attribution is unambiguous.
async function sendMessage(projectId: string, threadId: string, message: string) {
  // Freeze guard: no target, a turn already running, the listener not yet attached
  // (a reply could be lost), or a focus switch made `threadId` stale. Mirrors the
  // composer's `canChat` gate so a racing call can't double-send into a busy thread.
  if (
    !threadId ||
    running.value ||
    !listenerReady.value ||
    activeThreadId.value !== threadId
  ) {
    return;
  }
  // Unique optimistic id, reused as the wire `userItemId` so the persisted user turn
  // and this bubble dedup on reopen (see `userItemCounter`).
  const userItemId = `user:${++userItemCounter}:${Date.now()}`;
  // Optimistically render what the user sent (kept even on a send failure below so
  // they still see their message). The reply will append after it.
  items.value.push({ itemId: userItemId, kind: "user", text: message });
  running.value = true;
  finalStatus.value = null;
  error.value = null;
  try {
    await sendReviewMessage(projectId, threadId, message, userItemId);
  } catch (err) {
    console.error("发送对话消息失败", err);
    // Guard against a focus switch during the await: only mutate this session's state
    // if it's still the focused one (mirrors focus()/hydrateActiveSession). Otherwise
    // clearing `running`/setting `error` would clobber the NEW session's state.
    if (activeThreadId.value === threadId) {
      // Keep the user bubble (they should see what they tried to send); clear the
      // running freeze, mark the turn failed so the status line reads "已结束 / 失败"
      // (not the stale "未开始"), and surface the error so the composer unlocks.
      running.value = false;
      finalStatus.value = "failed";
      error.value = toMessage(err);
    }
  }
}

// Reattach to a still-active backend review session (e.g. after the panel
// remounts, or the app restarts, while codex streams on) so the UI doesn't show
// "not started" over a live turn. Past deltas aren't replayed — the backend
// doesn't buffer them — so `items` starts empty and new deltas append from here.
// MVP is single-active, so the first active session wins.
async function hydrateActiveSession() {
  try {
    // Sort by `threadId` (same order as `refreshSessions`) before picking the
    // first active one: the backend list is a HashMap snapshot with nondeterministic
    // order, so without this the reattached session could differ across restarts.
    const sessions = [...(await listReviewSessions())].sort((a, b) =>
      a.threadId.localeCompare(b.threadId),
    );
    // Reattach only within the active project (#35): focusing a session from a
    // non-active project would mismatch the PR # shown in the (active-project) PrList.
    const activeProjectId = useProjects().activeProjectId.value;
    const active = sessions.find(
      (s) =>
        s.projectId === activeProjectId &&
        (s.status === "running" ||
          s.status === "starting" ||
          s.status === "interrupting"),
    );
    if (active) {
      activeThreadId.value = active.threadId;
      activePr.value = active.prNumber;
      running.value = true; // non-terminal → still streaming; stop stays enabled.
      finalStatus.value = null;
      error.value = null;
    }
  } catch (err) {
    console.error("恢复 review 会话失败", err);
  }
}

// Point the single focused stream at a chosen session (from the ReviewSessions
// list) and HYDRATE it with the session's persisted history (#70 / #67): opening a
// past session now shows the content produced before it was opened, not just new
// deltas. `running` mirrors the picked session's non-terminal status so the stop
// button + waiting state behave like a fresh start would. Async (history load), but
// safe to call fire-and-forget from a click handler.
async function focus(
  projectId: string,
  threadId: string,
  prNumber: number,
  status: SessionStatus,
) {
  activeThreadId.value = threadId;
  activePr.value = prNumber;
  const active =
    status === "running" || status === "starting" || status === "interrupting";
  running.value = active;
  items.value = []; // clear for instant feedback; history fills in below.
  error.value = null;
  // A terminal session must render as ended, not "未开始": map its lifecycle status
  // to a turn-status string ReviewPanel can label (`done` collapses completed /
  // interrupted — that distinction isn't kept in SessionInfo). An active session
  // keeps `finalStatus` null (it shows "运行中").
  finalStatus.value = active ? null : status === "failed" ? "failed" : "completed";
  // Hydrate from the durable history. A still-running session keeps streaming during
  // the await, and `applyEvent` appends those live deltas onto `items` — so MERGE rather
  // than overwrite: seed the stored prefix, then keep any live item NOT already in the
  // history (a new item that began mid-load). Overwriting would drop those live deltas
  // for the user (review F1). For an item id present in both, the persisted history wins
  // (it holds the full accumulated text; the live tail re-appends on the next delta).
  try {
    const history = await getSessionHistory(projectId, prNumber, threadId);
    // Guard against a focus switch during the await: only apply if still focused here.
    if (activeThreadId.value !== threadId) return;
    const merged: StreamItem[] = history.map((h) => ({
      itemId: h.itemId,
      kind: h.kind,
      text: h.text,
    }));
    const seen = new Set(merged.map((i) => i.itemId));
    for (const live of items.value) {
      if (!seen.has(live.itemId)) merged.push(live);
    }
    items.value = merged;
  } catch (err) {
    console.error("加载历史会话内容失败", err);
    // Surface to the user (pr-review F4): a silent console error left the panel blank with
    // no sign the history failed to load. Only if still focused on this session (a focus
    // switch during the await already handed the panel to another session).
    if (activeThreadId.value === threadId) {
      error.value = (err as { message?: string })?.message ?? String(err);
    }
  }
}

// Drop the focused review (#35 F5). The focused stream is a GLOBAL single-active
// singleton, so switching the active project must clear it — otherwise ReviewPanel
// keeps rendering (and its 停止 button keeps stopping) a session belonging to the
// project the user just left, whose PR # no longer matches the visible PrList. The
// session itself keeps running in the backend and stays in the (project-filtered)
// ReviewSessions list; the user re-focuses it by clicking there. Clears only the
// focus refs — `sessions`/`dispatchError` are project-scoped elsewhere.
function clearFocus() {
  activeThreadId.value = null;
  activePr.value = null;
  running.value = false;
  finalStatus.value = null;
  error.value = null;
  items.value = [];
}

// Attach the streamed-event listener, then reattach to any live session. Returns
// a Promise<UnlistenFn> for cleanup. Marks `listenerReady` once attached (gates
// the start button) so a review can't begin before events can be received.
async function init() {
  let unlisten: Awaited<ReturnType<typeof onReviewEvent>>;
  try {
    // Attach BEFORE hydrating so no event arriving in between is lost.
    unlisten = await onReviewEvent(applyEvent);
    listenerReady.value = true;
  } catch (err) {
    listenerError.value = toMessage(err);
    console.error("review 事件监听注册失败", err);
    return () => {};
  }
  await hydrateActiveSession();
  // Seed the concurrent-session list now that the listener is live; subsequent
  // refreshes are event-driven from applyEvent (no polling timer).
  await refreshSessions();
  return unlisten;
}

export function useReviewStore() {
  return {
    codex,
    refreshCodexStatus,
    claude,
    refreshClaudeStatus,
    cursor,
    refreshCursorStatus,
    startCodexServer,
    stopCodexServer,
    startCursorServer,
    stopCursorServer,
    sessions,
    refreshSessions,
    dispatchError,
    clearDispatchError,
    focus,
    clearFocus,
    activeThreadId,
    activePr,
    running,
    finalStatus,
    error,
    items,
    listenerReady,
    listenerError,
    applyEvent,
    start,
    stop,
    sendMessage,
    init,
  };
}
