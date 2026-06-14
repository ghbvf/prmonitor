<script setup lang="ts">
// Review streaming panel. PR6 renders streamed deltas + the Stop button.
import { onMounted } from "vue";
import { useReviewStore } from "./useReviewStore";

const store = useReviewStore();

// Hydrate the codex availability on mount so the StatusBar has data to show.
onMounted(() => store.refreshCodexStatus());
</script>

<template>
  <section class="review-panel">
    <h2>Review</h2>
    <p class="muted">Select a PR to review.</p>
  </section>
</template>

<style scoped>
.muted { color: #888; }
</style>
