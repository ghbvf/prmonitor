import { getTransport } from "../transport";
import type { SubscribeOptions } from "../transport";
import type { PrEvent, ReviewEvent, TrackedPrView } from "../types";
import type { ClaudeStatus, CodexStatus, ReviewSession, StreamItem } from "../review/types";
import type { EngineKind, SourceKind, UpdateMode } from "../types.generated";

export interface RemoteProjectSummary {
  id: string;
  name: string;
  enabled: boolean;
  repo: string;
  sourceKind: SourceKind;
  engineKind: EngineKind;
  updateMode: UpdateMode;
}

export interface RemoteConsoleSnapshot {
  appVersion: string;
  activeProjectId: string;
  projects: RemoteProjectSummary[];
}

export function remoteConsoleSnapshot(): Promise<RemoteConsoleSnapshot> {
  return getTransport().request<RemoteConsoleSnapshot>("remote_console_snapshot");
}

export function getRemotePrs(projectId: string): Promise<TrackedPrView[]> {
  return getTransport().request<TrackedPrView[]>("get_prs", { projectId });
}

export function listRemoteReviewSessions(): Promise<ReviewSession[]> {
  return getTransport().request<ReviewSession[]>("list_review_sessions");
}

export function getRemotePrSessions(projectId: string, prNumber: number): Promise<ReviewSession[]> {
  return getTransport().request<ReviewSession[]>("get_pr_sessions", { projectId, prNumber });
}

export function getRemoteSessionHistory(
  projectId: string,
  prNumber: number,
  threadId: string,
): Promise<StreamItem[]> {
  return getTransport().request<StreamItem[]>("get_session_history", {
    projectId,
    prNumber,
    threadId,
  });
}

export function getRemoteCodexStatus(): Promise<CodexStatus> {
  return getTransport().request<CodexStatus>("get_codex_status");
}

export function getRemoteClaudeStatus(): Promise<ClaudeStatus> {
  return getTransport().request<ClaudeStatus>("get_claude_status");
}

export function startRemoteReview(
  projectId: string,
  prNumber: number,
  kind: "review" | "check",
): Promise<string> {
  return getTransport().request<string>("start_review", { projectId, prNumber, kind });
}

export function stopRemoteReview(sessionId: string): Promise<void> {
  return getTransport().request<void>("stop_review", { sessionId });
}

export function onRemoteReviewEvent(
  cb: (event: ReviewEvent) => void,
  options: SubscribeOptions = {},
) {
  return Object.keys(options).length > 0
    ? getTransport().subscribe<ReviewEvent>("review:event", cb, options)
    : getTransport().subscribe<ReviewEvent>("review:event", cb);
}

export function onRemotePrEvent(cb: (event: PrEvent) => void, options: SubscribeOptions = {}) {
  return Object.keys(options).length > 0
    ? getTransport().subscribe<PrEvent>("prs:updated", cb, options)
    : getTransport().subscribe<PrEvent>("prs:updated", cb);
}
