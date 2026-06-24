// Locks the inbox label helpers (AB#1065), the Medium `assertNever`穷尽 carriers that sit on
// top of the EVENT_TYPES / INBOX_STATUSES Hard `as const` sets. `eventTypeLabel` is the FIRST
// frontend consumer of `event.eventType`; this test pins that every wire value maps to a
// non-empty, distinct label, so a dropped/duplicated arm regresses visibly in CI (the compile
// error catches a MISSING arm; this catches an empty/duplicated one the compiler can't).
import { describe, it, expect } from "vitest";
import {
  EVENT_TYPES,
  INBOX_STATUSES,
  eventTypeLabel,
  inboxStatusLabel,
} from "./types";
import { INBOX_UPDATED_EVENT } from "./inbox/api";

// Machine-pin the TS side of the event-name funnel (Medium): `INBOX_UPDATED_EVENT` is the
// open downstream end of the `inbox:updated` name the backend emits. A typo here would
// silently drop every push, so lock the literal in CI rather than eyeballing the string.
describe("INBOX_UPDATED_EVENT", () => {
  it("matches the backend's inbox:updated event name", () => {
    expect(INBOX_UPDATED_EVENT).toBe("inbox:updated");
  });
});

describe("eventTypeLabel", () => {
  it("returns a non-empty label for every EventType", () => {
    for (const t of EVENT_TYPES) {
      expect(eventTypeLabel(t).length).toBeGreaterThan(0);
    }
  });

  it("maps each EventType to a distinct label (no two classes collide)", () => {
    const labels = EVENT_TYPES.map(eventTypeLabel);
    expect(new Set(labels).size).toBe(EVENT_TYPES.length);
  });

  it("renders the known classes (bilingual zh / en)", () => {
    expect(eventTypeLabel("pullRequest")).toBe("拉取请求 / Pull Request");
    expect(eventTypeLabel("issue")).toBe("议题 / Issue");
    expect(eventTypeLabel("comment")).toBe("评论 / Comment");
    expect(eventTypeLabel("label")).toBe("标签 / Label");
    expect(eventTypeLabel("generic")).toBe("通用 / Generic");
  });
});

describe("inboxStatusLabel", () => {
  it("returns a non-empty label for every InboxStatus", () => {
    for (const s of INBOX_STATUSES) {
      expect(inboxStatusLabel(s).length).toBeGreaterThan(0);
    }
  });

  it("maps each InboxStatus to a distinct label", () => {
    const labels = INBOX_STATUSES.map(inboxStatusLabel);
    expect(new Set(labels).size).toBe(INBOX_STATUSES.length);
  });

  it("renders the known statuses (bilingual zh / en)", () => {
    expect(inboxStatusLabel("received")).toBe("已接收 / Received");
    expect(inboxStatusLabel("processed")).toBe("已处理 / Processed");
    expect(inboxStatusLabel("failed")).toBe("失败 / Failed");
  });
});
