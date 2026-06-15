<script setup lang="ts">
// A single PR row: number/title (click to open in the external browser), the
// label badges, the trigger-label `kind` badge, and a muted "skipped" treatment
// carrying `skipReason` when the PR would not dispatch. Clicking the row selects
// the PR (for the Review panel); the title link opens the browser (`@click.stop`
// so the two actions stay distinct).
import { openUrl } from "@tauri-apps/plugin-opener";
import type { PullRequestView } from "../types";

const props = defineProps<{ pr: PullRequestView; selected: boolean }>();
const emit = defineEmits<{ select: [pr: PullRequestView] }>();

function open() {
  // Only follow web links (gh returns https PR urls); reject any other scheme,
  // and surface a failure instead of dropping it silently.
  const url = props.pr.url;
  if (!/^https?:\/\//i.test(url)) return;
  openUrl(url).catch((err) => console.error("打开链接失败", err));
}
</script>

<template>
  <li
    class="pr-row"
    :class="{ skipped: pr.skipReason != null, selected }"
    @click="emit('select', pr)"
  >
    <div class="title-line">
      <button type="button" class="title" @click.stop="open">
        #{{ pr.number }} — {{ pr.title }}
      </button>
      <span class="badge kind">{{ pr.kind }}</span>
    </div>

    <div v-if="pr.labels.length" class="labels">
      <span v-for="label in pr.labels" :key="label" class="badge label">
        {{ label }}
      </span>
    </div>

    <p v-if="pr.skipReason != null" class="skip-note">
      已跳过：{{ pr.skipReason }}
    </p>
  </li>
</template>

<style scoped>
.pr-row {
  list-style: none;
  padding: var(--space-3) var(--space-4);
  border-bottom: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.pr-row:hover {
  background: var(--color-surface-hover);
}
.pr-row.selected {
  background: var(--color-accent-bg);
}
.pr-row.skipped {
  opacity: 0.55;
}
.title-line {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.title {
  flex: 1;
  text-align: left;
  background: none;
  border: none;
  padding: 0;
  font: inherit;
  color: var(--color-accent);
  cursor: pointer;
}
.title:hover {
  text-decoration: underline;
}
.labels {
  margin-top: var(--space-2);
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
}
.badge {
  display: inline-block;
  padding: 1px var(--space-3);
  border-radius: var(--radius-md);
  font-size: var(--font-size-xs);
  line-height: 1.5;
}
.badge.label {
  background: var(--color-neutral-bg);
  color: inherit;
}
.badge.kind {
  background: var(--color-accent-badge-bg);
  color: var(--color-accent);
}
.skip-note {
  margin: var(--space-2) 0 0;
  font-size: var(--font-size-xs);
  color: var(--color-text-muted);
}
</style>
