// keymap tests (#1383). Each TerminalKey maps to its exact terminal byte sequence;
// the test pins the wire bytes so a regression (e.g. an arrow CSI flipped, or a ctrl
// byte off by one) fails loudly rather than silently sending the wrong escape.
import { describe, expect, it } from "vitest";
import type { TerminalKey } from "./keymap";
import { ctrlByte, toEscapeSequence } from "./keymap";

describe("toEscapeSequence", () => {
  // Exhaustive table of every TerminalKey → its expected byte string. Typed as
  // Record<TerminalKey, string> so dropping a key (or adding one without a row) is a
  // COMPILE error here, keeping this table in lockstep with the union.
  const cases: Record<TerminalKey, string> = {
    esc: "\x1b",
    tab: "\x09",
    enter: "\r",
    up: "\x1b[A",
    down: "\x1b[B",
    left: "\x1b[D",
    right: "\x1b[C",
    home: "\x1b[H",
    end: "\x1b[F",
    pageUp: "\x1b[5~",
    pageDown: "\x1b[6~",
    ctrlC: "\x03",
    ctrlD: "\x04",
    ctrlZ: "\x1a",
    ctrlL: "\x0c",
  };

  for (const [key, expected] of Object.entries(cases) as [TerminalKey, string][]) {
    it(`maps ${key} → ${JSON.stringify(expected)}`, () => {
      expect(toEscapeSequence(key)).toBe(expected);
    });
  }
});

describe("ctrlByte", () => {
  it("maps a → \\x01 (start of the C0 control range)", () => {
    expect(ctrlByte("a")).toBe("\x01");
  });

  it("maps c → \\x03 (the ctrl-C interrupt byte)", () => {
    expect(ctrlByte("c")).toBe("\x03");
  });

  it("maps z → \\x1a (end of the C0 control range)", () => {
    expect(ctrlByte("z")).toBe("\x1a");
  });

  it("is case-insensitive (uppercase C → \\x03)", () => {
    expect(ctrlByte("C")).toBe("\x03");
  });
});
