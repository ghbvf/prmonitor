<script setup lang="ts">
// A single PR row: number/title (click to open in the external browser), the
// label badges, the trigger-label `kind` badge, and a muted "skipped" treatment
// carrying `skipReason` when the PR would not dispatch.
import { openUrl } from "@tauri-apps/plugin-opener";
import type { PullRequestView } from "../types";

const props = defineProps<{ pr: PullRequestView }>();

function open() {
  // Only follow web links (gh returns https PR urls); reject any other scheme,
  // and surface a failure instead of dropping it silently.
  const url = props.pr.url;
  if (!/^https?:\/\//i.test(url)) return;
  openUrl(url).catch((err) => console.error("打开链接失败", err));
}
</script>

<template>
  <li class="pr-row" :class="{ skipped: pr.skipReason != null }">
    <div class="title-line">
      <button type="button" class="title" @click="open">
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
  padding: 6px 0;
  border-bottom: 1px solid rgba(128, 128, 128, 0.15);
}
.pr-row.skipped {
  opacity: 0.55;
}
.title-line {
  display: flex;
  align-items: center;
  gap: 6px;
}
.title {
  flex: 1;
  text-align: left;
  background: none;
  border: none;
  padding: 0;
  font: inherit;
  color: #2563eb;
  cursor: pointer;
}
.title:hover {
  text-decoration: underline;
}
.labels {
  margin-top: 4px;
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
}
.badge {
  display: inline-block;
  padding: 1px 6px;
  border-radius: 8px;
  font-size: 11px;
  line-height: 1.5;
}
.badge.label {
  background: rgba(128, 128, 128, 0.18);
  color: inherit;
}
.badge.kind {
  background: rgba(37, 99, 235, 0.15);
  color: #2563eb;
}
.skip-note {
  margin: 4px 0 0;
  font-size: 11px;
  color: #888;
}
</style>
