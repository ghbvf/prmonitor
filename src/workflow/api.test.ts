import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Transport } from "../transport";
import { setTransport } from "../transport";
import * as api from "./api";

const request = vi.fn();
const subscribe = vi.fn();

setTransport({ request, subscribe, openExternal: vi.fn() } as unknown as Transport);

beforeEach(() => {
  vi.clearAllMocks();
  request.mockResolvedValue(undefined);
  subscribe.mockResolvedValue(() => {});
});

describe("workflow api commands", () => {
  it("lists workflows with the optional project filter", async () => {
    await api.workflowList();
    await api.workflowList("p1");

    expect(request).toHaveBeenNthCalledWith(1, "workflow_list", {
      projectId: undefined,
    });
    expect(request).toHaveBeenNthCalledWith(2, "workflow_list", { projectId: "p1" });
  });

  it("loads one workflow and its raw JSON", async () => {
    await api.workflowGet(7);
    await api.workflowGetRaw(7);

    expect(request).toHaveBeenNthCalledWith(1, "workflow_get", { id: 7 });
    expect(request).toHaveBeenNthCalledWith(2, "workflow_get_raw", { id: 7 });
  });

  it("retries failed workflows by id", async () => {
    await api.workflowRetry(7);

    expect(request).toHaveBeenCalledWith("workflow_retry", { id: 7 });
  });

  it("subscribes to the workflow updated topic", async () => {
    const cb = vi.fn();
    await api.onWorkflowUpdated(cb);

    expect(api.WORKFLOW_UPDATED_EVENT).toBe("workflow:updated");
    expect(subscribe).toHaveBeenCalledWith("workflow:updated", cb);
  });
});
