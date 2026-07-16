// @vitest-environment happy-dom

import { mount } from "@vue/test-utils";
import { describe, expect, it } from "vitest";
import MessagingConnectionStatusList from "../MessagingConnectionStatusList.vue";
import type { MessagingConnectionStatus } from "../types.generated";

function status(state: MessagingConnectionStatus["status"]): MessagingConnectionStatus {
  return {
    provider: "feishu",
    integrationId: `feishu-${state}`,
    status: state,
    lastConnectedAtEpoch: state === "connected" ? 1_700_000_000 : null,
    lastEventAtEpoch: state === "connected" ? 1_700_000_100 : null,
    lastError: state === "error" ? "bad credentials" : null,
    reconnectCount: state === "reconnecting" ? 4 : 0,
  };
}

describe("MessagingConnectionStatusList", () => {
  it("renders every long-connection state and diagnostic detail", () => {
    const statuses: MessagingConnectionStatus[] = [
      "disabled", "connecting", "connected", "reconnecting", "error", "stopped",
    ].map((state) => status(state as MessagingConnectionStatus["status"]));
    const wrapper = mount(MessagingConnectionStatusList, { props: { statuses } });

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
    expect(wrapper.text()).toContain("飞书");
  });

  it("renders dingTalk provider label and loading empty state", () => {
    const ding: MessagingConnectionStatus = {
      provider: "dingTalk",
      integrationId: "dingtalk-main",
      status: "connected",
      lastConnectedAtEpoch: 1,
      lastEventAtEpoch: null,
      lastError: null,
      reconnectCount: 0,
    };
    const wrapper = mount(MessagingConnectionStatusList, { props: { statuses: [ding] } });
    expect(wrapper.text()).toContain("钉钉");
    expect(wrapper.text()).not.toContain("dingTalk");

    const loading = mount(MessagingConnectionStatusList, {
      props: { statuses: [], loading: true },
    });
    expect(loading.text()).toContain("加载消息长连接状态");
  });

  it("marks a retained connected snapshot as stale after refresh fails", () => {
    const wrapper = mount(MessagingConnectionStatusList, {
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
