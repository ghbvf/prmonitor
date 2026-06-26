// Terminal slice store (#1383). Module-level singleton (mirrors useReviewStore): every
// `useTerminalStore()` caller shares one instance, so the TerminalView picker, the
// connection banner, and the XtermPane all read/write the same refs.
//
// The xterm `Terminal` object is DELIBERATELY kept OUT of reactive state — it lives in the
// XtermPane component and bridges to the store via `registerScreenSink`. Putting a Terminal
// (a big DOM-bound object with its own internal buffers) into a Vue ref would make Vue
// deep-track it and is a known footgun. So the screen sink + last frame are NON-reactive
// module vars instead.
import { ref } from "vue";
import type { TerminalEvent, TerminalSession } from "../types";
import { assertNever } from "../types";
import {
  attachTerminal,
  createTerminalSession,
  detachTerminal,
  listTerminalSessions,
  onTerminalEvent,
  resizeTerminal,
  sendTerminalInput,
} from "./api";
import type { ScreenSink, TerminalConnStatus } from "./types";

// A rejected Tauri invoke throws the AppError object `{ message }`; fall back to a
// stringified form for any non-conforming throw (mirrors the other slice stores).
function toMessage(err: unknown): string {
  return (err as { message?: string })?.message ?? String(err);
}

// ── Reactive state ────────────────────────────────────────────────────────────────
const sessions = ref<TerminalSession[]>([]);
const activeSessionId = ref<string | null>(null);
const connection = ref<TerminalConnStatus>("idle");
const error = ref<string | null>(null);
// The `terminal:event` listener must attach before a session can be attached, or the
// session's earliest screen frames would be missed. `listenerReady` gates the UI;
// `listenerError` surfaces a failed registration.
const listenerReady = ref(false);
const listenerError = ref<string | null>(null);

// ── NON-reactive bridge to the xterm pane ───────────────────────────────────────────
// The currently-mounted pane's frame sink (null when no pane is mounted), and the last
// frame seen — replayed to a freshly-registered sink so a pane remount (or a late mount
// after the first frame already arrived) restores the screen instead of showing blank.
type ScreenFrame = { data: string; cursorRow?: number; cursorCol?: number };
let screenSink: ScreenSink | null = null;
let latestScreen: ScreenFrame | null = null;

// Register the pane's frame sink. Replays the last frame immediately so a remount restores
// the visible screen (the backend doesn't re-send a frame just because a pane re-attached).
function registerScreenSink(sink: ScreenSink) {
  screenSink = sink;
  if (latestScreen) sink(latestScreen);
}

// Drop the pane's frame sink on unmount. Keeps `latestScreen` so the NEXT pane's
// registration can replay it (that's the whole point of the replay-on-register path).
function unregisterScreenSink() {
  screenSink = null;
}

// ── Commands ────────────────────────────────────────────────────────────────────────
// Re-read the session list. Surfaces a rejected command via `error` rather than throwing.
// A reject means the daemon precondition failed (python3 missing / iTerm not running /
// iterm2 not installed) — flip connection to "error" so TerminalView's error banner (gated
// on connection==='error') actually shows the reason, mirroring attach/createAndAttach.
async function refreshSessions() {
  try {
    sessions.value = await listTerminalSessions();
  } catch (err) {
    connection.value = "error";
    error.value = toMessage(err);
    console.error("刷新 terminal 会话列表失败", err);
  }
}

// Focus + attach a session. Marks `attaching`; the terminal `attached` event flips it to
// `attached` (see applyEvent). Clears `latestScreen` so a remounting pane doesn't replay
// the PREVIOUS session's screen before the new one's first frame arrives.
async function attach(sessionId: string) {
  // Switching sessions: detach the PREVIOUSLY-focused session FIRST so the daemon tears
  // down its streamer (streamers are keyed by sessionId — leaving the old one running
  // leaks a background stream that keeps emitting frames we now drop). Best-effort: a
  // failed detach must NOT block the new attach, so it's swallowed. Skip when nothing is
  // focused or when re-attaching the same id.
  const previous = activeSessionId.value;
  if (previous !== null && previous !== sessionId) {
    try {
      await detachTerminal(previous);
    } catch (err) {
      console.error("切换会话时 detach 旧会话失败", err);
    }
  }
  activeSessionId.value = sessionId;
  connection.value = "attaching";
  error.value = null;
  latestScreen = null;
  try {
    await attachTerminal(sessionId);
  } catch (err) {
    connection.value = "error";
    error.value = toMessage(err);
    console.error("attach terminal 失败", err);
  }
}

// Create a fresh session (default window/profile) and attach to it. Surfaces it in the
// picker immediately (a later refresh reconciles) so the user sees it without waiting.
async function createAndAttach() {
  try {
    const created = await createTerminalSession();
    if (!sessions.value.some((s) => s.sessionId === created.sessionId)) {
      sessions.value = [...sessions.value, created];
    }
    await attach(created.sessionId);
  } catch (err) {
    connection.value = "error";
    error.value = toMessage(err);
    console.error("创建 terminal 会话失败", err);
  }
}

