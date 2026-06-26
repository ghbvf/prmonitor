// sessionTree tests (#1383). groupSessions folds the flat TerminalSession[] (the wire
// shape) into a window → tab → session tree for the picker. Order must be STABLE
// (id-sorted) so a list refresh that re-emits the same sessions in a different order
// doesn't reshuffle the picker.
import { describe, expect, it } from "vitest";
import type { TerminalSession } from "../types";
import { groupSessions } from "./sessionTree";

function session(over: Partial<TerminalSession> = {}): TerminalSession {
  return {
    sessionId: "s1",
    windowId: "w1",
    tabId: "t1",
    title: "bash",
    isActive: false,
    rows: 24,
    cols: 80,
    ...over,
  };
}

describe("groupSessions", () => {
  it("returns an empty array for no sessions", () => {
    expect(groupSessions([])).toEqual([]);
  });

  it("groups window → tab → session", () => {
    const tree = groupSessions([
      session({ sessionId: "s1", windowId: "w1", tabId: "t1" }),
      session({ sessionId: "s2", windowId: "w1", tabId: "t2" }),
      session({ sessionId: "s3", windowId: "w2", tabId: "t1" }),
    ]);

    expect(tree.map((w) => w.windowId)).toEqual(["w1", "w2"]);
    const w1 = tree[0];
    expect(w1.tabs.map((t) => t.tabId)).toEqual(["t1", "t2"]);
    expect(w1.tabs[0].sessions.map((s) => s.sessionId)).toEqual(["s1"]);
    expect(w1.tabs[1].sessions.map((s) => s.sessionId)).toEqual(["s2"]);
    expect(tree[1].tabs[0].sessions.map((s) => s.sessionId)).toEqual(["s3"]);
  });

  it("keeps multiple sessions under one tab, id-sorted", () => {
    const tree = groupSessions([
      session({ sessionId: "s3", windowId: "w1", tabId: "t1" }),
      session({ sessionId: "s1", windowId: "w1", tabId: "t1" }),
      session({ sessionId: "s2", windowId: "w1", tabId: "t1" }),
    ]);

    expect(tree).toHaveLength(1);
    expect(tree[0].tabs).toHaveLength(1);
    expect(tree[0].tabs[0].sessions.map((s) => s.sessionId)).toEqual([
      "s1",
      "s2",
      "s3",
    ]);
  });

  it("sorts windows and tabs by id regardless of input order", () => {
    const tree = groupSessions([
      session({ sessionId: "sb", windowId: "w2", tabId: "t2" }),
      session({ sessionId: "sa", windowId: "w1", tabId: "t9" }),
      session({ sessionId: "sc", windowId: "w1", tabId: "t1" }),
    ]);

    expect(tree.map((w) => w.windowId)).toEqual(["w1", "w2"]);
    expect(tree[0].tabs.map((t) => t.tabId)).toEqual(["t1", "t9"]);
  });
});
