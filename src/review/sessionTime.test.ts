import { describe, expect, it } from "vitest";
import { formatStartedAt } from "./sessionTime";

describe("formatStartedAt", () => {
  it("renders nothing for the 0 fallback stamp", () => {
    expect(formatStartedAt(0)).toBe("");
  });

  it("renders nothing for a negative stamp (signed-wire guard)", () => {
    expect(formatStartedAt(-1)).toBe("");
  });

  it("formats an epoch-SECONDS stamp to a non-empty local string", () => {
    // 1_700_000_000s = 2023-11-14. We scale by 1000 to ms, so the formatted string
    // must reflect 2023 — a dropped `* 1000` would format ~1970 instead. Comparing
    // against `getFullYear()` (always Gregorian) keeps this locale-independent.
    const out = formatStartedAt(1_700_000_000);
    expect(out).not.toBe("");
    expect(out).toContain(String(new Date(1_700_000_000 * 1000).getFullYear()));
  });
});
