// @vitest-environment happy-dom

import { flushPromises, mount } from "@vue/test-utils";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { PullRequestView } from "../types";
import ReviewPanel from "./ReviewPanel.vue";

const { mocks, storeState } = vi.hoisted(() => {
  // Refs are created lazily in the mock factory (vue not available in hoisted sync).
  const bag: {
    items: { value: unknown[] };
    running: { value: boolean };
    finalStatus: { value: string | null };
    error: { value: string | null };
    activePr: { value: number | null };
    activeThreadId: { value: string | null };
    listenerReady: { value: boolean };
    listenerError: { value: string | null };
  } = {} as never;
  return {
    mocks: {
      start: vi.fn(),
      stop: vi.fn(),
      sendMessage: vi.fn(),
      init: vi.fn(),
    },
    storeState: bag,
  };
});

vi.mock("./useReviewStore", async () => {
  const { ref } = await import("vue");
  storeState.items = ref([]);
  storeState.running = ref(false);
  storeState.finalStatus = ref(null);
  storeState.error = ref(null);
  storeState.activePr = ref(null);
  storeState.activeThreadId = ref(null);
  storeState.listenerReady = ref(true);
  storeState.listenerError = ref(null);
  return {
    useReviewStore: () => ({
      items: storeState.items,
      running: storeState.running,
      finalStatus: storeState.finalStatus,
      error: storeState.error,
      activePr: storeState.activePr,
      activeThreadId: storeState.activeThreadId,
      listenerReady: storeState.listenerReady,
      listenerError: storeState.listenerError,
      start: mocks.start,
      stop: mocks.stop,
      sendMessage: mocks.sendMessage,
      init: mocks.init,
    }),
  };
});

vi.mock("../projects", async () => {
  const { ref } = await import("vue");
  return { useProjects: () => ({ activeProjectId: ref("proj-1") }) };
});

function pr(overrides: Partial<PullRequestView> = {}): PullRequestView {
  return {
    number: 42,
    title: "sample",
    labels: [],
    url: "https://example.test/pr/42",
    // Discovery may tag check; explicit Review/Check buttons must ignore this.
    skillKey: "pr-review\0--check",
    skipReason: null,
    ...overrides,
  };
}

beforeEach(() => {
  mocks.start.mockReset();
  mocks.stop.mockReset();
  mocks.sendMessage.mockReset();
  mocks.init.mockReset();
  mocks.init.mockResolvedValue(() => undefined);
  storeState.items.value = [];
  storeState.running.value = false;
  storeState.finalStatus.value = null;
  storeState.error.value = null;
  storeState.activePr.value = null;
  storeState.activeThreadId.value = null;
  storeState.listenerReady.value = true;
  storeState.listenerError.value = null;
});

describe("ReviewPanel composer toolbar", () => {
  it("starts Review with empty extraArgs and Check with --check", async () => {
    const wrapper = mount(ReviewPanel, { props: { selectedPr: pr() } });
    await flushPromises();

    const buttons = wrapper.findAll(".composer-toolbar button");
    const review = buttons.find((b) => b.text() === "Review");
    const check = buttons.find((b) => b.text() === "Check");
    expect(review).toBeTruthy();
    expect(check).toBeTruthy();

    await review!.trigger("click");
    expect(mocks.start).toHaveBeenCalledWith("proj-1", 42, "");

    await check!.trigger("click");
    expect(mocks.start).toHaveBeenCalledWith("proj-1", 42, "--check");
  });

  it("keeps Review/Check/Stop on the same toolbar row as Send", async () => {
    storeState.activeThreadId.value = "thread-1";
    const wrapper = mount(ReviewPanel, { props: { selectedPr: pr() } });
    await flushPromises();

    const toolbar = wrapper.get(".composer-toolbar");
    const labels = toolbar.findAll("button").map((b) => b.text());
    expect(labels).toEqual(["Review", "Check", "停止", "发送 / Send"]);
    expect(wrapper.find(".head .actions").exists()).toBe(false);
  });

  it("keeps the toolbar visible when the chat composer is collapsed", async () => {
    storeState.activeThreadId.value = "thread-1";
    const wrapper = mount(ReviewPanel, { props: { selectedPr: pr() } });
    await flushPromises();

    await wrapper.get(".toggle").trigger("click");
    expect(wrapper.find("textarea.composer-input").exists()).toBe(false);
    expect(wrapper.find(".composer-toolbar").exists()).toBe(true);
    expect(wrapper.findAll(".composer-toolbar button").map((b) => b.text())).toEqual([
      "Review",
      "Check",
      "停止",
      "发送 / Send",
    ]);
  });
});
