<script setup lang="ts">
// Browser-only remote terminal root. The terminal listener intentionally exposes only the
// terminal HTTP surface, so the browser build must not mount the full desktop App shell.
import TerminalView from "./terminal/TerminalView.vue";
</script>

<template>
  <main class="remote-terminal">
    <TerminalView />
  </main>
</template>

<style scoped>
.remote-terminal {
  width: 100vw;
  height: 100vh;
  min-width: 0;
  min-height: 0;
  color: var(--color-text);
  background: var(--color-bg);
}
</style>
