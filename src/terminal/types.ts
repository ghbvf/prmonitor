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

// The sink the XtermPane registers with the store to bridge backend output to the xterm
// Terminal. The store owns the output data (state); the pane owns the xterm Terminal object
// (DOM) — this object is the one-way bridge between them, so the Terminal never enters reactive
// state. Two methods, one per backend render model (#1372):
//   • `writeFrame` — an iTerm FULL screen snapshot: the pane resets + writes the whole grid,
//     then repositions the cursor. `data` is the rendered visible screen.
//   • `writeRaw` — incremental raw PTY bytes: the pane does a plain `term.write(bytes)` (no
//     reset). Bytes (not a string) so xterm's own decoder stitches multi-byte UTF-8 across chunks.
export interface ScreenSink {
  writeFrame(frame: { data: string; cursorRow?: number; cursorCol?: number }): void;
  writeRaw(bytes: Uint8Array): void;
}
