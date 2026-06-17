<script setup lang="ts">
// Project switcher rail (#35): the leftmost ~200px column listing every monitored
// project. Each row shows the project name, a status dot, and an active-PR count
// badge; clicking a row makes that project active (persists the selection,
// baselines its PR list, clears its new-PR flag). Reads `useProjects()` for the
// list + active selection and `usePrStore()` for per-project status. Prop-free:
// App.vue mounts it without wiring.
import { usePrStore } from "./usePrStore";
import { useProjects } from "../projects";

const store = usePrStore();
const { projects, activeProjectId } = useProjects();

// Status dot intent per project: a project with an unseen PR (a non-active one
// that gained a PR while in the background) warns; otherwise it tracks whether the
// loop is running (success) or paused (muted). has-new takes priority so the
// badge is not lost behind the paused state.
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
</script>

<template>
  <nav class="project-switcher" aria-label="Projects">
    <ul class="rows">
      <li
        v-for="project in projects"
        :key="project.id"
        class="project-row"
        :class="{ active: project.id === activeProjectId }"
        @click="store.switchTo(project.id)"
      >
        <span
          class="dot"
          :class="dotIntent(project.id)"
          :title="dotTitle(dotIntent(project.id))"
        ></span>
        <span class="name" :title="project.name">{{ project.name }}</span>
        <span v-if="prCount(project.id) > 0" class="badge">
          {{ prCount(project.id) }}
        </span>
      </li>
    </ul>
  </nav>
</template>

<style scoped>
.project-switcher {
  width: 200px;
  flex: none;
  border-right: 1px solid var(--color-border);
  overflow-y: auto;
}
.rows {
  list-style: none;
  margin: 0;
  padding: var(--space-3);
}
.project-row {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3) var(--space-4);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.project-row:hover {
  background: var(--color-surface-hover);
}
.project-row.active {
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
.name {
  flex: 1;
  min-width: 0;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  font-size: var(--font-size-sm);
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
</style>
