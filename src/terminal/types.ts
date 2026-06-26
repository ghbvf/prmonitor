// Terminal slice-PRIVATE types (#1383) — NOT a wire contract (the cross-slice wire
// shapes TerminalSession / CreateSessionOpts / TerminalEvent live in `../types`). These
// model UI-only state the backend never sees, so they stay inside the slice.

// The xterm pane's connection lifecycle, driven by the store from attach()/applyEvent():
// idle (nothing focused) → attaching (attach in flight) → attached (the `attached` event
// arrived) → closed (sessionEnded) / error (attach rejected or an `error` event).
//
// Single-sourced as an `as const` array (mirrors SOURCE_KINDS / INBOX_STATUSES in
// ../types): the type is DERIVED from the array, so the literal set is Hard — a status
// outside this list is un-expressible.
export const TERMINAL_CONN_STATUSES = [
  "idle",
  "attaching",
  "attached",
  "closed",
  "error",
] as const;
export type TerminalConnStatus = (typeof TERMINAL_CONN_STATUSES)[number];

// iTerm daemon health reported by `get_terminal_status`. Slice-PRIVATE wire mirror of the
// Rust `src-tauri/src/terminal/process.rs::TerminalDaemonStatus` (`#[serde(rename_all =
// "camelCase")]`) — same placement rationale as the review slice's CodexStatus. The Rust
// side carries a golden wire-shape test (asserts `available` / `desiredRunning` / `message`
// camelCase keys, snake_case absent); this interface is the matching downstream end. Keep
// the two in lockstep — a field added here without a Rust change (or vice versa) drifts the
// contract. (No StatusBar consumer yet; this closes the previously-open mirror funnel.)
export interface TerminalDaemonStatus {
  available: boolean;
  desiredRunning: boolean;
  message: string;
}

// The sink the XtermPane registers with the store to receive each rendered screen frame.
// The store owns the frame data (state); the pane owns the xterm Terminal object (DOM) —
// this callback is the one-way bridge between them, so the Terminal never enters reactive
// state. A frame carries the full visible-screen `data` (write after term.reset()) plus an
// optional cursor position to reposition after the write.
export type ScreenSink = (frame: {
  data: string;
  cursorRow?: number;
  cursorCol?: number;
}) => void;
