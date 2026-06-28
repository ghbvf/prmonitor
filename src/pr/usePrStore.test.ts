// usePrStore state-machine tests (#27 F4, multi-project #35). Drives the store
// against a mocked `./api` module so the assertions stay deterministic — the
// `prs:updated` event is simulated by invoking the captured `onPrsUpdated`
// callback directly rather than through the Tauri event bus. State is partitioned
// by projectId; tests seed `prs["p1"]` and set "p1" active via useProjects().
import { beforeEach, describe, expect, it, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import type { PrEvent, TrackedPrView } from "../types";
import type { PollStatus } from "./types";
import type { Project } from "../config/types";

// Captured callback handed to `onPrsUpdated`, so a test can push a `PrEvent`
// through the same path `subscribe()` wires up.
let prsCb: ((e: PrEvent) => void) | null = null;

// A full PollStatus shape (#66 F8) so the `pollStatus` mock resolves the same wire
// form refreshPollStatus writes; `running` defaults true so the F4 reconciliation
// keeps `polling` at its baseline unless a test overrides it.
const pollStatus = (
  over: Partial<PollStatus> = {},
): PollStatus => ({
  running: true,
  intervalSecs: 60,
  lastStartedEpoch: null,
  lastSuccessEpoch: null,
  lastErrorEpoch: null,
  lastErrorMessage: null,
  lastPersistEpoch: null,
  lastDiscoveredCount: null,
  ...over,
});

vi.mock("./api", () => ({
  pollNow: vi.fn(() => Promise.resolve()),
  startPolling: vi.fn(() => Promise.resolve()),
  stopPolling: vi.fn(() => Promise.resolve()),
  ghStatus: vi.fn(() => Promise.resolve({ authenticated: true, message: "" })),
  getPrs: vi.fn(() => Promise.resolve([])),
  setPrArchived: vi.fn(() => Promise.resolve()),
  // pollStatus mock (#66 F8): refreshPollStatus is fired from subscribe()/init()/
  // toggle()/switchTo(), so the chains need a resolved PollStatus to write.
  pollStatus: vi.fn(() =>
    Promise.resolve({
      running: true,
      intervalSecs: 60,
      lastStartedEpoch: null,
      lastSuccessEpoch: null,
      lastErrorEpoch: null,
      lastErrorMessage: null,
      lastPersistEpoch: null,
      lastDiscoveredCount: null,
    }),
  ),
  onPrsUpdated: vi.fn((cb: (e: PrEvent) => void) => {
    prsCb = cb;
    // onPrsUpdated returns a Promise<UnlistenFn>.
    return Promise.resolve(() => {});
  }),
}));

// `useProjects().setActive` persists via config/api → the Transport port (AB#1375);
// stub the port so `switchTo` does not hit a real backend.
vi.mock("../transport", () => ({
  getTransport: () => ({
    request: vi.fn(() => Promise.resolve()),
    subscribe: vi.fn(() => Promise.resolve(() => {})),
    openExternal: vi.fn(() => Promise.resolve()),
  }),
}));

import * as api from "./api";
import { usePrStore } from "./usePrStore";
import { useProjects } from "../projects";

// `track` overrides the tracking fields so a test can mint stale / archived rows;
// the defaults (current + not archived) keep every existing single-arg call valid.
const view = (
  number: number,
  track: Partial<Pick<TrackedPrView, "presence" | "archived">> = {},
): TrackedPrView => ({
  number,
  title: `PR #${number}`,
  labels: [],
  url: `https://example.test/${number}`,
  kind: "review",
  skipReason: null,
  presence: "current",
  archived: false,
  ...track,
});

// Minimal Project rows for hydrate(); only id/name are exercised by these tests.
const project = (id: string): Project => ({
  id,
  name: id.toUpperCase(),
  enabled: true,
  repo: "owner/repo",
  repoRoot: "/tmp/repo",
  pollIntervalSecs: 60,
  authors: [],
  labelSource: "native",
  skillRelPath: ".claude/skills/pr-review",
  prCooldownSeconds: 0,
  updateMode: "webhook-only",
  sourceKind: "github",
  azureOrg: "",
  azureProject: "",
  bitbucketHost: "",
  bitbucketProject: "",
  bitbucketToken: "",
  engineKind: "codex",
  codexModel: "",
  claudeModel: "",
});

beforeEach(() => {
  setActivePinia(createPinia());
  prsCb = null;
  vi.clearAllMocks();
  // Restore default resolved behavior wiped by clearAllMocks.
  vi.mocked(api.pollNow).mockResolvedValue(undefined);
  vi.mocked(api.startPolling).mockResolvedValue(undefined);
  vi.mocked(api.stopPolling).mockResolvedValue(undefined);
  vi.mocked(api.getPrs).mockResolvedValue([]);
  vi.mocked(api.setPrArchived).mockResolvedValue(undefined);
  // Restore the pollStatus default wiped by clearAllMocks (#66 F8).
  vi.mocked(api.pollStatus).mockResolvedValue(pollStatus());
  vi.mocked(api.onPrsUpdated).mockImplementation((cb) => {
    prsCb = cb;
    return Promise.resolve(() => {});
  });
  // Seed two projects with "p1" active; module-level refs persist across tests,
  // so hydrate() re-establishes a clean baseline each run.
  useProjects().hydrate({
    projects: [project("p1"), project("p2")],
    activeProjectId: "p1",
    webhookEnabled: false,
    webhookPort: 0,
    webhookSecret: "",
    cloudflaredBin: "",
    webhookTunnelMode: "quick",
    webhookTunnelCommand: "",
    webhookPublicUrl: "",
    localApiToken: "",
    outbox: { notificationTtlSecs: 7200 },
    notifications: { channels: [] },
    messaging: { integrations: [] },
    remoteAccess: { entrypoints: [], tunnels: [] },
    rules: [],
  });
});

describe("usePrStore subscribe()", () => {
  it("applies an `updated` event: sets prs, lastPulledAt, clears error + loading for that project", () => {
    const store = usePrStore();
    store.subscribe();
    store.error.p1 = "stale";
    store.loading.p1 = true;

    const prs = [view(1), view(2)];
    prsCb?.({ kind: "updated", projectId: "p1", prs });

    expect(store.prs.p1).toEqual(prs);
    expect(store.lastPulledAt.p1).not.toBeNull();
    expect(store.error.p1).toBeNull();
    expect(store.loading.p1).toBe(false);
    // Active project's updated PRs do NOT raise the new-PR flag.
    expect(store.hasNewPr.p1).toBeFalsy();
  });

  it("applies an `error` event: sets error, clears loading for that project", () => {
    const store = usePrStore();
    store.subscribe();
    store.loading.p1 = true;

    prsCb?.({ kind: "error", projectId: "p1", message: "gh exploded" });

    expect(store.error.p1).toBe("gh exploded");
    expect(store.loading.p1).toBe(false);
  });

  it("routes a non-active project's event into its own partition without disturbing the active one", () => {
    const store = usePrStore();
    store.subscribe();
    const p1prs = [view(1)];
    prsCb?.({ kind: "updated", projectId: "p1", prs: p1prs });

    const p2prs = [view(10), view(11)];
    prsCb?.({ kind: "updated", projectId: "p2", prs: p2prs });

    expect(store.prs.p2).toEqual(p2prs);
    // p1 (active) partition untouched by the p2 event.
    expect(store.prs.p1).toEqual(p1prs);
  });

  it("flips hasNewPr for a NON-active project that gains a new PR number", () => {
    const store = usePrStore();
    store.subscribe();

    // First p2 update establishes a baseline (#10) — already counts as new since
    // p2 held nothing before; the flag flips on the first unseen number.
    prsCb?.({ kind: "updated", projectId: "p2", prs: [view(10)] });
    expect(store.hasNewPr.p2).toBe(true);
  });

  it("does NOT flip hasNewPr when a non-active project's update brings no new numbers", () => {
    const store = usePrStore();
    store.subscribe();

    // Seed p2 then clear the flag, mimicking the user having visited p2.
    prsCb?.({ kind: "updated", projectId: "p2", prs: [view(10)] });
    store.hasNewPr.p2 = false;

    // A re-poll of the SAME numbers must not re-flag.
    prsCb?.({ kind: "updated", projectId: "p2", prs: [view(10)] });
    expect(store.hasNewPr.p2).toBe(false);
  });

  it("refreshes the backend poll status for the event's project (#66 F8)", () => {
    const store = usePrStore();
    store.subscribe();

    prsCb?.({ kind: "updated", projectId: "p1", prs: [view(1)] });
    expect(api.pollStatus).toHaveBeenCalledWith("p1");

    // The error branch must refresh too, so a running-but-failing loop still surfaces.
    prsCb?.({ kind: "error", projectId: "p2", message: "boom" });
    expect(api.pollStatus).toHaveBeenCalledWith("p2");
  });
});

describe("usePrStore pollNow()", () => {
  it("on success leaves loading=true for the project (event clears it later)", async () => {
    const store = usePrStore();
    await store.pollNow("p1");

    expect(api.pollNow).toHaveBeenCalledWith("p1");
    expect(store.loading.p1).toBe(true);
    expect(store.error.p1).toBeNull();
  });

  it("on a rejected invoke sets error and clears loading", async () => {
    vi.mocked(api.pollNow).mockRejectedValueOnce({ message: "boom" });
    const store = usePrStore();
    await store.pollNow("p1");

    expect(store.error.p1).toBe("boom");
    expect(store.loading.p1).toBe(false);
  });
});

describe("usePrStore toggle()", () => {
  it("polling -> paused calls stopPolling and flips polling=false for all projects", async () => {
    // Backend confirms the loop stopped, so the F4 reconciliation in the trailing
    // refreshPollStatus agrees with the optimistic flip (default mock would report
    // running:true and bounce p1 back, masking the optimistic stop under test).
    vi.mocked(api.pollStatus).mockResolvedValue(pollStatus({ running: false }));
    const store = usePrStore();
    expect(store.pollingActive).toBe(true);

    await store.toggle();
    // Let the fire-and-forget refreshPollStatus(activeId) settle so its reconciliation
    // (p1 -> running:false) is reflected before asserting.
    await Promise.resolve();

    expect(api.stopPolling).toHaveBeenCalledOnce();
    expect(store.pollingFor("p1")).toBe(false);
    expect(store.pollingFor("p2")).toBe(false);
    expect(store.errorActive).toBeNull();
    // Reflects the start/stop in the active project's backend diagnostics (#66 F8).
    expect(api.pollStatus).toHaveBeenCalledWith("p1");
  });

  it("paused -> resume calls startPolling and sets polling per eligibility (#150 F1b)", async () => {
    // Mixed modes/enabled: only an enabled pull/hybrid project is poll-eligible, so the
    // resume branch must NOT blanket-true webhook-only / disabled projects.
    useProjects().hydrate({
      projects: [
        { ...project("p1"), updateMode: "pull-only" }, // enabled + pull → eligible
        { ...project("p2"), updateMode: "webhook-only" }, // mode-ineligible
        { ...project("p3"), updateMode: "hybrid", enabled: false }, // disabled
      ],
      activeProjectId: "p1",
      webhookEnabled: false,
      webhookPort: 0,
      webhookSecret: "",
      cloudflaredBin: "",
      webhookTunnelMode: "quick",
      webhookTunnelCommand: "",
      webhookPublicUrl: "",
      localApiToken: "",
      outbox: { notificationTtlSecs: 7200 },
      notifications: { channels: [] },
      messaging: { integrations: [] },
      remoteAccess: { entrypoints: [], tunnels: [] },
      rules: [],
    });
    const store = usePrStore();
    // Start the active project paused so toggle() takes the resume branch.
    store.polling.p1 = false;

    await store.toggle();

    expect(api.startPolling).toHaveBeenCalledOnce();
    expect(store.pollingFor("p1")).toBe(true); // enabled + pull-only
    expect(store.pollingFor("p2")).toBe(false); // webhook-only → no loop
    expect(store.pollingFor("p3")).toBe(false); // disabled → no loop
  });

  it("on a rejected command sets error and does NOT flip polling", async () => {
    vi.mocked(api.stopPolling).mockRejectedValueOnce({ message: "stop failed" });
    const store = usePrStore();
    expect(store.pollingActive).toBe(true);

    await store.toggle();

    // catch runs before the flip, so polling stays at its prior (default) value.
    expect(store.errorActive).toBe("stop failed");
    expect(store.pollingActive).toBe(true);
  });
});

describe("usePrStore loadSnapshot()", () => {
  it("sets prs from getPrs for the project", async () => {
    const snapshot = [view(7)];
    vi.mocked(api.getPrs).mockResolvedValueOnce(snapshot);
    const store = usePrStore();

    await store.loadSnapshot("p1");

    expect(api.getPrs).toHaveBeenCalledWith("p1");
    expect(store.prs.p1).toEqual(snapshot);
  });

  it("guards a second load of the same project (visited-once)", async () => {
    vi.mocked(api.getPrs).mockResolvedValue([view(7)]);
    const store = usePrStore();

    await store.loadSnapshot("p1");
    await store.loadSnapshot("p1");

    expect(api.getPrs).toHaveBeenCalledOnce();
  });

  it("surfaces a rejected getPrs as error without mutating prs (and resets the guard)", async () => {
    vi.mocked(api.getPrs).mockRejectedValueOnce({ message: "no snapshot" });
    const store = usePrStore();

    await store.loadSnapshot("p1");

    expect(store.error.p1).toBe("no snapshot");
    expect(store.prs.p1).toBeUndefined();
    // Guard reset so a retry can re-fetch.
    vi.mocked(api.getPrs).mockResolvedValueOnce([view(7)]);
    await store.loadSnapshot("p1");
    expect(store.prs.p1).toEqual([view(7)]);
  });
});

describe("usePrStore init()", () => {
  it("subscribes before reading the active project's snapshot baseline", async () => {
    const snapshot = [view(3)];
    vi.mocked(api.getPrs).mockResolvedValueOnce(snapshot);
    const store = usePrStore();

    const unlisten = await store.init();

    expect(api.onPrsUpdated).toHaveBeenCalledOnce();
    expect(api.getPrs).toHaveBeenCalledWith("p1");
    expect(store.prs.p1).toEqual(snapshot);
    expect(typeof (await unlisten)).toBe("function");
    // Baselines the active project's poll-loop diagnostics (#66 F8).
    expect(api.pollStatus).toHaveBeenCalledWith("p1");
  });

  it("registers the listener BEFORE reading the snapshot (#27 F3 race guard)", async () => {
    // Capture the actual call order: a snapshot read that landed first would let a
    // `prs:updated` event fired in the gap go unheard. The guard is the await on the
    // listener registration inside init() — assert it observably precedes getPrs.
    const order: string[] = [];
    vi.mocked(api.onPrsUpdated).mockImplementation((cb) => {
      order.push("subscribe");
      prsCb = cb;
      return Promise.resolve(() => {});
    });
    vi.mocked(api.getPrs).mockImplementation(() => {
      order.push("snapshot");
      return Promise.resolve([]);
    });
    const store = usePrStore();

    await store.init();

    expect(order).toEqual(["subscribe", "snapshot"]);
  });
});

describe("usePrStore switchTo()", () => {
  it("activates the project, clears its new-PR flag, and baselines its list", async () => {
    const snapshot = [view(10)];
    vi.mocked(api.getPrs).mockResolvedValueOnce(snapshot);
    const store = usePrStore();
    store.hasNewPr.p2 = true;

    await store.switchTo("p2");

    expect(useProjects().activeProjectId.value).toBe("p2");
    expect(store.hasNewPr.p2).toBe(false);
    expect(api.getPrs).toHaveBeenCalledWith("p2");
    expect(store.prs.p2).toEqual(snapshot);
    // Refreshes the switched-to project's poll diagnostics immediately (#66 F5/F8).
    expect(api.pollStatus).toHaveBeenCalledWith("p2");
  });
});

describe("usePrStore tracking getters (#38, scoped #35)", () => {
  // One PR of each tracking class, fed through the same `subscribe` path the
  // backend's `prs:updated` push uses, so the getters partition real store state.
  it("partition the ACTIVE project's retained list into current / stale / archived", () => {
    const store = usePrStore();
    store.subscribe();

    const current = view(1, { presence: "current", archived: false });
    const stale = view(2, { presence: "stale", archived: false });
    const archivedCurrent = view(3, { presence: "current", archived: true });
    const archivedStale = view(4, { presence: "stale", archived: true });
    prsCb?.({
      kind: "updated",
      projectId: "p1",
      prs: [current, stale, archivedCurrent, archivedStale],
    });

    // The prop-free active wrappers resolve the "p1" partition.
    expect(store.currentPrs).toEqual([current]);
    expect(store.stalePrs).toEqual([stale]);
    expect(store.archivedPrs).toEqual([archivedCurrent, archivedStale]);
    // Parameterized getters agree.
    expect(store.currentPrsFor("p1")).toEqual([current]);
  });

  it("an empty/unknown project partition resolves to empty getters", () => {
    const store = usePrStore();
    expect(store.currentPrsFor("nope")).toEqual([]);
    expect(store.stalePrsFor("nope")).toEqual([]);
    expect(store.archivedPrsFor("nope")).toEqual([]);
  });

  it("keep an archived+current PR out of currentPrs (archived wins)", () => {
    const store = usePrStore();
    store.subscribe();

    const archivedCurrent = view(5, { presence: "current", archived: true });
    prsCb?.({ kind: "updated", projectId: "p1", prs: [archivedCurrent] });

    expect(store.currentPrs).toEqual([]);
    expect(store.archivedPrs).toEqual([archivedCurrent]);
  });
});

describe("usePrStore setArchived()", () => {
  it("invokes setPrArchived with {projectId, number, archived}", async () => {
    const store = usePrStore();

    await store.setArchived("p1", 42, true);

    expect(api.setPrArchived).toHaveBeenCalledWith("p1", 42, true);
    expect(store.error.p1).toBeUndefined();
  });

  it("surfaces a rejected invoke as error on that project", async () => {
    vi.mocked(api.setPrArchived).mockRejectedValueOnce({ message: "archive failed" });
    const store = usePrStore();

    await store.setArchived("p1", 7, false);

    expect(store.error.p1).toBe("archive failed");
  });
});

describe("usePrStore refreshPollStatus() (#62, #66 F4/F8)", () => {
  it("writes pollStatus and reconciles the optimistic polling flag to backend running", async () => {
    // Backend reports the loop STOPPED — the optimistic flag (default true) must
    // reconcile to false so the pause/resume button reads "恢复轮询" (#66 F4).
    const status = pollStatus({ running: false, lastDiscoveredCount: 3 });
    vi.mocked(api.pollStatus).mockResolvedValueOnce(status);
    const store = usePrStore();
    expect(store.pollingFor("p1")).toBe(true);

    await store.refreshPollStatus("p1");

    expect(api.pollStatus).toHaveBeenCalledWith("p1");
    expect(store.pollStatus.p1).toEqual(status);
    expect(store.pollingFor("p1")).toBe(false);
  });

  it("leaves the prior pollStatus AND polling flag unchanged on a rejected fetch (error swallowed)", async () => {
    const store = usePrStore();
    // Seed a prior good status + a known optimistic flag.
    const prior = pollStatus({ running: true, lastDiscoveredCount: 1 });
    store.pollStatus.p1 = prior;
    store.polling.p1 = true;

    vi.mocked(api.pollStatus).mockRejectedValueOnce({ message: "status failed" });
    await store.refreshPollStatus("p1");

    // Rejected fetch: no error banner, no mutation of either field.
    expect(store.pollStatus.p1).toEqual(prior);
    expect(store.polling.p1).toBe(true);
    expect(store.error.p1).toBeUndefined();
  });
});
