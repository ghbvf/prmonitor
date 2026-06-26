<script setup lang="ts">
// A single PR row: number/title (click to open in the external browser), the
// label badges, the trigger-label `kind` badge, and a muted "skipped" treatment
// carrying `skipReason` when the PR would not dispatch. Clicking the row selects
// the PR (for the Review panel); the title link opens the browser (`@click.stop`
// so the two actions stay distinct).
import { getTransport } from "../transport";
import type { TrackedPrView } from "../types";

const props = defineProps<{ pr: TrackedPrView; selected: boolean }>();
const emit = defineEmits<{
  select: [pr: TrackedPrView];
  "set-archived": [payload: { number: number; archived: boolean }];
}>();

function open() {
  // Only follow web links (gh returns https PR urls); reject any other scheme,
  // and surface a failure instead of dropping it silently.
  const url = props.pr.url;
  if (!/^https?:\/\//i.test(url)) return;
  getTransport()
    .openExternal(url)
    .catch((err) => console.error("打开链接失败", err));
}
</script>

<template>
  <li
    class="pr-row"
    :class="{ skipped: pr.skipReason != null, selected, stale: pr.presence === 'stale' }"
    @click="emit('select', pr)"
  >
    <div class="title-line">
      <button
        type="button"
        class="title"
        :title="`#${pr.number} — ${pr.title}`"
        @click.stop="open"
      >
        #{{ pr.number }} — {{ pr.title }}
      </button>
      <span class="badge kind">{{ pr.kind }}</span>
      <button
        type="button"
        class="archive-btn"
        :title="pr.archived ? '恢复 / unarchive' : '归档 / archive'"
        @click.stop="
          emit('set-archived', { number: pr.number, archived: !pr.archived })
        "
      >
        {{ pr.archived ? "恢复" : "归档" }}
      </button>
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
/* Stale (retained-but-inactive) rows get the same muted treatment as skipped
   ones — reuses the existing opacity convention so the two read consistently. */
.pr-row.stale {
  opacity: 0.55;
}
.title-line {
  display: flex;
  align-items: center;
  gap: var(--space-3);
}
.title {
  flex: 1;
  /* #39: truncate long titles to one line so they never overflow the 320px
     sidebar; min-width:0 lets the flex item shrink below its content width. */
  min-width: 0;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
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
/* Badge + archive control must not shrink; only the title (flex:1) absorbs the
   width pressure (#39). */
.badge.kind {
  flex: none;
}
.archive-btn {
  flex: none;
  padding: 1px var(--space-3);
  font: inherit;
  font-size: var(--font-size-xs);
  color: var(--color-text-muted);
  background: none;
  border: none;
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.archive-btn:hover {
  background: var(--color-surface-hover);
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
