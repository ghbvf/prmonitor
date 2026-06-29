// Locks the outbox label helpers (AB#1066), the Medium `assertNever`穷尽 carriers that sit on
// top of the OUTBOX_STATUSES / ACTION_KINDS Hard `as const` sets (mirrors src/inbox.test.ts).
// This test pins that every wire value maps to a non-empty, distinct label, so a
// dropped/duplicated arm regresses visibly in CI (the compile error catches a MISSING arm;
// this catches an empty/duplicated one the compiler can't).
import { describe, it, expect } from "vitest";
import {
  ACTION_KINDS,
  OUTBOX_STATUSES,
  outboxKindLabel,
  outboxStatusLabel,
} from "./types";
import { OUTBOX_UPDATED_EVENT } from "./outbox/api";

// Machine-pin the TS side of the event-name funnel (Medium): `OUTBOX_UPDATED_EVENT` is the
// open downstream end of the `outbox:updated` name the backend emits. A typo here would
// silently drop every push, so lock the literal in CI rather than eyeballing the string.
describe("OUTBOX_UPDATED_EVENT", () => {
  it("matches the backend's outbox:updated event name", () => {
    expect(OUTBOX_UPDATED_EVENT).toBe("outbox:updated");
  });
});

describe("outboxStatusLabel", () => {
  it("returns a non-empty label for every OutboxStatus", () => {
    for (const s of OUTBOX_STATUSES) {
      expect(outboxStatusLabel(s).length).toBeGreaterThan(0);
    }
  });

  it("maps each OutboxStatus to a distinct label", () => {
    const labels = OUTBOX_STATUSES.map(outboxStatusLabel);
    expect(new Set(labels).size).toBe(OUTBOX_STATUSES.length);
  });

  it("renders the known statuses (bilingual zh / en)", () => {
    expect(outboxStatusLabel("pending")).toBe("待执行 / Pending");
    expect(outboxStatusLabel("done")).toBe("已完成 / Done");
    expect(outboxStatusLabel("dead")).toBe("最终失败 / Dead-letter");
  });
});

describe("outboxKindLabel", () => {
  it("returns a non-empty label for every ActionKind", () => {
    for (const k of ACTION_KINDS) {
      expect(outboxKindLabel(k).length).toBeGreaterThan(0);
    }
  });

  it("maps each ActionKind to a distinct label", () => {
    const labels = ACTION_KINDS.map(outboxKindLabel);
    expect(new Set(labels).size).toBe(ACTION_KINDS.length);
  });

  it("renders the known kinds (bilingual zh / en)", () => {
    expect(outboxKindLabel("notification")).toBe("通知 / Notification");
    expect(outboxKindLabel("review")).toBe("评审 / Review");
    expect(outboxKindLabel("check")).toBe("复查 / Check");
    expect(outboxKindLabel("stopReview")).toBe("停止评审 / Stop Review");
    expect(outboxKindLabel("messagingReply")).toBe("消息回复 / Messaging Reply");
    expect(outboxKindLabel("messagingSend")).toBe("消息发送 / Messaging Send");
  });

  it("includes the action-executor kinds (mirrors Rust ActionKind)", () => {
    // Pins the `as const` set membership — review/check/stopReview are the AB#1069 additions;
    // messagingReply is the #1559 bot reply executor; messagingSend is active messaging egress.
    expect([...ACTION_KINDS]).toEqual([
      "notification",
      "review",
      "check",
      "stopReview",
      "messagingReply",
      "messagingSend",
    ]);
  });
});
