<script setup lang="ts">
// The ONLY component that touches xterm (#1383). It owns the `Terminal` instance (kept OUT
// of reactive state — see useTerminalStore) and bridges it to the store via the screen sink:
// the store pushes each rendered frame here, and this pane forwards keystrokes / resizes back.
// Lazy-loaded by TerminalView via defineAsyncComponent so the heavy xterm chunk (and its
// DOM-only code) never lands in the eager bundle.
import { onMounted, onUnmounted, ref } from "vue";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { useTerminalStore } from "./useTerminalStore";

const store = useTerminalStore();
const host = ref<HTMLDivElement | null>(null);

// Non-reactive locals: the Terminal / addon / observer are imperative DOM resources, not
// state — keeping them off `ref` avoids Vue deep-tracking xterm's internal buffers.
let term: Terminal | null = null;
let fit: FitAddon | null = null;
let ro: ResizeObserver | null = null;

onMounted(() => {
  if (!host.value) return;
  term = new Terminal({
    convertEol: true,
    cursorBlink: true,
    fontFamily: "'SF Mono', Menlo, Monaco, 'Courier New', monospace",
  });
  fit = new FitAddon();
  term.loadAddon(fit);
  term.open(host.value);
  fit.fit();

  // Two render paths by backend (#1372). `writeFrame` (iTerm): a FULL visible-screen snapshot —
  // redraw the whole grid (reset + write) then reposition the cursor when the frame carries one.
  // `writeRaw` (webPty): incremental raw PTY bytes — a plain `term.write` (no reset; xterm v6's
  // `write` accepts a Uint8Array and runs its own UTF-8 decoder). registerScreenSink replays the
  // last frame / buffered raw chunks immediately, so a remount restores the screen without waiting.
  store.registerScreenSink({
    writeFrame({ data, cursorRow, cursorCol }) {
      term?.reset();
      term?.write(data);
      if (cursorRow != null) {
        // CUP (Cursor Position) is 1-based; the wire cursor is 0-based.
        term?.write(`\x1b[${cursorRow + 1};${(cursorCol ?? 0) + 1}H`);
      }
    },
    writeRaw(bytes) {
      term?.write(bytes);
    },
  });

  // Forward every keystroke / paste to the focused session (guarded in the store).
  term.onData((d) => void store.sendInput(d));

  // Keep the session grid in lockstep with the pane size.
  ro = new ResizeObserver(() => {
    fit?.fit();
    if (term) void store.resize(term.cols, term.rows);
  });
  ro.observe(host.value);
});

onUnmounted(() => {
  store.unregisterScreenSink();
  ro?.disconnect();
  ro = null;
  term?.dispose();
  term = null;
  fit = null;
});
</script>

<template>
  <div ref="host" class="xterm-host" />
</template>

<style scoped>
.xterm-host {
  width: 100%;
  height: 100%;
  min-height: 0;
}
</style>
