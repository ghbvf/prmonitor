import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Transport } from "../transport";
import { setTransport } from "../transport";
import { sendNotification } from "./api";

const request = vi.fn();
setTransport({ request, subscribe: vi.fn(), openExternal: vi.fn() } as unknown as Transport);

beforeEach(() => {
  vi.clearAllMocks();
  request.mockResolvedValue({ outboxIds: [10, 11] });
});

describe("notification api commands", () => {
  it("invokes send_notification with the generated camelCase request", async () => {
    const body = {
      level: "warning" as const,
      title: "Deploy done",
      body: "Build 42 finished",
      url: "https://example.com/build/42",
      projectId: "p1",
      channelIds: ["desktop", "slack-main"],
    };

    await expect(sendNotification(body)).resolves.toEqual({ outboxIds: [10, 11] });
    expect(request).toHaveBeenCalledWith("send_notification", { request: body });
  });
});
