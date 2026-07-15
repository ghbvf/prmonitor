// @vitest-environment happy-dom

import { mount } from "@vue/test-utils";
import { describe, expect, it } from "vitest";
import FeishuConnectionStatusList from "../FeishuConnectionStatusList.vue";
import type { FeishuConnectionStatus } from "../types.generated";

function status(state: FeishuConnectionStatus["status"]): FeishuConnectionStatus {
  return {
    integrationId: `feishu-${state}`,
    status: state,
    lastConnectedAtEpoch: state === "connected" ? 1_700_000_000 : null,
    lastEventAtEpoch: state === "connected" ? 1_700_000_100 : null,
    lastError: state === "error" ? "bad credentials" : null,
    reconnectCount: state === "reconnecting" ? 4 : 0,
  };
}

describe("FeishuConnectionStatusList", () => {
  it("renders every long-connection state and diagnostic detail", () => {
    const statuses: FeishuConnectionStatus[] = [
      "disabled", "connecting", "connected", "reconnecting", "error", "stopped",
    ].map((state) => status(state as FeishuConnectionStatus["status"]));
    const wrapper = mount(FeishuConnectionStatusList, { props: { statuses } });

    expect(wrapper.text()).toContain("已禁用");
    expect(wrapper.text()).toContain("连接中");
    expect(wrapper.text()).toContain("已连接");
    expect(wrapper.text()).toContain("重连中");
    expect(wrapper.text()).toContain("错误");
    expect(wrapper.text()).toContain("已停止");
    expect(wrapper.text()).toContain("重连 4 次");
    expect(wrapper.text()).toContain("bad credentials");
    expect(wrapper.text()).toContain("最后连接");
    expect(wrapper.text()).toContain("最后事件");
  });

  it("marks a retained connected snapshot as stale after refresh fails", () => {
    const wrapper = mount(FeishuConnectionStatusList, {
      props: {
        statuses: [status("connected")],
        error: "offline",
        stale: true,
        lastUpdatedAt: 1_700_000_200_000,
      },
    });

    expect(wrapper.text()).toContain("已过期 / Stale");
    expect(wrapper.text()).toContain("快照更新于");
    expect(wrapper.get(".state").classes()).toContain("stale");
    expect(wrapper.get(".state").classes()).not.toContain("connected");
  });
});
