// Locks the poll-eligibility helpers (#150 F1b): the frontend's "is this project's loop
// running / does it get poll actions" judgment must fold `enabled` together with the
// mode, mirroring the backend `periodic_polling` gate. A regression to a mode-only check
// would show disabled projects as monitored / actionable.
import { describe, it, expect } from "vitest";
import { periodicPollEligible, manualPullEligible } from "./types";

describe("periodicPollEligible", () => {
  it("is true only when enabled AND the mode runs the periodic loop (pull/hybrid)", () => {
    expect(periodicPollEligible(true, "pull-only")).toBe(true);
    expect(periodicPollEligible(true, "hybrid")).toBe(true);
    expect(periodicPollEligible(true, "webhook-only")).toBe(false);
    expect(periodicPollEligible(true, "manual")).toBe(false);
  });

  it("is false for a disabled project regardless of mode", () => {
    expect(periodicPollEligible(false, "pull-only")).toBe(false);
    expect(periodicPollEligible(false, "hybrid")).toBe(false);
    expect(periodicPollEligible(false, "webhook-only")).toBe(false);
    expect(periodicPollEligible(false, "manual")).toBe(false);
  });
});

describe("manualPullEligible", () => {
  it("is true only when enabled AND the mode allows on-demand pull (not webhook-only)", () => {
    expect(manualPullEligible(true, "pull-only")).toBe(true);
    expect(manualPullEligible(true, "hybrid")).toBe(true);
    expect(manualPullEligible(true, "manual")).toBe(true);
    expect(manualPullEligible(true, "webhook-only")).toBe(false);
  });

  it("is false for a disabled project regardless of mode", () => {
    expect(manualPullEligible(false, "pull-only")).toBe(false);
    expect(manualPullEligible(false, "manual")).toBe(false);
    expect(manualPullEligible(false, "webhook-only")).toBe(false);
  });
});
