// useReviewStore codex-status tests. Drives the factory store against a mocked
// `./api` module so the assertions stay deterministic — mirrors the mock style
// of src/pr/usePrStore.test.ts.
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./api", () => ({
  getCodexStatus: vi.fn(() =>
    Promise.resolve({ available: true, message: "ok" }),
  ),
}));

import * as api from "./api";
import { useReviewStore } from "./useReviewStore";

beforeEach(() => {
  vi.clearAllMocks();
  // Restore default resolved behavior wiped by clearAllMocks.
  vi.mocked(api.getCodexStatus).mockResolvedValue({
    available: true,
    message: "ok",
  });
  // The store keeps module-level singleton state, so reset codex between tests
  // to keep the initial-null assertion independent of test ordering.
  useReviewStore().codex.value = null;
});

describe("useReviewStore refreshCodexStatus()", () => {
  it("writes codex.value from getCodexStatus on success", async () => {
    const store = useReviewStore();
    expect(store.codex.value).toBeNull();

    await store.refreshCodexStatus();

    expect(api.getCodexStatus).toHaveBeenCalledOnce();
    expect(store.codex.value).toEqual({ available: true, message: "ok" });
  });

  it("on a rejected invoke sets an unavailable codex status", async () => {
    vi.mocked(api.getCodexStatus).mockRejectedValueOnce({ message: "boom" });
    const store = useReviewStore();

    await store.refreshCodexStatus();

    expect(store.codex.value?.available).toBe(false);
    expect(store.codex.value?.message).toBe("boom");
  });
});
