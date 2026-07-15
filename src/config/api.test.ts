import { beforeEach, describe, expect, it, vi } from "vitest";
import { DEFAULT_CLI_TOOLS_CONFIG } from "./types.generated";

const request = vi.fn();
vi.mock("../transport", () => ({
  getTransport: () => ({ request }),
}));

import { getFeishuConnectionStatuses, probeCliTools } from "./api";

beforeEach(() => request.mockReset());

describe("probeCliTools", () => {
  it("sends the current unsaved CLI draft and refresh flag through the transport", async () => {
    request.mockResolvedValue([]);
    const cliTools = { ...DEFAULT_CLI_TOOLS_CONFIG, ghPath: "/opt/homebrew/bin/gh" };

    await probeCliTools(cliTools, true);

    expect(request).toHaveBeenCalledWith("probe_cli_tools", {
      cliTools,
      refreshPath: true,
    });
  });
});

describe("getFeishuConnectionStatuses", () => {
  it("uses the long-connection runtime status command", async () => {
    request.mockResolvedValue([]);

    await getFeishuConnectionStatuses();

    expect(request).toHaveBeenCalledWith("messaging_connection_statuses_list");
  });
});
