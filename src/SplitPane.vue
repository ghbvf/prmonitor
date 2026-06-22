<script setup lang="ts">
// Vertical split with a draggable horizontal divider: the `top` slot's height vs the
// `bottom` slot's is driven by a ratio, and each pane scrolls independently. Pure
// presentation — the ratio is in-memory only and resets to the default on reload (by
// design; no persistence). Lives at src/ root with the other composition-level UI
// (StatusBar.vue), so App.vue stays the cross-slice arranger while the pointer-drag
// interaction + its scoped styles are isolated here. The resize math is the
// unit-tested splitRatio.ts, leaving this file as just pointer wiring + layout.
import { ref } from "vue";
import {
  clampRatio,
  DEFAULT_TOP_RATIO,
  MIN_RATIO,
  ratioFromPointer,
} from "./splitRatio";

// `initialTopRatio` only SEEDS the ratio at setup — later prop changes do not re-drive
// the split (the ratio is user-controlled after mount, by design). `minRatio` bounds how
// small either pane may get.
const props = withDefaults(
  defineProps<{ initialTopRatio?: number; minRatio?: number }>(),
  { initialTopRatio: DEFAULT_TOP_RATIO, minRatio: MIN_RATIO },
);

const rootEl = ref<HTMLElement | null>(null);
const topRatio = ref(clampRatio(props.initialTopRatio, props.minRatio));
const dragging = ref(false);

// One arrow press nudges the split by 2% (a11y: the separator is keyboard-operable).
const KEY_STEP = 0.02;

function onPointerDown(e: PointerEvent) {
  dragging.value = true;
  // Capture so pointermove/up keep routing to the divider even when the pointer leaves
  // it mid-drag — no document-level listeners to register and clean up.
  (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
}

function onPointerMove(e: PointerEvent) {
  if (!dragging.value || !rootEl.value) return;
  const rect = rootEl.value.getBoundingClientRect();
  topRatio.value = ratioFromPointer(
    e.clientY,
    rect.top,
    rect.height,
    props.minRatio,
  );
}

function endDrag() {
  dragging.value = false;
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === "ArrowUp") {
    topRatio.value = clampRatio(topRatio.value - KEY_STEP, props.minRatio);
    e.preventDefault();
  } else if (e.key === "ArrowDown") {
    topRatio.value = clampRatio(topRatio.value + KEY_STEP, props.minRatio);
    e.preventDefault();
  } else if (e.key === "Home") {
    topRatio.value = clampRatio(0, props.minRatio); // smallest top pane
    e.preventDefault();
  } else if (e.key === "End") {
    topRatio.value = clampRatio(1, props.minRatio); // largest top pane
    e.preventDefault();
  }
}
</script>

<template>
  <div ref="rootEl" class="split" :class="{ dragging }">
    <div class="pane" :style="{ flexGrow: topRatio }">
      <slot name="top" />
    </div>
    <div
      class="divider"
      role="separator"
      aria-orientation="horizontal"
      aria-label="调整面板高度 / Resize panes"
      :aria-valuenow="Math.round(topRatio * 100)"
      aria-valuemin="0"
      aria-valuemax="100"
      tabindex="0"
      @pointerdown="onPointerDown"
      @pointermove="onPointerMove"
      @pointerup="endDrag"
      @lostpointercapture="endDrag"
      @keydown="onKeydown"
    ></div>
    <div class="pane" :style="{ flexGrow: 1 - topRatio }">
      <slot name="bottom" />
    </div>
  </div>
</template>

<style scoped>
.split {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-height: 0;
}
/* Each pane owns its scroll. flex-basis:0 + the flex-grow ratios split the height purely
   by ratio (content size doesn't bias it); min-height:0 lets a flex child actually
   shrink so overflow scrolls instead of forcing the pane taller. */
.pane {
  flex-basis: 0;
  min-height: 0;
  overflow-y: auto;
  /* Small vertical inset so pane content doesn't sit flush against the divider. */
  padding: var(--space-2) 0;
}
.divider {
  flex: none;
  height: var(--space-3);
  cursor: row-resize;
  background: var(--color-border);
  /* Stop touch devices (touchscreen Windows) from turning a drag into a scroll gesture
     that would steal the pointer capture mid-resize. */
  touch-action: none;
}
.divider:hover,
.split.dragging .divider {
  background: var(--color-border-strong);
}
.divider:focus-visible {
  outline: 2px solid var(--color-accent);
  outline-offset: -1px;
}
/* During a drag, suppress text selection across both panes. */
.split.dragging {
  user-select: none;
}
</style>
