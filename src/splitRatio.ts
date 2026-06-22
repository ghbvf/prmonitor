// Pure resize math for SplitPane's vertical split (review / 会话清单). DOM-free on
// purpose: the position/clamping contract is unit-tested here (splitRatio.test.ts),
// while SplitPane.vue keeps only the pointer wiring — mirrors the repo's convention of
// testing pure .ts logic (no @vue/test-utils dep for component tests).

// Top pane's (会话清单) default share of the split height; the bottom pane (review +
// the chat composer) takes the rest (≈90%). Deliberately small: the session list is a
// compact picker, the review/chat stream is the focus.
export const DEFAULT_TOP_RATIO = 0.1;

// Neither pane may shrink below this fraction of the container, so a drag to the edge
// leaves both panes (and their scrollbars) usable instead of collapsing one to a sliver.
// Set equal to DEFAULT_TOP_RATIO so the 10% default is the floor (the session list can
// only be dragged larger, never below its default) and is not clamped up on mount.
export const MIN_RATIO = 0.1;

// Normalize a caller-supplied minRatio into the valid floor band [0, 0.5]: a value with
// no room for both panes (>= 0.5) collapses to 0.5 (50/50), and non-finite input
// (NaN / ±Infinity) routes back to the MIN_RATIO default instead of escaping the funnel
// and yielding an out-of-range split. Single source shared by clampRatio AND SplitPane's
// ARIA range, so the real and advertised bounds can't drift apart.
export function normalizeMinRatio(minRatio: number): number {
  if (!Number.isFinite(minRatio)) return MIN_RATIO;
  return Math.min(Math.max(minRatio, 0), 0.5);
}

// Clamp a top-pane ratio into the usable band [lo, 1 - lo], where lo = normalizeMinRatio.
// NaN ratio (bad input) falls back to the smallest valid top rather than propagating into
// a style value.
export function clampRatio(ratio: number, minRatio: number): number {
  const lo = normalizeMinRatio(minRatio);
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
