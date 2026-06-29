import { beforeEach, describe, expect, it, vi } from "vitest";

const request = vi.fn();

vi.mock("../transport", () => ({
  getTransport: () => ({ request }),
}));

import {
  messagingEventRaw,
  messagingEventReplay,
  messagingEventsList,
  messagingIntegrationsList,
  messagingSend,
  messagingSendsList,
} from "./api";

beforeEach(() => {
  request.mockReset();
  request.mockResolvedValue(null);
});

describe("messaging api wrappers", () => {
  it("uses stable Tauri command envelopes", async () => {
    await messagingSend({
      integrationId: "fs",
      conversationId: "chat",
      text: "hello",
      requestId: "req-1",
    });
    await messagingEventsList("fs");
    await messagingEventRaw(7);
    await messagingEventReplay(7);
    await messagingSendsList("fs");
    await messagingIntegrationsList();

    expect(request).toHaveBeenNthCalledWith(1, "messaging_send", {
      request: {
        integrationId: "fs",
        conversationId: "chat",
        text: "hello",
        requestId: "req-1",
      },
    });
    expect(request).toHaveBeenNthCalledWith(2, "messaging_events_list", { integrationId: "fs" });
    expect(request).toHaveBeenNthCalledWith(3, "messaging_event_raw", { id: 7 });
    expect(request).toHaveBeenNthCalledWith(4, "messaging_event_replay", { id: 7 });
    expect(request).toHaveBeenNthCalledWith(5, "messaging_sends_list", { integrationId: "fs" });
    expect(request).toHaveBeenNthCalledWith(6, "messaging_integrations_list");
  });
});
