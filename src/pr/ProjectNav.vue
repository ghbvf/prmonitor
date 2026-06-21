<script setup lang="ts">
// Unified project → PR navigation (#67): merges the former ProjectSwitcher rail and the
// PrList sidebar into ONE column. Projects are the top-level grouping (H2); the active
// project expands to its PR list (current / stale / archived sections), so projects and
// PRs are no longer split across two separate sidebars. Selection is owned by the
// composition root (App.vue): a PR row's `select` is forwarded up and the selected PR
// number passed back down for highlighting (keeps pr/review decoupled).
import { computed, ref, watch } from "vue";
import type { TrackedPrView } from "../types";
import { usePrStore } from "./usePrStore";
import { useProjects } from "../projects";
import PrRow from "./PrRow.vue";

defineProps<{ selectedNumber: number | null }>();
const emit = defineEmits<{ select: [pr: TrackedPrView] }>();

const store = usePrStore();
const { projects, activeProjectId } = useProjects();

// ── Project header bits (ported from ProjectSwitcher) ──
// Status dot intent: an unseen-PR project warns; otherwise it tracks running (success)
// vs paused (muted). has-new takes priority so the badge isn't lost behind paused.
type DotIntent = "new" | "monitoring" | "paused";
function dotIntent(id: string): DotIntent {
  if (store.hasNewPr[id]) return "new";
  return store.pollingFor(id) ? "monitoring" : "paused";
}
function dotTitle(intent: DotIntent): string {
  switch (intent) {
    case "new":
      return "有新 PR / new PR";
    case "monitoring":
      return "监控中 / monitoring";
    case "paused":
      return "已暂停 / paused";
  }
}
// Active (not archived, presence "current") PR count for the badge.
function prCount(id: string): number {
  return store.currentPrsFor(id).length;
}

// ── PR sections for the ACTIVE project (ported from PrList) ──
const activePrs = computed(() => store.prs[activeProjectId.value] ?? []);

// Stale section: collapsed by default; when open, window to STALE_LIMIT rows with a
// nested "显示更多 / 显示更少" toggle so a long inactive backlog stays bounded.
const STALE_LIMIT = 10;
const showStale = ref(false);
const staleExpanded = ref(false);
const visibleStale = computed(() =>
  staleExpanded.value ? store.stalePrs : store.stalePrs.slice(0, STALE_LIMIT),
);
function toggleStale() {
  showStale.value = !showStale.value;
  if (!showStale.value) staleExpanded.value = false;
}
const showArchived = ref(false);

// Reset the per-project collapse state when the active project changes, so one
// project's expanded "不活跃 / 已归档" state doesn't bleed into the next.
watch(activeProjectId, () => {
  showStale.value = false;
  staleExpanded.value = false;
  showArchived.value = false;
});

</script>

<template>
  <nav class="project-nav" aria-label="Projects and pull requests">
    <ul class="projects">
      <li v-for="project in projects" :key="project.id" class="project">
        <div
          class="project-header"
          :class="{ active: project.id === activeProjectId }"
          @click="store.switchTo(project.id)"
        >
          <span
            class="dot"
            :class="dotIntent(project.id)"
            :title="dotTitle(dotIntent(project.id))"
          ></span>
          <h2 class="name" :title="project.name">{{ project.name }}</h2>
          <span v-if="prCount(project.id) > 0" class="badge">
            {{ prCount(project.id) }}
          </span>
        </div>

        <!-- The active project expands to its PR list (the Project → PR hierarchy). -->
        <div v-if="project.id === activeProjectId" class="prs">
          <p v-if="store.errorActive" class="error">{{ store.errorActive }}</p>

          <p
            v-else-if="store.loadingActive && activePrs.length === 0"
            class="muted"
          >
            拉取中…
          </p>

          <p v-else-if="activePrs.length === 0" class="muted">
            暂无 PR / No PRs
          </p>

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
                @set-archived="
                  (e) => store.setArchived(activeProjectId, e.number, e.archived)
                "
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
                    @set-archived="
                      (e) =>
                        store.setArchived(activeProjectId, e.number, e.archived)
                    "
                  />
                </ul>
                <button
                  v-if="store.stalePrs.length > STALE_LIMIT"
                  type="button"
                  class="more-toggle muted"
                  @click="staleExpanded = !staleExpanded"
                >
                  {{
                    staleExpanded
                      ? "显示更少"
                      : `显示更多 (${store.stalePrs.length - STALE_LIMIT})`
                  }}
                </button>
              </template>
            </section>

            <section v-if="store.archivedPrs.length" class="section">
              <button
                type="button"
                class="section-toggle"
                @click="showArchived = !showArchived"
              >
                {{ showArchived ? "▾" : "▸" }} 已归档 ({{
                  store.archivedPrs.length
                }})
              </button>
              <!-- Archived rows are NOT review-selectable (App's `selectedPr` excludes
                   archived PRs): pin `:selected` false and drop `@select` so the left
                   highlight can't split from the right review target. -->
              <ul v-if="showArchived" class="rows">
                <PrRow
                  v-for="pr in store.archivedPrs"
                  :key="pr.number"
                  :pr="pr"
                  :selected="false"
                  @set-archived="
                    (e) =>
                      store.setArchived(activeProjectId, e.number, e.archived)
                  "
                />
              </ul>
            </section>
          </template>
        </div>
      </li>
    </ul>
  </nav>
</template>

<style scoped>
.project-nav {
  overflow-y: auto;
}
.projects {
  list-style: none;
  margin: 0;
  padding: 0;
}
.project + .project {
  margin-top: var(--space-3);
}
.project-header {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3) var(--space-4);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.project-header:hover {
  background: var(--color-surface-hover);
}
.project-header.active {
  background: var(--color-accent-bg);
}
.dot {
  flex: none;
  width: 8px;
  height: 8px;
  border-radius: var(--radius-full);
}
.dot.monitoring {
  background: var(--color-success);
}
.dot.paused {
  background: var(--color-text-muted);
}
.dot.new {
  background: var(--color-warn-dot);
}
/* Project title as the first-level grouping heading (#67): larger than a PR row. */
.name {
  flex: 1;
  min-width: 0;
  margin: 0;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  font-size: var(--font-size-lg);
  font-weight: 600;
}
.badge {
  flex: none;
  display: inline-block;
  padding: 1px var(--space-3);
  border-radius: var(--radius-md);
  font-size: var(--font-size-xs);
  line-height: 1.5;
  background: var(--color-accent-badge-bg);
  color: var(--color-accent);
}
/* PRs nested under their project, indented to read as a sub-level. */
.prs {
  padding: var(--space-2) var(--space-3) var(--space-4) var(--space-6);
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
  margin: var(--space-3) 0 0;
  padding: 0;
}
.section {
  margin-top: var(--space-4);
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