// Detach the focused session (it keeps running in iTerm). Resets to idle regardless of the
// outcome — the user asked to leave — and clears the last frame so a future attach starts blank.
async function detach() {
  const id = activeSessionId.value;
  if (!id) return;
  try {
    await detachTerminal(id);
  } catch (err) {
    error.value = toMessage(err);
    console.error("detach terminal 失败", err);
  } finally {
    activeSessionId.value = null;
    connection.value = "idle";
    latestScreen = null;
  }
}

// Forward keystrokes to the focused session. GUARD: only when a session is active AND the
// connection is attached — otherwise a keystroke during attach/close/error has no live
// session to receive it.
async function sendInput(data: string) {
  const id = activeSessionId.value;
  if (!id || connection.value !== "attached") return;
  try {
    await sendTerminalInput(id, data);
  } catch (err) {
    // Flip connection so TerminalView's error banner (gated on connection==='error') +
    // its Retry CTA surface the failure — setting only `error` would leave it invisible
    // (mirrors the attach/createAndAttach/refreshSessions pattern).
    connection.value = "error";
    error.value = toMessage(err);
    console.error("发送 terminal 输入失败", err);
  }
}

// Resize the focused session's grid to match the pane.
async function resize(cols: number, rows: number) {
  const id = activeSessionId.value;
  if (!id) return;
  try {
    await resizeTerminal(id, cols, rows);
  } catch (err) {
    // Flip connection so the error banner + Retry CTA surface the failure (mirrors the
    // attach/createAndAttach/refreshSessions pattern); `error` alone stays invisible.
    connection.value = "error";
    error.value = toMessage(err);
    console.error("resize terminal 失败", err);
  }
}

// Fold one streamed event into the store. The `default: assertNever(ev)` is the Medium
// exhaustiveness carrier for the events.rs ↔ types.ts TerminalEvent contract: a new arm
// without a case here is a COMPILE error.
function applyEvent(ev: TerminalEvent) {
  // A connection-level error carries NO sessionId (not tied to a session) — surface it
  // regardless of which session is focused, BEFORE the session-scoped drop guard below
  // would discard it (its undefined sessionId never equals activeSessionId).
  if (ev.kind === "error" && ev.sessionId === undefined) {
    connection.value = "error";
    error.value = ev.message;
    return;
  }
  // Session-scoped from here: drop anything not for the focused session (a background
  // session's frames must not bleed into the focused pane).
  if (ev.sessionId !== activeSessionId.value) return;

  switch (ev.kind) {
    case "attached":
      connection.value = "attached";
      return;
    case "screenUpdate":
      // Cache the full frame (for replay-on-register) THEN push it to the live pane.
      latestScreen = {
        data: ev.contents,
        cursorRow: ev.cursorRow,
        cursorCol: ev.cursorCol,
      };
      screenSink?.(latestScreen);
      return;
    case "sessionEnded":
      // The session died. Keep connection="closed" so TerminalView shows the "ended"
      // banner, but DROP the dead session: clearing activeSessionId unmounts the now-frozen
      // XtermPane + MobileToolbar (TerminalView gates the pane on activeSessionId !== null,
      // and the closed banner is gated on connection==='closed', not on activeSessionId),
      // and clearing latestScreen stops a future pane replaying the dead screen. Refresh the
      // list so the picker no longer offers / highlights the gone session.
      connection.value = "closed";
      activeSessionId.value = null;
      latestScreen = null;
      void refreshSessions();
      return;
    case "error":
      connection.value = "error";
      error.value = ev.message;
      return;
    default:
      return assertNever(ev);
  }
}

// Attach the streamed-event listener, THEN read the session snapshot (subscribe-before-
// snapshot, so no frame arriving in between is lost). Returns a Promise<UnlistenFn> for
// cleanup. On a registration failure, surface it and skip the snapshot (a noop unlisten).
async function init() {
  let unlisten: Awaited<ReturnType<typeof onTerminalEvent>>;
  try {
    unlisten = await onTerminalEvent(applyEvent);
    listenerReady.value = true;
  } catch (err) {
    listenerError.value = toMessage(err);
    console.error("terminal 事件监听注册失败", err);
    return () => {};
  }
  await refreshSessions();
  return unlisten;
}

export function useTerminalStore() {
  return {
    sessions,
    activeSessionId,
    connection,
    error,
    listenerReady,
    listenerError,
    registerScreenSink,
    unregisterScreenSink,
    refreshSessions,
    attach,
    createAndAttach,
    detach,
    sendInput,
    resize,
    applyEvent,
    init,
  };
}
