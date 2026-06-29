import { beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import type { WorkflowEvent, WorkflowInstance } from "../types";

let workflowCb: ((e: WorkflowEvent) => void) | null = null;

vi.mock("./api", () => ({
  WORKFLOW_UPDATED_EVENT: "workflow:updated",
  workflowList: vi.fn(() => Promise.resolve([])),
  workflowGetRaw: vi.fn(() => Promise.resolve("{}")),
  workflowRetry: vi.fn(() => Promise.resolve()),
  onWorkflowUpdated: vi.fn((cb: (e: WorkflowEvent) => void) => {
    workflowCb = cb;
    return Promise.resolve(() => {});
  }),
}));

import * as api from "./api";
import { useWorkflowStore } from "./useWorkflowStore";

const entry = (id: number, projectId = "p1"): WorkflowInstance => ({
  id,
  projectId,
  type: "reviewNotify",
  status: "waiting",
  currentStep: "waitReview",
  input: { reference: projectId, prNumber: id, kind: "review" },
  state: {},
  attemptCount: 0,
  nextWakeAt: 0,
  lastError: null,
  createdAt: id,
  updatedAt: id,
});

beforeEach(() => {
  setActivePinia(createPinia());
  workflowCb = null;
  vi.clearAllMocks();
  vi.mocked(api.workflowList).mockResolvedValue([]);
  vi.mocked(api.workflowGetRaw).mockResolvedValue("{}");
  vi.mocked(api.workflowRetry).mockResolvedValue(undefined);
  vi.mocked(api.onWorkflowUpdated).mockImplementation((cb) => {
    workflowCb = cb;
    return Promise.resolve(() => {});
  });
});

describe("useWorkflowStore", () => {
  it("upserts by id and sorts newest first", () => {
    const store = useWorkflowStore();
    store.upsert(entry(1));
    store.upsert(entry(3));
    store.upsert({ ...entry(1), status: "done", updatedAt: 4 });

    expect(store.entries.map((e) => e.id)).toEqual([1, 3]);
    expect(store.entries[0].status).toBe("done");
  });

  it("honors project filter for updated events", () => {
    const store = useWorkflowStore();
    store.projectId = "p1";
    store.subscribe();

    workflowCb?.({ kind: "updated", projectId: "p1", instance: entry(1, "p1") });
    workflowCb?.({ kind: "updated", projectId: "p2", instance: entry(2, "p2") });

    expect(store.entries.map((e) => e.id)).toEqual([1]);
  });

  it("surfaces worker errors separately from command errors", () => {
    const store = useWorkflowStore();
    store.subscribe();

    workflowCb?.({ kind: "error", operation: "announce", message: "db locked" });

    expect(store.cycleError).toBe("announce: db locked");
    expect(store.error).toBeNull();
  });

  it("subscribes before loading snapshot", async () => {
    const order: string[] = [];
    vi.mocked(api.onWorkflowUpdated).mockImplementation((cb) => {
      order.push("subscribe");
      workflowCb = cb;
      return Promise.resolve(() => {});
    });
    vi.mocked(api.workflowList).mockImplementation(() => {
      order.push("snapshot");
      return Promise.resolve([]);
    });
    const store = useWorkflowStore();

    await store.init();

    expect(order).toEqual(["subscribe", "snapshot"]);
  });

  it("guards retry re-entry and refreshes after retry", async () => {
    const store = useWorkflowStore();

    await Promise.all([store.retry(7), store.retry(7)]);

    expect(api.workflowRetry).toHaveBeenCalledOnce();
    expect(api.workflowList).toHaveBeenCalledOnce();
    expect(store.retryLoading[7]).toBe(false);
  });
});
