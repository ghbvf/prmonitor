<script setup lang="ts">
// PR list slice view: renders the discovered rows. The "立即拉取" control lives
// in PollControls; this component only renders the store's PR state. Selection is
// owned by the composition root (App.vue): the row's `select` is forwarded up and
// the currently-selected PR number is passed back down for highlighting.
import { onMounted } from "vue";
import type { PullRequestView } from "../types";
import { usePrStore } from "./usePrStore";
import PrRow from "./PrRow.vue";

defineProps<{ selectedNumber: number | null }>();
const emit = defineEmits<{ select: [pr: PullRequestView] }>();

const store = usePrStore();

// Hydrate the gh CLI status on mount so the StatusBar has data to show.
onMounted(() => store.refreshGhStatus());
</script>

<template>
  <section class="pr-list">
    <header class="pr-head">
      <h2>Pull requests</h2>
    </header>

    <p v-if="store.error" class="error">{{ store.error }}</p>

    <p v-else-if="store.loading && store.prs.length === 0" class="muted">
      拉取中…
    </p>

    <p v-else-if="store.prs.length === 0" class="muted">暂无 PR / No PRs</p>

    <ul v-else class="rows">
      <PrRow
        v-for="pr in store.prs"
        :key="pr.number"
        :pr="pr"
        :selected="pr.number === selectedNumber"
        @select="(p) => emit('select', p)"
      />
    </ul>
  </section>
</template>

<style scoped>
.pr-list {
  margin-bottom: var(--space-8);
}
.pr-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
}
.pr-head h2 {
  margin: 0;
}
.muted {
  color: var(--color-text-muted);
  font-size: var(--font-size-sm);
}
.error {
  color: var(--color-danger);
  font-size: var(--font-size-sm);
}
.rows {
  list-style: none;
  margin: var(--space-4) 0 0;
  padding: 0;
}
</style>
