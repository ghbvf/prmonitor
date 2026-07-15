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
  messagingConnectionStatusesList,
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
      content: { kind: "text", text: "hello" },
      requestId: "req-1",
    });
    await messagingSend({
      integrationId: "fs",
      conversationId: "chat",
      content: {
        kind: "card",
        title: "Build complete",
        text: "**status:** done",
        template: "blue",
      },
      requestId: "req-card",
    });
    await messagingEventsList("fs");
    await messagingEventRaw(7);
    await messagingEventReplay(7);
    await messagingSendsList("fs");
    await messagingIntegrationsList();
    await messagingConnectionStatusesList();

    expect(request).toHaveBeenNthCalledWith(1, "messaging_send", {
      request: {
        integrationId: "fs",
        conversationId: "chat",
        content: { kind: "text", text: "hello" },
        requestId: "req-1",
      },
    });
    expect(request).toHaveBeenNthCalledWith(2, "messaging_send", {
      request: {
        integrationId: "fs",
        conversationId: "chat",
        content: {
          kind: "card",
          title: "Build complete",
          text: "**status:** done",
          template: "blue",
        },
        requestId: "req-card",
      },
    });
    expect(request).toHaveBeenNthCalledWith(3, "messaging_events_list", { integrationId: "fs" });
    expect(request).toHaveBeenNthCalledWith(4, "messaging_event_raw", { id: 7 });
    expect(request).toHaveBeenNthCalledWith(5, "messaging_event_replay", { id: 7 });
    expect(request).toHaveBeenNthCalledWith(6, "messaging_sends_list", { integrationId: "fs" });
    expect(request).toHaveBeenNthCalledWith(7, "messaging_integrations_list");
    expect(request).toHaveBeenNthCalledWith(8, "messaging_connection_statuses_list");
  });
});
