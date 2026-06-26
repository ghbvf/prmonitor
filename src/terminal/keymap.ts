// Terminal key → escape-sequence map (#1383). The MobileToolbar emits these special keys
// (which a touch keyboard can't type) and the store forwards the resulting bytes to the
// session via send_terminal_input. Pure (no transport / store dependency) so PR2's browser
// build can reuse the toolbar + this map unchanged.
import { assertNever } from "../types";

// The special keys the touch toolbar exposes. CSI = arrows / nav (ESC [ …); C0 = the ctrl
// chord bytes. Plain printable keys go through the soft keyboard, not this map.
export type TerminalKey =
  | "esc"
  | "tab"
  | "enter"
  | "up"
  | "down"
  | "left"
  | "right"
  | "home"
  | "end"
  | "pageUp"
  | "pageDown"
  | "ctrlC"
  | "ctrlD"
  | "ctrlZ"
  | "ctrlL";

// A control chord byte for letter a–z: 'a' → \x01 … 'z' → \x1a (the C0 control range).
// Lowercased first so ctrlByte("C") === ctrlByte("c"). Used by the ctrl* arms below.
export function ctrlByte(letter: string): string {
  return String.fromCharCode(letter.toLowerCase().charCodeAt(0) - 96);
}

// Map a TerminalKey to the exact bytes a terminal expects. The `default: assertNever(key)`
// (Medium — `assertNever`穷尽, the same carrier class as ../types' label switches) makes
// adding a TerminalKey without a byte mapping a COMPILE error, so the toolbar's buttons and
// their wire bytes can never silently drift.
export function toEscapeSequence(key: TerminalKey): string {
  switch (key) {
    case "esc":
      return "\x1b";
    case "tab":
      return "\x09"; // horizontal tab
    case "enter":
      return "\r"; // carriage return (\x0d)
    case "up":
      return "\x1b[A";
    case "down":
      return "\x1b[B";
    case "right":
      return "\x1b[C";
    case "left":
      return "\x1b[D";
    case "home":
      return "\x1b[H";
    case "end":
      return "\x1b[F";
    case "pageUp":
      return "\x1b[5~";
    case "pageDown":
      return "\x1b[6~";
    case "ctrlC":
      return ctrlByte("c"); // \x03 — interrupt
    case "ctrlD":
      return ctrlByte("d"); // \x04 — EOF
    case "ctrlZ":
      return ctrlByte("z"); // \x1a — suspend
    case "ctrlL":
      return ctrlByte("l"); // \x0c — clear
    default:
      return assertNever(key);
  }
}
