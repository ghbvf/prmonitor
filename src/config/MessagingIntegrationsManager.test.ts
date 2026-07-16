// @vitest-environment happy-dom

import { flushPromises, mount } from "@vue/test-utils";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { AppConfig, MessagingIntegration } from "./types";
import type { MessagingConnectionStatus } from "../types.generated";
import MessagingIntegrationsManager from "./MessagingIntegrationsManager.vue";

const { getMessagingConnectionStatuses } = vi.hoisted(() => ({
  getMessagingConnectionStatuses: vi.fn<() => Promise<MessagingConnectionStatus[]>>(() => Promise.resolve([])),
}));
vi.mock("./api", () => ({ getMessagingConnectionStatuses }));

function integration(kind: MessagingIntegration["kind"]): MessagingIntegration {
  return {
    id: `${kind}-main`,
    name: kind,
    kind,
    enabled: true,
    verificationToken: "legacy-token",
    encryptKey: "legacy-encrypt-key",
    appId: "app-id",
    appSecret: "app-secret",
    botOpenId: "bot-id",
    cardTemplateId: "card-template",
    allowedConversationIds: ["conversation-1"],
    requireMention: true,
    timeoutSecs: 15,
  };
}

function draft(integrations: MessagingIntegration[]): AppConfig {
  return { messaging: { integrations } } as AppConfig;
}

describe("MessagingIntegrationsManager Feishu long connection", () => {
  beforeEach(() => {
    getMessagingConnectionStatuses.mockReset();
    getMessagingConnectionStatuses.mockResolvedValue([]);
  });

  it("keeps only long-connection credentials and clears obsolete callback secrets", () => {
    const feishu = integration("feishu");
    const wrapper = mount(MessagingIntegrationsManager, { props: { draft: draft([feishu]) } });

    expect(wrapper.text()).toContain("App ID");
    expect(wrapper.text()).toContain("App Secret");
    expect(wrapper.text()).not.toContain("Verification Token");
    expect(wrapper.text()).not.toContain("Encrypt Key");
    expect(feishu.verificationToken).toBe("");
    expect(feishu.encryptKey).toBe("");
  });

  it("does not clear callback credentials for other providers", () => {
    const wechat = integration("weChatWork");
    const wrapper = mount(MessagingIntegrationsManager, { props: { draft: draft([wechat]) } });

    expect(wrapper.text()).toContain("Token");
    expect(wrapper.text()).toContain("Encoding AES Key");
    expect(wechat.verificationToken).toBe("legacy-token");
    expect(wechat.encryptKey).toBe("legacy-encrypt-key");
  });

  it("clears obsolete dingTalk webhook secrets and shows card template guidance", () => {
    const dingTalk = integration("dingTalk");
    const wrapper = mount(MessagingIntegrationsManager, { props: { draft: draft([dingTalk]) } });

    expect(wrapper.text()).toContain("App Key");
    expect(wrapper.text()).toContain("卡片模板 ID");
    expect(wrapper.text()).toContain("Stream");
    expect(wrapper.text()).toContain("开放平台");
    expect(dingTalk.verificationToken).toBe("");
    expect(dingTalk.encryptKey).toBe("");
  });

  it("retains and marks the last successful snapshot stale when refresh fails", async () => {
    getMessagingConnectionStatuses.mockResolvedValueOnce([{
      provider: "feishu",
      integrationId: "feishu-main",
      status: "connected",
      lastConnectedAtEpoch: 1,
      lastEventAtEpoch: null,
      lastError: null,
      reconnectCount: 0,
    }]);
    const wrapper = mount(MessagingIntegrationsManager, {
      props: { draft: draft([integration("feishu")]) },
    });
    await flushPromises();

    getMessagingConnectionStatuses.mockRejectedValueOnce(new Error("offline"));
    const refresh = wrapper.findAll("button").find((button) => button.text() === "刷新");
    expect(refresh).toBeDefined();
    await refresh!.trigger("click");
    await flushPromises();

    expect(wrapper.text()).toContain("offline");
    expect(wrapper.text()).toContain("已过期 / Stale");
    expect(wrapper.get(".state").classes()).toContain("stale");
    expect(wrapper.get(".state").classes()).not.toContain("connected");
  });

  it("uses generated MESSAGING_LONG_CONNECTION_KINDS for status panel visibility", async () => {
    const { MESSAGING_LONG_CONNECTION_KINDS } = await import("../types.generated");
    expect([...MESSAGING_LONG_CONNECTION_KINDS]).toEqual(["feishu", "dingTalk"]);
    const wrapper = mount(MessagingIntegrationsManager, {
      props: { draft: draft([integration("weChatWork")]) },
    });
    expect(wrapper.findComponent({ name: "MessagingConnectionStatusList" }).exists()).toBe(false);
  });
});
