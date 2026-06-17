<script setup lang="ts">
// PR list slice view: renders the discovered rows in three tracking-aware
// sections (#38) — "current" always shown, "stale" (inactive) and "archived"
// folded into collapsible sections. The "立即拉取" control lives in PollControls;
// this component only renders the store's PR state. Selection is owned by the
// composition root (App.vue): the row's `select` is forwarded up and the
// currently-selected PR number is passed back down for highlighting.
import { computed, onMounted, ref } from "vue";
import type { TrackedPrView } from "../types";
import { usePrStore } from "./usePrStore";
import { useProjects } from "../projects";
import PrRow from "./PrRow.vue";

defineProps<{ selectedNumber: number | null }>();
const emit = defineEmits<{ select: [pr: TrackedPrView] }>();

const store = usePrStore();
const { activeProjectId } = useProjects();

// The active project's retained list (#35) — drives the empty/loading states the
// three section getters partition.
const activePrs = computed(() => store.prs[activeProjectId.value] ?? []);

// Stale section: collapsed by default; when open, window to STALE_LIMIT rows with
// a nested "显示更多 / 显示更少" toggle so a long inactive backlog stays bounded.
const STALE_LIMIT = 10;
const showStale = ref(false);
const staleExpanded = ref(false);
const visibleStale = computed(() =>
  staleExpanded.value
    ? store.stalePrs
    : store.stalePrs.slice(0, STALE_LIMIT),
);
// Collapsing the section also resets the window so re-opening starts at the
// STALE_LIMIT view (with its "显示更多" entry), never jumping straight to expanded.
function toggleStale() {
  showStale.value = !showStale.value;
  if (!showStale.value) staleExpanded.value = false;
}

// Archived section: collapsed by default.
const showArchived = ref(false);

// Hydrate the gh CLI status on mount so the StatusBar has data to show.
onMounted(() => store.refreshGhStatus());
</script>

<template>
  <section class="pr-list">
    <header class="pr-head">
      <h2>Pull requests</h2>
    </header>

    <p v-if="store.errorActive" class="error">{{ store.errorActive }}</p>

    <p v-else-if="store.loadingActive && activePrs.length === 0" class="muted">
      拉取中…
    </p>

    <p v-else-if="activePrs.length === 0" class="muted">暂无 PR / No PRs</p>

    <template v-else>
      <p v-if="!store.currentPrs.length" class="muted">
        无活跃 PR（展开下方「不活跃 / 已归档」查看）
      </p>

      <ul v-if="store.currentPrs.length" class="rows">
        <PrRow
          v-for="pr in store.currentPrs"
          :key="pr.number"
          :pr="pr"
          :selected="pr.number === selectedNumber"
          @select="(p) => emit('select', p)"
          @set-archived="(e) => store.setArchived(activeProjectId, e.number, e.archived)"
        />
      </ul>

      <section v-if="store.stalePrs.length" class="section">
        <button type="button" class="section-toggle" @click="toggleStale">
          {{ showStale ? "▾" : "▸" }} 不活跃 ({{ store.stalePrs.length }})
        </button>
        <template v-if="showStale">
          <ul class="rows">
            <PrRow
              v-for="pr in visibleStale"
              :key="pr.number"
              :pr="pr"
              :selected="pr.number === selectedNumber"
              @select="(p) => emit('select', p)"
              @set-archived="(e) => store.setArchived(activeProjectId, e.number, e.archived)"
            />
          </ul>
          <button
            v-if="store.stalePrs.length > STALE_LIMIT"
            type="button"
            class="more-toggle muted"
            @click="staleExpanded = !staleExpanded"
          >
            {{ staleExpanded ? "显示更少" : `显示更多 (${store.stalePrs.length - STALE_LIMIT})` }}
          </button>
        </template>
      </section>

      <section v-if="store.archivedPrs.length" class="section">
        <button
          type="button"
          class="section-toggle"
          @click="showArchived = !showArchived"
        >
          {{ showArchived ? "▾" : "▸" }} 已归档 ({{ store.archivedPrs.length }})
        </button>
        <!-- Archived rows are NOT review-selectable: the review target (App.vue's
             `selectedPr`) excludes archived PRs, so letting an archived row drive
             `select`/highlight would split the left highlight from the right target
             (F3). Pin `:selected` false and drop `@select` here — archiving the
             currently-selected PR thus clears its highlight as the row moves into
             this section, staying consistent with the disabled ReviewPanel. The
             archive/restore + open-link controls (their own `@click.stop`) still work. -->
        <ul v-if="showArchived" class="rows">
          <PrRow
            v-for="pr in store.archivedPrs"
            :key="pr.number"
            :pr="pr"
            :selected="false"
            @set-archived="(e) => store.setArchived(activeProjectId, e.number, e.archived)"
          />
        </ul>
      </section>
    </template>
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
.section {
  margin-top: var(--space-5);
}
.section-toggle {
  display: block;
  width: 100%;
  text-align: left;
  padding: var(--space-2) 0;
  font: inherit;
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
  background: none;
  border: none;
  cursor: pointer;
}
.section-toggle:hover {
  color: var(--color-text);
}
.more-toggle {
  display: block;
  margin-top: var(--space-3);
  padding: var(--space-2) 0;
  font: inherit;
  font-size: var(--font-size-sm);
  background: none;
  border: none;
  cursor: pointer;
}
.more-toggle:hover {
  color: var(--color-text);
}
</style>
