<script setup lang="ts">
import type {
  AppConfig,
  ReviewLifecycleEvent,
  ReviewLifecycleTarget,
} from "./types";
import { REVIEW_LIFECYCLE_EVENTS } from "./types";

const props = defineProps<{ draft: AppConfig }>();
const emit = defineEmits<{ edit: [] }>();

function assertNever(value: never): never {
  throw new Error(`Unhandled review lifecycle target: ${JSON.stringify(value)}`);
}

function eventLabel(event: ReviewLifecycleEvent): string {
  switch (event) {
    case "started":
      return "开始";
    case "completed":
      return "完成";
    case "failed":
      return "失败";
    case "interrupted":
      return "中断";
    default:
      return assertNever(event);
  }
}

function targetLabel(target: ReviewLifecycleTarget): string {
  switch (target.kind) {
    case "notificationChannels":
      return "通知渠道";
    case "messagingConversation":
      return "机器人会话";
    default:
      return assertNever(target);
  }
}

function csv(value: string): string[] {
  return value
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

function setEnabled(event: Event) {
  props.draft.reviewLifecycleNotifications.enabled = (event.target as HTMLInputElement).checked;
  emit("edit");
}

function toggleEvent(kind: ReviewLifecycleEvent, event: Event) {
  const checked = (event.target as HTMLInputElement).checked;
  const events = props.draft.reviewLifecycleNotifications.events;
  if (checked && !events.includes(kind)) events.push(kind);
  if (!checked) props.draft.reviewLifecycleNotifications.events = events.filter((e) => e !== kind);
  emit("edit");
}

function setDelay(key: "startDelaySecs" | "endDelaySecs", event: Event) {
  props.draft.reviewLifecycleNotifications[key] = Math.max(
    0,
    Number((event.target as HTMLInputElement).value) || 0,
  );
  emit("edit");
}

function addNotificationTarget() {
  props.draft.reviewLifecycleNotifications.targets.push({
    kind: "notificationChannels",
    channelIds: [],
  });
  emit("edit");
}

function addMessagingTarget() {
  props.draft.reviewLifecycleNotifications.targets.push({
    kind: "messagingConversation",
    integrationId: "",
    conversationId: "",
  });
  emit("edit");
}

function deleteTarget(index: number) {
  props.draft.reviewLifecycleNotifications.targets.splice(index, 1);
  emit("edit");
}

function setTargetChannels(target: Extract<ReviewLifecycleTarget, { kind: "notificationChannels" }>, event: Event) {
  target.channelIds = csv((event.target as HTMLInputElement).value);
  emit("edit");
}

function setTargetText(
  target: Extract<ReviewLifecycleTarget, { kind: "messagingConversation" }>,
  key: "integrationId" | "conversationId",
  event: Event,
) {
  target[key] = (event.target as HTMLInputElement).value;
  emit("edit");
}
</script>

<template>
  <section class="lifecycle">
    <header class="head">
      <label class="enabled">
        <input
          type="checkbox"
          :checked="draft.reviewLifecycleNotifications.enabled"
          @change="setEnabled"
        />
        <span>Review 生命周期通知</span>
      </label>
    </header>

    <div class="events">
      <label v-for="event in REVIEW_LIFECYCLE_EVENTS" :key="event" class="chip">
        <input
          type="checkbox"
          :checked="draft.reviewLifecycleNotifications.events.includes(event)"
          @change="toggleEvent(event, $event)"
        />
        <span>{{ eventLabel(event) }}</span>
      </label>
    </div>

    <div class="grid">
      <label>
        <span>开始延迟（秒）</span>
        <input
          type="number"
          min="0"
          :value="draft.reviewLifecycleNotifications.startDelaySecs"
          @input="setDelay('startDelaySecs', $event)"
        />
      </label>
      <label>
        <span>结束延迟（秒）</span>
        <input
          type="number"
          min="0"
          :value="draft.reviewLifecycleNotifications.endDelaySecs"
          @input="setDelay('endDelaySecs', $event)"
        />
      </label>
    </div>

    <div class="targets">
      <div class="target-actions">
        <button type="button" @click="addNotificationTarget">添加通知渠道</button>
        <button type="button" @click="addMessagingTarget">添加机器人会话</button>
      </div>

      <section
        v-for="(target, index) in draft.reviewLifecycleNotifications.targets"
        :key="`${target.kind}-${index}`"
        class="target"
      >
        <header>
          <strong>{{ targetLabel(target) }}</strong>
          <button type="button" class="delete" @click="deleteTarget(index)">删除</button>
        </header>
        <label v-if="target.kind === 'notificationChannels'">
          <span>Channel IDs</span>
          <input
            type="text"
            :value="target.channelIds.join(', ')"
            @input="setTargetChannels(target, $event)"
          />
        </label>
        <template v-else-if="target.kind === 'messagingConversation'">
          <label>
            <span>Integration ID</span>
            <input
              type="text"
              :value="target.integrationId"
              @input="setTargetText(target, 'integrationId', $event)"
            />
          </label>
          <label>
            <span>Conversation ID</span>
            <input
              type="text"
              :value="target.conversationId"
              @input="setTargetText(target, 'conversationId', $event)"
            />
          </label>
        </template>
      </section>
    </div>
  </section>
</template>

<style scoped>
.lifecycle,
.targets,
.target {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
}
.head,
.events,
.target-actions,
.target header {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  flex-wrap: wrap;
}
.enabled,
.chip {
  display: inline-flex;
  align-items: center;
  gap: var(--space-2);
  font-size: var(--font-size-sm);
}
.grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(180px, 1fr));
  gap: var(--space-3);
}
label {
  display: flex;
  flex-direction: column;
  gap: var(--space-1);
  font-size: var(--font-size-sm);
}
input[type="text"],
input[type="number"] {
  width: 100%;
  padding: var(--space-2);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
  background: var(--color-surface);
  color: var(--color-text);
}
button {
  padding: var(--space-2) var(--space-3);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
  background: var(--color-surface);
  color: var(--color-text);
  cursor: pointer;
}
.delete {
  color: var(--color-danger);
}
.target {
  padding: var(--space-3);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-sm);
}
@media (max-width: 720px) {
  .grid {
    grid-template-columns: 1fr;
  }
}
</style>
