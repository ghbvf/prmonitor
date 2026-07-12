import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Transport } from "../transport";
import { setTransport } from "../transport";
import { REMOTE_BEARER_TOKEN_KEY } from "../transport/http";
import { LOCAL_API_BEARER_TOKEN_KEY } from "./api";
import { externalRequestId, reviewReceiptId } from "../types.generated";
import * as api from "./api";

const request = vi.fn();
const subscribe = vi.fn();
const fetchMock = vi.fn();
const sessionItems = new Map<string, string>();

setTransport({ request, subscribe, openExternal: vi.fn() } as unknown as Transport);

beforeEach(() => {
  vi.clearAllMocks();
  sessionItems.clear();
  request.mockResolvedValue(undefined);
  subscribe.mockResolvedValue(() => {});
  // The browser shell is served under `/ui`; receipt REST is a distinct LocalApi route.
  vi.stubGlobal("window", { __PRMONITOR_REMOTE_BASE_PATH__: "/ui" });
  vi.stubGlobal("sessionStorage", {
    getItem: (key: string) => sessionItems.get(key) ?? null,
    setItem: (key: string, value: string) => sessionItems.set(key, value),
  });
  vi.stubGlobal("fetch", fetchMock);
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

  it("uses the runtime-injected LocalApi route instead of the UI route", async () => {
    vi.stubGlobal("window", {
      __PRMONITOR_REMOTE_BASE_PATH__: "/ui",
      __PRMONITOR_API_BASE_PATH__: "/local-api",
    });
    fetchMock.mockResolvedValue({
      ok: true,
      status: 202,
      text: async () => JSON.stringify({ receiptId: 11, statusUrl: "/local-api/reviews/11" }),
    });
    await api.requestRemoteReview(
      "p1",
      7,
      "review",
      externalRequestId("a".repeat(32)),
    );
    expect(fetchMock).toHaveBeenCalledWith("/local-api/reviews", expect.any(Object));
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
    await api.getRemoteCursorStatus();
    expect(request).toHaveBeenNthCalledWith(1, "get_codex_status");
    expect(request).toHaveBeenNthCalledWith(2, "get_claude_status");
    expect(request).toHaveBeenNthCalledWith(3, "get_cursor_status");
  });

  it("submits review and check as idempotent external requests", async () => {
    const reviewId = "00112233445566778899aabbccddeeff";
    const checkId = "ffeeddccbbaa99887766554433221100";
    sessionItems.set(REMOTE_BEARER_TOKEN_KEY, "terminal-secret");
    sessionItems.set(LOCAL_API_BEARER_TOKEN_KEY, "local-api-secret");
    fetchMock
      .mockResolvedValueOnce({
        ok: true,
        text: async () => JSON.stringify({ receiptId: 11, statusUrl: "/api/reviews/11" }),
      })
      .mockResolvedValueOnce({
        ok: true,
        text: async () => JSON.stringify({ receiptId: 12, statusUrl: "/api/reviews/12" }),
      });

    await expect(
      api.requestRemoteReview("p1", 42, "review", externalRequestId(reviewId)),
    ).resolves.toEqual({
      receiptId: 11,
      statusUrl: "/api/reviews/11",
    });
    await api.requestRemoteReview("p1", 42, "check", externalRequestId(checkId));
    expect(fetchMock).toHaveBeenNthCalledWith(
      1,
      "/api/reviews",
      expect.objectContaining({
        method: "POST",
        headers: expect.objectContaining({
          authorization: "Bearer local-api-secret",
          "content-type": "application/json",
        }),
        body: JSON.stringify({ projectId: "p1", pr: 42, kind: "review", requestId: reviewId }),
      }),
    );
    expect(fetchMock).toHaveBeenNthCalledWith(
      2,
      "/api/reviews",
      expect.objectContaining({
        body: JSON.stringify({ projectId: "p1", pr: 42, kind: "check", requestId: checkId }),
      }),
    );
    expect(request).not.toHaveBeenCalledWith("request_review", expect.anything());
  });

  it("never reuses the Terminal credential for the independently guarded LocalApi route", async () => {
    sessionItems.set(REMOTE_BEARER_TOKEN_KEY, "terminal-secret");
    sessionItems.set(LOCAL_API_BEARER_TOKEN_KEY, "local-api-secret");
    fetchMock.mockResolvedValue({
      ok: true,
      text: async () => JSON.stringify({ receiptId: 11, statusUrl: "/api/reviews/11" }),
    });

    await api.requestRemoteReview(
      "p1",
      42,
      "review",
      externalRequestId("00112233445566778899aabbccddeeff"),
    );

    expect(fetchMock.mock.calls[0][1].headers.authorization).toBe("Bearer local-api-secret");
    expect(fetchMock.mock.calls[0][1].headers.authorization).not.toContain("terminal-secret");
  });

  it("rejects an untrusted receipt statusUrl before it can receive the LocalApi bearer", async () => {
    sessionItems.set(LOCAL_API_BEARER_TOKEN_KEY, "local-api-secret");
    fetchMock.mockResolvedValue({
      ok: true,
      text: async () =>
        JSON.stringify({ receiptId: 11, statusUrl: "https://attacker.example/steal" }),
    });

    await expect(
      api.requestRemoteReview(
        "p1",
        42,
        "review",
        externalRequestId("00112233445566778899aabbccddeeff"),
      ),
    ).rejects.toThrow(/statusUrl/);
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(fetchMock).not.toHaveBeenCalledWith(
      "https://attacker.example/steal",
      expect.anything(),
    );
  });

  it("does not confuse the remote UI route with the receipt REST route", async () => {
    fetchMock.mockResolvedValue({
      ok: true,
      text: async () => JSON.stringify({ receiptId: 11, statusUrl: "/api/reviews/11" }),
    });

    await api.requestRemoteReview(
      "p1",
      42,
      "review",
      externalRequestId("00112233445566778899aabbccddeeff"),
    );

    expect(fetchMock).toHaveBeenCalledWith("/api/reviews", expect.any(Object));
    expect(fetchMock).not.toHaveBeenCalledWith("/ui/reviews", expect.any(Object));
  });

  it("fails closed when the REST receipt contract has the wrong id type", async () => {
    fetchMock.mockResolvedValue({
      ok: true,
      text: async () => JSON.stringify({ receiptId: "11", statusUrl: "/api/reviews/11" }),
    });

    await expect(
      api.requestRemoteReview(
        "p1",
        42,
        "review",
        externalRequestId("00112233445566778899aabbccddeeff"),
      ),
    ).rejects.toThrow("receiptId must be a number");
  });

  it("creates exactly 32 lowercase hexadecimal request id characters", () => {
    expect(api.createExternalRequestId()).toMatch(/^[0-9a-f]{32}$/);
  });

  it("reads the durable receipt instead of assuming a session started", async () => {
    fetchMock.mockResolvedValue({
      ok: true,
      text: async () =>
        JSON.stringify({
          receiptId: 9,
          status: "queued",
          threadId: null,
          commentUrl: null,
          outcome: null,
          error: null,
        }),
    });
    await expect(
      api.getRemoteReviewReceipt(reviewReceiptId(9)),
    ).resolves.toEqual({
      receiptId: 9,
      status: "queued",
      threadId: null,
      commentUrl: null,
      outcome: null,
      error: null,
    });
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/reviews/9",
      expect.objectContaining({ headers: expect.any(Object) }),
    );
    expect(request).not.toHaveBeenCalledWith("review_receipt", expect.anything());
  });

  it("surfaces REST error bodies from the receipt routes", async () => {
    fetchMock.mockResolvedValue({
      ok: false,
      status: 503,
      statusText: "Unavailable",
      text: async () => JSON.stringify({ message: "receipt store busy" }),
    });
    await expect(api.getRemoteReviewReceipt(reviewReceiptId(9))).rejects.toThrow(
      "receipt store busy",
    );
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
