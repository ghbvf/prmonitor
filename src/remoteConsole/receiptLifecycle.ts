import type { ReviewSession } from "../review/types";
import type {
  ExternalRequestId,
  ReviewReceiptId,
  ReviewReceiptSnapshot,
} from "../types.generated";

export interface MutableReceiptState {
  receipt: ReviewReceiptSnapshot | null;
  activeReceiptId: ReviewReceiptId | null;
  receiptPollingStopped: boolean;
}

export function invalidateReceiptState(state: MutableReceiptState, stopPolling: () => void): void {
  stopPolling();
  state.receipt = null;
  state.activeReceiptId = null;
  state.receiptPollingStopped = false;
}

export function mergeSessionsByThread(
  current: ReviewSession[],
  incoming: ReviewSession[],
): ReviewSession[] {
  const byThread = new Map(current.map((session) => [session.threadId, session]));
  for (const session of incoming) byThread.set(session.threadId, session);
  return Array.from(byThread.values());
}

export class RequestIdLifecycle {
  private operation: string | null = null;
  private requestId: ExternalRequestId | null = null;

  constructor(private readonly create: () => ExternalRequestId) {}

  forOperation(projectId: string, prNumber: number, extraArgs: string = ""): ExternalRequestId {
    const operation = `${projectId}\n${prNumber}\n${extraArgs}`;
    if (this.operation !== operation || this.requestId === null) {
      this.operation = operation;
      this.requestId = this.create();
    }
    return this.requestId;
  }

  accepted(): void {
    this.operation = null;
    this.requestId = null;
  }

  selectionChanged(): void {
    this.accepted();
  }
}

export interface ReceiptPollResult {
  receipt: ReviewReceiptSnapshot;
  sessions: ReviewSession[] | null;
  sessionRefreshError: unknown | null;
}

export async function pollReceiptOnce(
  receiptId: ReviewReceiptId,
  getReceipt: (id: ReviewReceiptId) => Promise<ReviewReceiptSnapshot>,
  refreshSessions: () => Promise<ReviewSession[]>,
): Promise<ReceiptPollResult> {
  const receipt = await getReceipt(receiptId);
  if (receipt.status !== "done" && receipt.status !== "failed") {
    return { receipt, sessions: null, sessionRefreshError: null };
  }
  try {
    return { receipt, sessions: await refreshSessions(), sessionRefreshError: null };
  } catch (error) {
    return { receipt, sessions: null, sessionRefreshError: error };
  }
}
