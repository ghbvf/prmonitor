import { getTransport } from "../transport";
import type { SubscribeOptions } from "../transport";
import type { PrEvent, ReviewEvent, TrackedPrView } from "../types";
import type { ClaudeStatus, CodexStatus, ReviewSession, StreamItem } from "../review/types";
import type {
  EngineKind,
  ExternalRequestId,
  ReviewKind,
  ReviewReceiptId,
  ReviewReceiptSnapshot,
  SourceKind,
  UpdateMode,
} from "../types.generated";
import {
  externalRequestId,
  reviewReceiptId,
  REVIEW_RECEIPT_STATUSES,
} from "../types.generated";
export const LOCAL_API_BEARER_TOKEN_KEY = "prmonitor.localApiBearerToken";

export interface ReviewReceiptAccepted {
  receiptId: ReviewReceiptId;
  statusUrl: string;
}

interface RemoteErrorBody {
  message?: string;
}

function record(value: unknown, context: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`${context}: expected JSON object`);
  }
  return value as Record<string, unknown>;
}

function nullableString(value: unknown, field: string): string | null {
  if (value === null || typeof value === "string") return value;
  throw new Error(`receipt response: ${field} must be string or null`);
}

function parsedReceiptId(value: unknown): ReviewReceiptId {
  if (typeof value !== "number") {
    throw new Error("receipt response: receiptId must be a number");
  }
  return reviewReceiptId(value);
}

function parseReceiptAccepted(value: unknown): ReviewReceiptAccepted {
  const body = record(value, "review request response");
  const receiptId = parsedReceiptId(body.receiptId);
  if (typeof body.statusUrl !== "string" || body.statusUrl.length === 0) {
    throw new Error("review request response: statusUrl must be a non-empty string");
  }
  const expectedStatusUrl = receiptStatusUrl(receiptId);
  if (body.statusUrl !== expectedStatusUrl) {
    throw new Error("review request response: statusUrl is outside the trusted LocalApi route");
  }
  return {
    receiptId,
    statusUrl: expectedStatusUrl,
  };
}

function parseReceiptSnapshot(value: unknown): ReviewReceiptSnapshot {
  const body = record(value, "receipt response");
  if (
    typeof body.status !== "string" ||
    !REVIEW_RECEIPT_STATUSES.includes(body.status as ReviewReceiptSnapshot["status"])
  ) {
    throw new Error("receipt response: invalid status");
  }
  return {
    receiptId: parsedReceiptId(body.receiptId),
    status: body.status as ReviewReceiptSnapshot["status"],
    threadId: nullableString(body.threadId, "threadId"),
    commentUrl: nullableString(body.commentUrl, "commentUrl"),
    outcome: nullableString(body.outcome, "outcome"),
    error: nullableString(body.error, "error"),
  };
}

function trimTrailingSlashes(value: string): string {
  return value.replace(/\/+$/, "");
}

function remoteApiBaseUrl(): string {
  const runtime = globalThis.window?.__PRMONITOR_API_BASE_PATH__?.trim();
  if (runtime) return trimTrailingSlashes(runtime);
  const configured = import.meta.env.VITE_API_BASE_URL?.trim();
  if (configured) return trimTrailingSlashes(configured);
  // The SPA is served by a Terminal route (commonly `/ui`), while durable review receipts
  // belong to the independently mounted LocalApi route. Never reuse the injected SPA base here:
  // doing so would silently send `/reviews` to `/ui/reviews`. `/api` is the configured default;
  // The server injects the live LocalApi route. The build-time override remains useful for a
  // separately hosted development shell; `/api` is only the final default.
  return "/api";
}

function bearerHeaders(): Record<string, string> {
  let token = "";
  try {
    token = globalThis.sessionStorage?.getItem(LOCAL_API_BEARER_TOKEN_KEY)?.trim() ?? "";
  } catch {
    // Hardened browser contexts may deny session storage; omit auth and let LocalApi fail closed.
  }
  return token ? { authorization: `Bearer ${token}` } : {};
}

function receiptStatusUrl(receiptId: ReviewReceiptId): string {
  return `${remoteApiBaseUrl()}/reviews/${receiptId}`;
}

async function receiptJson<T>(url: string, init?: RequestInit): Promise<T> {
  const response = await fetch(url, {
    ...init,
    headers: {
      ...bearerHeaders(),
      ...(init?.body ? { "content-type": "application/json" } : {}),
      ...(init?.headers ?? {}),
    },
  });
  const text = await response.text();
  if (!response.ok) {
    let message = text || `${response.status} ${response.statusText}`;
    try {
      message = (JSON.parse(text) as RemoteErrorBody).message ?? message;
    } catch {
      // Keep the text/status fallback.
    }
    throw new Error(message);
  }
  return JSON.parse(text) as T;
}

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

export function createExternalRequestId(): ExternalRequestId {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return externalRequestId(
    Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join(""),
  );
}

export function requestRemoteReview(
  projectId: string,
  prNumber: number,
  kind: ReviewKind,
  requestId: ExternalRequestId = createExternalRequestId(),
): Promise<ReviewReceiptAccepted> {
  return receiptJson<unknown>(`${remoteApiBaseUrl()}/reviews`, {
    method: "POST",
    body: JSON.stringify({
      projectId,
      pr: prNumber,
      kind,
      requestId,
    }),
  }).then(parseReceiptAccepted);
}

export function getRemoteReviewReceipt(
  receiptId: ReviewReceiptId,
): Promise<ReviewReceiptSnapshot> {
  return receiptJson<unknown>(receiptStatusUrl(receiptId)).then(parseReceiptSnapshot);
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
