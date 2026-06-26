<script setup lang="ts">
// Touch toolbar for the special keys a soft keyboard can't type (#1383). PURE emit: it has
// NO transport / store dependency — every key maps through the pure `keymap` and is emitted
// as a `send` event — so PR2's browser build can reuse this component unchanged.
import type { TerminalKey } from "./keymap";
import { toEscapeSequence } from "./keymap";

const emit = defineEmits<{ (e: "send", data: string): void }>();

interface KeyButton {
  key: TerminalKey;
  label: string;
  aria: string;
}

// Every key emits a concrete escape sequence via `press`. The common Ctrl chords are explicit
// ^C/^D/^Z/^L buttons. NOTE: there is intentionally NO sticky "Ctrl" modifier here — a
// general Ctrl+<letter> combine for soft-keyboard input is deferred to PR2 (it needs store
// coordination to intercept the next typed letter, which would break this component's
// pure-emit, transport-free contract). A togglable-but-inert "Ctrl" button would be a
// misleading affordance, so it is omitted rather than faked.
const keys: KeyButton[] = [
  { key: "esc", label: "Esc", aria: "Escape" },
  { key: "tab", label: "Tab", aria: "Tab" },
  { key: "up", label: "↑", aria: "Arrow up" },
  { key: "down", label: "↓", aria: "Arrow down" },
  { key: "left", label: "←", aria: "Arrow left" },
  { key: "right", label: "→", aria: "Arrow right" },
  { key: "ctrlC", label: "^C", aria: "Control C" },
  { key: "ctrlD", label: "^D", aria: "Control D" },
  { key: "ctrlZ", label: "^Z", aria: "Control Z" },
  { key: "ctrlL", label: "^L", aria: "Control L" },
  { key: "enter", label: "⏎", aria: "Enter" },
  { key: "home", label: "Home", aria: "Home" },
  { key: "end", label: "End", aria: "End" },
  { key: "pageUp", label: "PgUp", aria: "Page up" },
  { key: "pageDown", label: "PgDn", aria: "Page down" },
];

function press(key: TerminalKey) {
  emit("send", toEscapeSequence(key));
}
</script>

<template>
  <div class="mobile-toolbar" role="toolbar" aria-label="终端按键 / terminal keys">
    <button
      v-for="k in keys"
      :key="k.key"
      type="button"
      class="key"
      :aria-label="k.aria"
      @click="press(k.key)"
    >
      {{ k.label }}
    </button>
  </div>
</template>

<style scoped>
.mobile-toolbar {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
  padding: var(--space-3);
  border-top: 1px solid var(--color-border-strong);
  background: var(--color-surface);
}
.key {
  /* Touch-first sizing: each target meets the ~44px minimum touch dimension. */
  min-width: 44px;
  min-height: 44px;
  padding: var(--space-2) var(--space-3);
  font: inherit;
  font-family: var(--font-mono);
  font-size: var(--font-size-sm);
  color: var(--color-text);
  background: var(--color-bg);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.key:active {
  background: var(--color-surface-hover);
}
</style>
