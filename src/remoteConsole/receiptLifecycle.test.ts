import { describe, expect, it, vi } from "vitest";
import { externalRequestId, reviewReceiptId } from "../types.generated";
import type { ReviewSession } from "../review/types";
import {
  invalidateReceiptState,
  mergeSessionsByThread,
  pollReceiptOnce,
  RequestIdLifecycle,
} from "./receiptLifecycle";

function session(threadId: string, createdAtEpoch: number): ReviewSession {
  return {
    projectId: "p1",
    threadId,
    turnId: `${threadId}-turn`,
    prNumber: 42,
    skillKey: "pr-review\0",
    engineKind: "codex",
    status: "running",
    createdAtEpoch,
  };
}

describe("receipt lifecycle", () => {
  it("merges the session that first appears on a running receipt", () => {
    const merged = mergeSessionsByThread([session("old", 1)], [session("new", 2)]);
    expect(merged.map((item) => item.threadId)).toEqual(["old", "new"]);
    expect(merged.find((item) => item.threadId === "new")?.status).toBe("running");
  });

  it("invalidates the old receipt when project/PR selection changes", () => {
    const state = {
      receipt: { receiptId: reviewReceiptId(7), status: "queued" as const, threadId: null, commentUrl: null, outcome: null, error: null },
      activeReceiptId: reviewReceiptId(7),
      receiptPollingStopped: true,
    };
    const stop = vi.fn();

    invalidateReceiptState(state, stop);

    expect(stop).toHaveBeenCalledOnce();
    expect(state.receipt).toBeNull();
    expect(state.activeReceiptId).toBeNull();
    expect(state.receiptPollingStopped).toBe(false);
  });

  it("keeps one requestId for ambiguous retries of the same logical operation", () => {
    const ids = [
      externalRequestId("00112233445566778899aabbccddeeff"),
      externalRequestId("ffeeddccbbaa99887766554433221100"),
    ];
    const lifecycle = new RequestIdLifecycle(() => ids.shift()!);

    const first = lifecycle.forOperation("p1", 42, "");
    const retry = lifecycle.forOperation("p1", 42, "");
    expect(retry).toBe(first);

    lifecycle.accepted();
    expect(lifecycle.forOperation("p1", 42, "")).not.toBe(first);
  });

  it("does not turn a terminal receipt into a receipt retry when session refresh fails", async () => {
    const getReceipt = vi.fn().mockResolvedValue({
      receiptId: reviewReceiptId(7),
      status: "done",
      threadId: "new",
      commentUrl: null,
      error: null,
    });
    const refreshSessions = vi.fn().mockRejectedValue(new Error("session refresh failed"));

    const result = await pollReceiptOnce(reviewReceiptId(7), getReceipt, refreshSessions);

    expect(result.receipt.status).toBe("done");
    expect(result.sessionRefreshError).toEqual(new Error("session refresh failed"));
    expect(getReceipt).toHaveBeenCalledOnce();
  });
});
