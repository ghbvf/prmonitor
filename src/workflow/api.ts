import { getTransport } from "../transport";
import type { WorkflowEvent, WorkflowInstance } from "../types";

export const WORKFLOW_UPDATED_EVENT = "workflow:updated" as const;

export function workflowList(projectId?: string): Promise<WorkflowInstance[]> {
  return getTransport().request<WorkflowInstance[]>("workflow_list", { projectId });
}

export function workflowGet(id: number): Promise<WorkflowInstance> {
  return getTransport().request<WorkflowInstance>("workflow_get", { id });
}

export function workflowGetRaw(id: number): Promise<string> {
  return getTransport().request<string>("workflow_get_raw", { id });
}

export function workflowRetry(id: number): Promise<void> {
  return getTransport().request<void>("workflow_retry", { id });
}

export function onWorkflowUpdated(cb: (e: WorkflowEvent) => void) {
  return getTransport().subscribe<WorkflowEvent>(WORKFLOW_UPDATED_EVENT, cb);
}
