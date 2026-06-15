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
  getCodexStatus,
  listReviewSessions,
  onReviewEvent,
  startReview,
  stopReview,
} from "./api";
import type { CodexStatus, StreamItem } from "./types";

// A rejected Tauri invoke throws the AppError object `{ message }`; fall back to
// a stringified form for any non-conforming throw.
function toMessage(err: unknown): string {
  return (err as { message?: string })?.message ?? String(err);
}

const codex = ref<CodexStatus | null>(null);

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

// Hydrate codex availability. Tolerates a rejected command by surfacing an
// unavailable status rather than throwing.
async function refreshCodexStatus() {
  try {
    codex.value = await getCodexStatus();
  } catch (err) {
    const message = toMessage(err);
    console.error("codex 状态获取失败", err);
    codex.value = { available: false, message: message || "codex 状态获取失败" };
  }
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
  // Start in flight (id not yet known): buffer rather than guess attribution —
  // replayed (our id only) once `startReview` resolves, so a concurrent session's
  // events can't pollute this panel.
  if (inFlightBuffer && activeThreadId.value === null) {
    inFlightBuffer.push(ev);
    return;
  }
  // Single active panel: once our session id is known, ignore other sessions' events.
  if (activeThreadId.value && ev.threadId !== activeThreadId.value) return;

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

// Start a review for a PR. Resets the panel, then records the returned session id.
async function start(prNumber: number, kind: string) {
  items.value = [];
  error.value = null;
  finalStatus.value = null;
  activeThreadId.value = null;
  activePr.value = prNumber;
  running.value = true;
  inFlightBuffer = []; // buffer events until the id is known (see applyEvent).
  try {
    const id = await startReview(prNumber, kind);
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

// Reattach to a still-active backend review session (e.g. after the panel
// remounts, or the app restarts, while codex streams on) so the UI doesn't show
// "not started" over a live turn. Past deltas aren't replayed — the backend
// doesn't buffer them — so `items` starts empty and new deltas append from here.
// MVP is single-active, so the first active session wins.
async function hydrateActiveSession() {
  try {
    const sessions = await listReviewSessions();
    const active = sessions.find(
      (s) =>
        s.status === "running" ||
        s.status === "starting" ||
        s.status === "interrupting",
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
  return unlisten;
}

export function useReviewStore() {
  return {
    codex,
    refreshCodexStatus,
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
    init,
  };
}
