// Terminal slice → backend adapter (#1383). Wraps the iTerm daemon commands and the
// streamed `terminal:event` channel. Mirrors src/review/api.ts: every backend call goes
// through the shared `../transport` port (never `@tauri-apps/*` directly — slice-boundary
// funnel), and the wire types come from the `../types` root.
import { getTransport } from "../transport";
import type { CreateSessionOpts, TerminalEvent, TerminalSession } from "../types";
import type { TerminalDaemonStatus } from "./types";

// Mirrors `src-tauri/src/events.rs::TERMINAL_EVENT` (the open downstream end of the
// event-name funnel — keep in lockstep with the Rust emitter / its golden test).
const TERMINAL_EVENT = "terminal:event" as const;

// List the daemon's current sessions (the picker's snapshot).
export function listTerminalSessions(): Promise<TerminalSession[]> {
  return getTransport().request<TerminalSession[]>("list_terminal_sessions");
}

// Create a new iTerm session. `opts` defaults to `{}` (empty CreateSessionOpts: no
// windowId / profile) so the daemon picks a fresh window with the default profile — the
// arg is always present, matching the Rust command's `opts: CreateSessionOpts` param.
export function createTerminalSession(
  opts: CreateSessionOpts = {},
): Promise<TerminalSession> {
  return getTransport().request<TerminalSession>("create_terminal_session", { opts });
}

// Attach to a session — start receiving its `screenUpdate` frames on `terminal:event`.
export function attachTerminal(sessionId: string): Promise<void> {
  return getTransport().request<void>("attach_terminal", { sessionId });
}

// Detach — stop streaming the session (it keeps running in iTerm).
export function detachTerminal(sessionId: string): Promise<void> {
  return getTransport().request<void>("detach_terminal", { sessionId });
}

// Forward keystrokes / escape sequences to the attached session.
export function sendTerminalInput(sessionId: string, data: string): Promise<void> {
  return getTransport().request<void>("send_terminal_input", { sessionId, data });
}

// Resize the session's grid to match the xterm pane (cols × rows).
export function resizeTerminal(
  sessionId: string,
  cols: number,
  rows: number,
): Promise<void> {
  return getTransport().request<void>("resize_terminal", { sessionId, cols, rows });
}

// Probe daemon health. Returns the slice-private `TerminalDaemonStatus` mirror of the Rust
// `terminal/process.rs::TerminalDaemonStatus` (golden-locked wire shape). No StatusBar
// consumer yet, but the typed shape closes the previously-open downstream mirror funnel.
export function getTerminalStatus(): Promise<TerminalDaemonStatus> {
  return getTransport().request<TerminalDaemonStatus>("get_terminal_status");
}

// Stop the resident iTerm daemon (a manual teardown). Returns the post-stop
// `TerminalDaemonStatus` the Rust `stop_terminal_daemon` yields (`AppResult<…>`) — keep the
// generic in lockstep with the command's return type (dropping it to `void` discards the
// status, the previously-open downstream mirror gap).
export function stopTerminalDaemon(): Promise<TerminalDaemonStatus> {
  return getTransport().request<TerminalDaemonStatus>("stop_terminal_daemon");
}

// Subscribe to streamed terminal events. Returns a Promise<UnlistenFn> for cleanup.
export function onTerminalEvent(cb: (e: TerminalEvent) => void) {
  return getTransport().subscribe<TerminalEvent>(TERMINAL_EVENT, cb);
}
