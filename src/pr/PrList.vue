<script setup lang="ts">
// PR list slice view: the "立即拉取" control + the discovered rows. The store
// owns the fetch/error plumbing; this component only renders its state.
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
      <button type="button" :disabled="store.loading" @click="store.fetchNow()">
        {{ store.loading ? "拉取中…" : "立即拉取" }}
      </button>
    </header>

    <p v-if="store.error" class="error">{{ store.error }}</p>

    <p v-else-if="!store.loading && store.prs.length === 0" class="muted">
      暂无 PR / No PRs
    </p>

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
.pr-head button {
  padding: 4px 8px;
  font-size: 12px;
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
