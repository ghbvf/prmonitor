// Session grouping for the picker (#1383). The backend returns a FLAT TerminalSession[]
// (the wire shape); the picker renders it as a window → tab → session tree. Pure
// transform (no transport / store dependency) so it is trivially unit-testable.
import type { TerminalSession } from "../types";

// One tab's sessions, id-sorted.
export interface TabGroup {
  tabId: string;
  sessions: TerminalSession[];
}

// One window's tabs, id-sorted.
export interface WindowGroup {
  windowId: string;
  tabs: TabGroup[];
}

// Fold the flat session list into a window → tab → session tree. Order is STABLE: windows,
// tabs, and sessions are each sorted by their id, so a list refresh that re-emits the same
// sessions in a different order produces the identical tree (no picker reshuffle).
export function groupSessions(sessions: TerminalSession[]): WindowGroup[] {
  const windows = new Map<string, Map<string, TerminalSession[]>>();
  for (const s of sessions) {
    let tabs = windows.get(s.windowId);
    if (!tabs) {
      tabs = new Map();
      windows.set(s.windowId, tabs);
    }
    let list = tabs.get(s.tabId);
    if (!list) {
      list = [];
      tabs.set(s.tabId, list);
    }
    list.push(s);
  }

  return [...windows.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([windowId, tabs]) => ({
      windowId,
      tabs: [...tabs.entries()]
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([tabId, list]) => ({
          tabId,
          sessions: [...list].sort((a, b) =>
            a.sessionId.localeCompare(b.sessionId),
          ),
        })),
    }));
}
