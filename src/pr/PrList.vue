<script setup lang="ts">
// PR list slice view: renders the discovered rows. The "立即拉取" control lives
// in PollControls; this component only renders the store's PR state.
import { onMounted } from "vue";
import { usePrStore } from "./usePrStore";
import PrRow from "./PrRow.vue";

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
      <PrRow v-for="pr in store.prs" :key="pr.number" :pr="pr" />
    </ul>
  </section>
</template>

<style scoped>
.pr-list {
  margin-bottom: 16px;
}
.pr-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}
.pr-head h2 {
  margin: 0;
}
.muted {
  color: #888;
  font-size: 12px;
}
.error {
  color: #c00;
  font-size: 12px;
}
.rows {
  list-style: none;
  margin: 8px 0 0;
  padding: 0;
}
</style>
