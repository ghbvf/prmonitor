<script setup lang="ts">
// A single PR row. PR3 adds label badges + review/skip state.
import type { PullRequestView } from "../types";

defineProps<{ pr: PullRequestView }>();
</script>

<template>
  <li class="pr-row">#{{ pr.number }} — {{ pr.title }}</li>
</template>

<style scoped>
.pr-row { list-style: none; padding: 4px 0; }
</style>
