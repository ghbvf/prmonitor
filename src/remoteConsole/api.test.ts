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

describe("remote console api", () => {
  it("loads the sanitized remote snapshot", async () => {
    await api.remoteConsoleSnapshot();
    expect(request).toHaveBeenCalledWith("remote_console_snapshot");
  });

  it("loads one project's PRs", async () => {
    await api.getRemotePrs("p1");
    expect(request).toHaveBeenCalledWith("get_prs", { projectId: "p1" });
  });

  it("loads live review sessions", async () => {
    await api.listRemoteReviewSessions();
    expect(request).toHaveBeenCalledWith("list_review_sessions");
  });

  it("loads durable PR sessions", async () => {
    await api.getRemotePrSessions("p1", 42);
    expect(request).toHaveBeenCalledWith("get_pr_sessions", { projectId: "p1", prNumber: 42 });
  });

  it("loads session history", async () => {
    await api.getRemoteSessionHistory("p1", 42, "t1");
    expect(request).toHaveBeenCalledWith("get_session_history", {
      projectId: "p1",
      prNumber: 42,
      threadId: "t1",
    });
  });

  it("checks engine statuses", async () => {
    await api.getRemoteCodexStatus();
    await api.getRemoteClaudeStatus();
    expect(request).toHaveBeenNthCalledWith(1, "get_codex_status");
    expect(request).toHaveBeenNthCalledWith(2, "get_claude_status");
  });

  it("starts review and check via start_review kind", async () => {
    await api.startRemoteReview("p1", 42, "review");
    await api.startRemoteReview("p1", 42, "check");
    expect(request).toHaveBeenNthCalledWith(1, "start_review", {
      projectId: "p1",
      prNumber: 42,
      kind: "review",
    });
    expect(request).toHaveBeenNthCalledWith(2, "start_review", {
      projectId: "p1",
      prNumber: 42,
      kind: "check",
    });
  });

  it("stops a review session", async () => {
    await api.stopRemoteReview("t1");
    expect(request).toHaveBeenCalledWith("stop_review", { sessionId: "t1" });
  });

  it("subscribes to allowed remote event topics", async () => {
    const review = vi.fn();
    const prs = vi.fn();
    await api.onRemoteReviewEvent(review);
    await api.onRemotePrEvent(prs);
    expect(subscribe).toHaveBeenNthCalledWith(1, "review:event", review);
    expect(subscribe).toHaveBeenNthCalledWith(2, "prs:updated", prs);
  });

  it("passes SSE close handlers through to the transport", async () => {
    const review = vi.fn();
    const prs = vi.fn();
    const reviewOptions = { onClosed: vi.fn() };
    const prOptions = { onClosed: vi.fn() };
    await api.onRemoteReviewEvent(review, reviewOptions);
    await api.onRemotePrEvent(prs, prOptions);
    expect(subscribe).toHaveBeenNthCalledWith(1, "review:event", review, reviewOptions);
    expect(subscribe).toHaveBeenNthCalledWith(2, "prs:updated", prs, prOptions);
  });
});
