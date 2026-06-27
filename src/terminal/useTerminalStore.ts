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
import type { TerminalBackend, TerminalEvent, TerminalSession } from "../types";
import { assertNever } from "../types";
import {
  attachTerminal,
  closeTerminalSession,
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
// Re-entry guard for `stopSession` (PROD-5a): true while a `close_terminal_session` is in flight.
// Gates the Stop button (`:disabled`) so a fast double-click can't fire a second close that would
// hit the already-deregistered session and clobber the normal `"closed"` state with an error.
const stopping = ref(false);

// ── NON-reactive bridge to the xterm pane ───────────────────────────────────────────
// The currently-mounted pane's sink (null when no pane is mounted), and the backend-specific
// replay state — replayed to a freshly-registered sink so a pane remount (or a late mount after
// output already arrived) restores the screen instead of showing blank. At most ONE of the two
// replay states is populated per session (iTerm uses `latestScreen` frames; webPty uses the raw
// `rawReplay` ring) — the backend picks the render model.
type ScreenFrame = { data: string; cursorRow?: number; cursorCol?: number };
let screenSink: ScreenSink | null = null;
let latestScreen: ScreenFrame | null = null;

// Raw-PTY replay ring (#1372). A webPty backend streams incremental raw bytes (`output` events)
// instead of full snapshots, so there's no single "last frame" to replay — keep a BOUNDED ring of
// the recent raw chunks and replay them in order to a freshly-registered sink. Bounded (~256 KiB)
// and NON-reactive, same rationale as `latestScreen`: a large byte buffer in a Vue ref would be
// deep-tracked. Always retains at least the most-recent chunk (even one larger than the cap).
// mirrors SCROLLBACK_CAP in src-tauri/src/terminal/webpty_manager.rs (the backend's scrollback ring).
// Exported so the bounded-ring test asserts against the real cap (single source of truth).
export const RAW_REPLAY_MAX_BYTES = 256 * 1024;
let rawReplay: Uint8Array[] = [];
let rawReplayBytes = 0;

function pushRawReplay(bytes: Uint8Array) {
  rawReplay.push(bytes);
  rawReplayBytes += bytes.byteLength;
  while (rawReplayBytes > RAW_REPLAY_MAX_BYTES && rawReplay.length > 1) {
    const dropped = rawReplay.shift();
    if (dropped) rawReplayBytes -= dropped.byteLength;
  }
}

function clearRawReplay() {
  rawReplay = [];
  rawReplayBytes = 0;
}

// Decode a base64 `output` payload to raw bytes. `atob` yields a binary string (one char per
// byte); copy each char code into a Uint8Array. We pass BYTES straight to xterm (never a
// per-chunk TextDecoder, which would corrupt a multi-byte UTF-8 sequence split across two
// `output` chunks — xterm's own decoder stitches them). Throws on malformed base64; the caller
// guards.
function decodeBase64(b64: string): Uint8Array {
  const binary = atob(b64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

// Register the pane's sink. Replays immediately so a remount restores the visible screen (the
// backend doesn't re-send just because a pane re-attached): an iTerm `latestScreen` via
// `writeFrame`, OR the buffered webPty `rawReplay` chunks via `writeRaw` — at most one is set.
function registerScreenSink(sink: ScreenSink) {
  screenSink = sink;
  if (latestScreen) {
    sink.writeFrame(latestScreen);
  } else {
    for (const chunk of rawReplay) sink.writeRaw(chunk);
  }
}

// Drop the pane's sink on unmount. Keeps `latestScreen` / `rawReplay` so the NEXT pane's
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
  clearRawReplay();
  try {
    await attachTerminal(sessionId);
  } catch (err) {
    connection.value = "error";
    error.value = toMessage(err);
    console.error("attach terminal 失败", err);
  }
}

// Create a fresh session and attach to it. `backend` (#1372) selects the backend (iterm vs
// webPty); omitted ⇒ empty opts, so the daemon applies its default (iterm). Surfaces the new
// session in the picker immediately (a later refresh reconciles) so the user sees it without waiting.
async function createAndAttach(backend?: TerminalBackend) {
  try {
    const created = await createTerminalSession(backend ? { backend } : {});
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

// Stop the focused session's PROCESS (#1372) — terminates it (used for a webPty shell; an iTerm
// session can't be closed this way). Fires `close_terminal_session`; teardown rides on the
// backend's subsequent `sessionEnded` event (the `sessionEnded` arm already does the full
// teardown), so this does NOT touch local state on success. No-op with no active session.
//
// PROD-5a (re-entry + race): `stopping` drops a second call while one is in flight (a fast
// double-click would otherwise fire a second close that hits the already-deregistered session).
// And on reject we only flip connection="error" if a normal close did NOT already win the race
// (sessionEnded → connection="closed"): clobbering "closed" with a confusing error banner is the
// exact bug we're avoiding. A genuine failure still surfaces (banner + Retry), mirroring send/resize.
async function stopSession() {
  const id = activeSessionId.value;
  if (!id || stopping.value) return;
  stopping.value = true;
  try {
    await closeTerminalSession(id);
  } catch (err) {
    if (connection.value === "closed") return; // lost the race to a normal close — leave it closed
    connection.value = "error";
    error.value = toMessage(err);
    console.error("停止 terminal 进程失败", err);
  } finally {
    stopping.value = false;
  }
}

// Detach the focused session (it keeps running in iTerm). Resets to idle regardless of the
// outcome — the user asked to leave — and clears the last frame so a future attach starts blank.
async function detach(options: { keepalive?: boolean } = {}) {
  const id = activeSessionId.value;
  if (!id) return;
  try {
    if (options.keepalive) {
      await detachTerminal(id, { keepalive: true });
    } else {
      await detachTerminal(id);
    }
  } catch (err) {
    error.value = toMessage(err);
    console.error("detach terminal 失败", err);
  } finally {
    activeSessionId.value = null;
    connection.value = "idle";
    latestScreen = null;
    clearRawReplay();
  }
}

function resetListener() {
  listenerReady.value = false;
  listenerError.value = null;
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
  // `sessionEnded` is a session-LIFECYCLE event, not a pane-private frame: a BACKGROUND session
  // ending must STILL be dropped from the picker (F6 — otherwise the active-session guard below
  // discards it and the ended shell lingers in the list). So handle it BEFORE that guard, always
  // refreshing the list; the active-pane teardown runs only when the ENDED session is the focused one.
  if (ev.kind === "sessionEnded") {
    if (ev.sessionId === activeSessionId.value) {
      // The FOCUSED session died: keep connection="closed" so TerminalView shows the "ended"
      // banner, but DROP the dead session — clearing activeSessionId unmounts the now-frozen
      // XtermPane + MobileToolbar (TerminalView gates the pane on activeSessionId !== null, and the
      // closed banner is gated on connection==='closed'), and clearing the replay state stops a
      // future pane replaying the dead screen.
      connection.value = "closed";
      activeSessionId.value = null;
      latestScreen = null;
      clearRawReplay();
    }
    // Always re-list so the picker no longer offers / highlights the gone session (active OR
    // background); a background end leaves the focused pane's state untouched.
    void refreshSessions();
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
      // iTerm full snapshot: cache the frame (for replay-on-register) THEN push it to the live pane.
      latestScreen = {
        data: ev.contents,
        cursorRow: ev.cursorRow,
        cursorCol: ev.cursorCol,
      };
      screenSink?.writeFrame(latestScreen);
      return;
    case "output": {
      // webPty raw incremental bytes (base64). Decode → append to the bounded replay ring → push
      // to the live pane. A malformed base64 payload (atob throws) is dropped + logged rather than
      // crashing the event fold (a single bad chunk must not wedge the whole stream).
      let bytes: Uint8Array;
      try {
        bytes = decodeBase64(ev.data);
      } catch (err) {
        console.error("terminal output base64 解码失败（已丢弃该帧）", err);
        return;
      }
      pushRawReplay(bytes);
      screenSink?.writeRaw(bytes);
      return;
    }
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
    resetListener();
    unlisten = await onTerminalEvent(applyEvent, {
      onClosed: (message) => {
        listenerReady.value = false;
        listenerError.value = message;
      },
    });
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
    stopping,
    registerScreenSink,
    unregisterScreenSink,
    refreshSessions,
    attach,
    createAndAttach,
    stopSession,
    detach,
    resetListener,
    sendInput,
    resize,
    applyEvent,
    init,
  };
}
