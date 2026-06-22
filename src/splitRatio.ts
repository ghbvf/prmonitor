// Pure resize math for SplitPane's vertical split (review / 会话清单). DOM-free on
// purpose: the position/clamping contract is unit-tested here (splitRatio.test.ts),
// while SplitPane.vue keeps only the pointer wiring — mirrors the repo's convention of
// testing pure .ts logic (no @vue/test-utils dep for component tests).

// Top pane's (会话清单) default share of the split height; the bottom pane (review)
// takes the rest (≈65%).
export const DEFAULT_TOP_RATIO = 0.35;

// Neither pane may shrink below this fraction of the container, so a drag to the edge
// leaves both panes (and their scrollbars) usable instead of collapsing one to a sliver.
export const MIN_RATIO = 0.15;

// Clamp a top-pane ratio into the usable band [lo, 1 - lo]. NaN (bad input) falls back to
// the smallest valid top rather than propagating into a style value. minRatio is itself
// normalized into [0, 0.5] so a caller passing >= 0.5 (no room for both panes) collapses
// to the 50/50 midpoint instead of an inverted band that returns inconsistent values.
export function clampRatio(ratio: number, minRatio: number): number {
  const lo = Math.min(Math.max(minRatio, 0), 0.5);
  const hi = 1 - lo;
  if (Number.isNaN(ratio) || ratio < lo) return lo;
  if (ratio > hi) return hi;
  return ratio;
}

// Top-pane ratio for a pointer at `clientY`, given the split container's top edge
// (`rectTop`) and `rectHeight`, clamped into the usable band. A non-positive height
// (container not laid out yet) has no meaningful position → fall back to the smallest
// valid top, never NaN/Infinity.
export function ratioFromPointer(
  clientY: number,
  rectTop: number,
  rectHeight: number,
  minRatio: number,
): number {
  // No meaningful position (container not laid out) → smallest valid top, routed through
  // clampRatio so an out-of-range minRatio is normalized here too.
  if (rectHeight <= 0) return clampRatio(0, minRatio);
  return clampRatio((clientY - rectTop) / rectHeight, minRatio);
}
