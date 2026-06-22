import { describe, expect, it } from "vitest";

import {
  clampRatio,
  DEFAULT_TOP_RATIO,
  MIN_RATIO,
  ratioFromPointer,
} from "./splitRatio";

// Pure resize math for SplitPane's vertical split (review / 会话清单). DOM-free, so it
// exercises the clamping/position contract without a Vue/DOM runtime — mirrors the
// repo's .ts-only test convention (see projects.test.ts; no @vue/test-utils dep).

describe("splitRatio constants", () => {
  it("default top ratio sits inside the usable band", () => {
    expect(DEFAULT_TOP_RATIO).toBeGreaterThanOrEqual(MIN_RATIO);
    expect(DEFAULT_TOP_RATIO).toBeLessThanOrEqual(1 - MIN_RATIO);
  });
});

describe("clampRatio", () => {
  it("passes through a value already in band", () => {
    expect(clampRatio(0.5, 0.15)).toBe(0.5);
  });

  it("clamps below the minimum up to minRatio", () => {
    expect(clampRatio(0.05, 0.15)).toBe(0.15);
  });

  it("clamps above the maximum down to 1 - minRatio", () => {
    expect(clampRatio(0.99, 0.15)).toBeCloseTo(0.85);
  });

  it("treats the band edges as in-band", () => {
    expect(clampRatio(0.15, 0.15)).toBe(0.15);
    expect(clampRatio(0.85, 0.15)).toBeCloseTo(0.85);
  });

  it("falls back to minRatio on NaN", () => {
    expect(clampRatio(Number.NaN, 0.15)).toBe(0.15);
  });

  it("collapses an unusable minRatio (>= 0.5) to the 50/50 midpoint", () => {
    // Band would invert (lo > hi) — normalize minRatio to 0.5 so both edges meet at 0.5.
    expect(clampRatio(0.7, 0.6)).toBe(0.5);
    expect(clampRatio(0.3, 0.6)).toBe(0.5);
    expect(clampRatio(0.5, 0.6)).toBe(0.5);
  });

  it("treats a negative minRatio as 0 (full range usable)", () => {
    expect(clampRatio(0.5, -1)).toBe(0.5);
    expect(clampRatio(0.01, -1)).toBe(0.01);
  });
});

describe("ratioFromPointer", () => {
  it("maps a mid-container pointer to ~0.5", () => {
    expect(ratioFromPointer(150, 100, 100, 0.15)).toBeCloseTo(0.5);
  });

  it("clamps a pointer above the container to minRatio", () => {
    // clientY at/above rectTop → raw <= 0 → clamped up.
    expect(ratioFromPointer(100, 100, 100, 0.15)).toBe(0.15);
    expect(ratioFromPointer(50, 100, 100, 0.15)).toBe(0.15);
  });

  it("clamps a pointer below the container to 1 - minRatio", () => {
    expect(ratioFromPointer(300, 100, 100, 0.15)).toBeCloseTo(0.85);
  });

  it("returns a finite fallback (no NaN) when the container has no height", () => {
    const r = ratioFromPointer(150, 100, 0, 0.15);
    expect(Number.isFinite(r)).toBe(true);
    expect(r).toBe(0.15);
  });

  it("returns a finite fallback when the container height is negative", () => {
    const r = ratioFromPointer(150, 100, -10, 0.15);
    expect(Number.isFinite(r)).toBe(true);
    expect(r).toBe(0.15);
  });
});
