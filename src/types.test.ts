// Locks the poll-eligibility helpers (#150 F1b): the frontend's "is this project's loop
// running / does it get poll actions" judgment must fold `enabled` together with the
// mode, mirroring the backend `periodic_polling` gate. A regression to a mode-only check
// would show disabled projects as monitored / actionable.
import { describe, it, expect } from "vitest";
import {
  externalRequestId,
  inboxDedupeKey,
  inboxEventId,
  periodicPollEligible,
  OUTBOX_STATUSES,
  outboxStatusLabel,
  presentEvent,
  outboxProducerKey,
  reviewActionKey,
  reviewReceiptId,
  manualPullEligible,
  TERMINAL_BACKENDS,
  terminalBackendLabel,
  WORKFLOW_STATUSES,
  WORKFLOW_STEPS,
  WORKFLOW_TYPES,
  workflowStatusLabel,
  workflowStepLabel,
  type ExternalRequestId,
  type InboxEventId,
  type OutboxProducerKey,
  type ReviewActionKey,
  type ReviewReceiptId,
  type WorkflowInstance,
} from "./types";

describe("periodicPollEligible", () => {
  it("is true only when enabled AND the mode runs the periodic loop (pull/hybrid)", () => {
    expect(periodicPollEligible(true, "pull-only")).toBe(true);
    expect(periodicPollEligible(true, "hybrid")).toBe(true);
    expect(periodicPollEligible(true, "webhook-only")).toBe(false);
    expect(periodicPollEligible(true, "manual")).toBe(false);
  });

  it("is false for a disabled project regardless of mode", () => {
    expect(periodicPollEligible(false, "pull-only")).toBe(false);
    expect(periodicPollEligible(false, "hybrid")).toBe(false);
    expect(periodicPollEligible(false, "webhook-only")).toBe(false);
    expect(periodicPollEligible(false, "manual")).toBe(false);
  });
});

describe("typed dispatch contracts", () => {
  it("constructs and validates semantic boundary ids", () => {
    expect(externalRequestId("00112233445566778899aabbccddeeff")).toHaveLength(32);
    expect(() => externalRequestId("not-hex")).toThrow(/32 lowercase hexadecimal/);
    expect(inboxEventId(7)).toBe(7);
    expect(reviewReceiptId(9)).toBe(9);
    expect(() => inboxEventId(0)).toThrow(/positive safe integer/);
    expect(() => reviewReceiptId(Number.MAX_SAFE_INTEGER + 1)).toThrow(/positive safe integer/);
    expect(inboxDedupeKey("event:7")).toBe("event:7");
    expect(reviewActionKey("7@abc:review")).toBe("7@abc:review");
    expect(outboxProducerKey("inbox:7:action:review")).toBe("inbox:7:action:review");
  });

  it("keeps equal primitive representations nominally distinct at compile time", () => {
    const request: ExternalRequestId = externalRequestId("00112233445566778899aabbccddeeff");
    const inbox: InboxEventId = inboxEventId(7);
    const receipt: ReviewReceiptId = reviewReceiptId(7);
    const action: ReviewActionKey = reviewActionKey("action");
    const producer: OutboxProducerKey = outboxProducerKey("producer");

    // @ts-expect-error raw strings cannot enter a typed external-request boundary.
    const rawRequest: ExternalRequestId = "00112233445566778899aabbccddeeff";
    // @ts-expect-error inbox ids and receipt ids remain distinct despite both being numbers.
    const wrongReceipt: ReviewReceiptId = inbox;
    // @ts-expect-error producer keys cannot substitute for review-action keys.
    const wrongAction: ReviewActionKey = producer;

    expect([request, receipt, action, rawRequest, wrongReceipt, wrongAction]).toHaveLength(6);
  });

  it("renders every generated action status including blocked", () => {
    expect([...OUTBOX_STATUSES]).toEqual(["pending", "blocked", "done", "dead"]);
    for (const status of OUTBOX_STATUSES) {
      expect(outboxStatusLabel(status).length).toBeGreaterThan(0);
    }
  });

  it("presents explicit review requests without flattening them into observations", () => {
    const presented = presentEvent({
      dedupeKey: inboxDedupeKey("review-request:00112233445566778899aabbccddeeff"),
      source: "github",
      projectId: "p1",
      repo: "octocat/hello",
      payload: {
        kind: "reviewRequest",
        prNumber: 7,
        reviewKind: "check",
        requestId: externalRequestId("00112233445566778899aabbccddeeff"),
        origin: "remoteWeb",
        notifyOnCompletion: false,
      },
      receivedAtEpoch: 42,
    });
    expect(presented).toEqual({ eventType: "pullRequest", number: 7, title: "Check request" });
  });
});

describe("manualPullEligible", () => {
  it("is true only when enabled AND the mode allows on-demand pull (not webhook-only)", () => {
    expect(manualPullEligible(true, "pull-only")).toBe(true);
    expect(manualPullEligible(true, "hybrid")).toBe(true);
    expect(manualPullEligible(true, "manual")).toBe(true);
    expect(manualPullEligible(true, "webhook-only")).toBe(false);
  });

  it("is false for a disabled project regardless of mode", () => {
    expect(manualPullEligible(false, "pull-only")).toBe(false);
    expect(manualPullEligible(false, "manual")).toBe(false);
    expect(manualPullEligible(false, "webhook-only")).toBe(false);
  });
});

// Mirrors the eventTypeLabel / inboxStatusLabel coverage pattern: every TerminalBackend in the
// `as const` set must get a non-empty label, so the value-set and the rendered badge can't drift
// (the `assertNever` default is the Medium exhaustiveness carrier; this is its data-side check).
describe("terminalBackendLabel", () => {
  it("returns a non-empty label for every TerminalBackend", () => {
    for (const b of TERMINAL_BACKENDS) {
      expect(terminalBackendLabel(b).length).toBeGreaterThan(0);
    }
  });
});

describe("workflow contracts", () => {
  it("pins workflow literal sets", () => {
    expect([...WORKFLOW_TYPES]).toEqual(["reviewNotify"]);
    expect([...WORKFLOW_STATUSES]).toEqual(["pending", "running", "waiting", "done", "failed"]);
    expect([...WORKFLOW_STEPS]).toEqual(["startReview", "waitReview", "enqueueNotify", "done"]);
  });

  it("returns non-empty labels for every workflow status and step", () => {
    for (const s of WORKFLOW_STATUSES) {
      expect(workflowStatusLabel(s).length).toBeGreaterThan(0);
    }
    for (const s of WORKFLOW_STEPS) {
      expect(workflowStepLabel(s).length).toBeGreaterThan(0);
    }
  });

  it("types reviewNotify input and state keys as a cross-end contract", () => {
    const row: WorkflowInstance = {
      id: 1,
      projectId: "p1",
      type: "reviewNotify",
      status: "done",
      currentStep: "done",
      input: { reference: "repo", prNumber: 7, kind: "review" },
      state: {
        reviewThreadId: "t1",
        reviewWireStatus: "completed",
        commentUrl: "https://example.com/pr/7#comment",
        notificationOutboxIds: [9],
      },
      attemptCount: 0,
      nextWakeAt: 0,
      lastError: null,
      createdAt: 10,
      updatedAt: 11,
    };

    expect(row.input.prNumber).toBe(7);
    expect(row.state.notificationOutboxIds).toEqual([9]);
  });
});
