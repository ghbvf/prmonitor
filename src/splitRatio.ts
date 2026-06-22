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

// Clamp a top-pane ratio into the usable band [minRatio, 1 - minRatio]. NaN (bad input)
// falls back to the smallest valid top rather than propagating into a style value.
export function clampRatio(ratio: number, minRatio: number): number {
  const hi = 1 - minRatio;
  if (Number.isNaN(ratio) || ratio < minRatio) return minRatio;
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
  if (rectHeight <= 0) return minRatio;
  return clampRatio((clientY - rectTop) / rectHeight, minRatio);
}
