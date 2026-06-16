<script setup lang="ts">
// Composition-root layout: wires the slice views together and switches between the
// monitor / settings / onboarding views. Slices own their own UI + state; App only
// arranges them.
import { computed, onMounted, ref } from "vue";
import { appVersion } from "./config/api";
import { useConfigStore } from "./config/useConfigStore";
import SettingsView from "./config/SettingsView.vue";
import OnboardingWizard from "./config/OnboardingWizard.vue";
import PollControls from "./pr/PollControls.vue";
import PrList from "./pr/PrList.vue";
import { usePrStore } from "./pr/usePrStore";
import StatusBar from "./StatusBar.vue";
import ReviewPanel from "./review/ReviewPanel.vue";
import ReviewSessions from "./review/ReviewSessions.vue";
import { useReviewStore } from "./review/useReviewStore";
import { reschedule, startPolling } from "./pr/api";
import { useAppView } from "./useAppView";

const version = ref("");
// Gate view selection until the config load resolves, so a first-launch user never
// flashes the monitor (and its poll-triggering children) before onboarding mounts.
const booting = ref(true);

const prStore = usePrStore();
const configStore = useConfigStore();
const { currentView, goMonitor, goSettings, goOnboarding } = useAppView();

// Login / availability banner (composition layer only): #8 auto-triggers reviews,
// which silently stall if `gh` isn't authenticated or codex is unavailable.
// Reading BOTH slices' status (gh from the pr store, codex from the review store)
// is legitimate here — App is the cross-slice wiring point, exactly like StatusBar.
// Hidden while either status is still loading (null) so a cold start doesn't flash
// a false warning; shown only on a confirmed unavailable signal.
// `dispatchError` is the session-less auto-trigger notice (#8): the backend
// dispatcher emits it on a bad config / start failure / ledger-write failure, so
// the same banner that warns "auto review paused" also reports "auto review failed".
const { codex, dispatchError, clearDispatchError } = useReviewStore();
const ghBlocked = computed(() => prStore.gh?.authenticated === false);
const codexBlocked = computed(() => codex.value?.available === false);
const showPrompt = computed(() => ghBlocked.value || codexBlocked.value);
// A config LOAD failure leaves `config` null; surface it instead of silently
// degrading to an empty monitor (finding F3). The user can open Settings to retry
// (SettingsView re-loads on mount) or re-save.
const configError = computed(() => (configStore.config ? null : configStore.error));

// A rejected Tauri invoke throws the AppError object `{ message }`; fall back to a
// stringified form for any non-conforming throw (mirrors the slice stores).
function toMsg(e: unknown): string {
  return (e as { message?: string })?.message ?? String(e);
}

onMounted(async () => {
  version.value = await appVersion();
  // First-launch detection: a freshly-installed app loads AppConfig::default(),
  // whose only empty field is repoRoot — the backend gate (lib.rs) won't have
  // auto-started the poll loop for it → route to onboarding. Require config to be
  // *present* and empty: a null config means the load itself failed (IPC error),
  // and forcing a returning user back through onboarding on a transient error would
  // be worse than degrading to the monitor view.
  await configStore.load();
  if (configStore.config && configStore.config.repoRoot === "") {
    goOnboarding();
  } else {
    goMonitor();
  }
  booting.value = false;
});

// Cross-slice wiring (composition root only): a config save may both (a) fix a
// previously-invalid config that left the loop gated off at launch and (b) change
// the poll interval. start_polling is idempotent (no-op if already running) and
// recovers the gated-off case; reschedule then applies the new period.
async function onConfigSaved() {
  try {
    await startPolling();
    prStore.polling = true;
  } catch (e) {
    // start_polling now rejects under an invalid config (finding F1). Keep the flag
    // honest and surface the error (finding F3) so a saved-but-not-running monitor is
    // visible — not just a console line implying the loop resumed.
    prStore.polling = false;
    prStore.error = toMsg(e);
    return;
  }
  // The loop is running; a reschedule failure only delays the period rebuild (the
  // next poll still runs on the old period), so surface it without claiming a stop.
  try {
    await reschedule();
  } catch (e) {
    prStore.error = toMsg(e);
  }
}

// Onboarding finished with a validated save. The backend gate did NOT auto-start
// the loop (config was invalid at launch), so start it now and reflect it in the pr
// store before entering the monitor view — this is the downstream that closes the
// poll-gate funnel (upstream = the lib.rs validity gate).
async function onOnboardingDone() {
  try {
    await startPolling();
    prStore.polling = true;
  } catch (e) {
    // Keep the polling flag honest (PollControls won't claim the loop is running)
    // AND surface the error (finding F3) so the user sees why the monitor didn't
    // start — not just a console line — and can retry from the monitor controls.
    prStore.polling = false;
    prStore.error = toMsg(e);
  }
  goMonitor();
}

// Cross-slice wiring: the pr slice selects a PR, the review slice reviews it.
// Holding the selection here keeps the two slices decoupled (neither imports the
// other) — App passes it down to both PrList (highlight) and ReviewPanel (target).
// Selection is keyed by PR *number*, not list identity (#38): the retained list
// re-emits stable rows, so resolving the selected PR from the live store keeps the
// highlight + review target pinned across refreshes — and auto-clears to null when
// the selected PR drops out of the list entirely. Resolve against the *non-archived*
// rows: archiving the selected PR retires it, so the review target clears and
// ReviewPanel's "开始 review" doesn't stay enabled on an archived PR.
const selectedNumber = ref<number | null>(null);
const selectedPr = computed(
  () =>
    prStore.prs.find(
      (p) => !p.archived && p.number === selectedNumber.value,
    ) ?? null,
);
</script>

<template>
  <div class="app">
    <header class="app-bar">
      <strong>prmonitor</strong>
      <small v-if="version">v{{ version }}</small>
      <span class="spacer" />
      <button
        v-if="!booting && currentView === 'monitor'"
        type="button"
        class="nav-btn"
        @click="goSettings"
      >
        设置
      </button>
      <button
        v-else-if="currentView === 'settings'"
        type="button"
        class="nav-btn"
        @click="goMonitor"
      >
        ← 返回监控
      </button>
    </header>

    <div v-if="booting" class="view booting" />

    <OnboardingWizard
      v-else-if="currentView === 'onboarding'"
      class="view"
      @done="onOnboardingDone"
    />

    <SettingsView
      v-else-if="currentView === 'settings'"
      class="view"
      @saved="onConfigSaved"
      @close="goMonitor"
    />

    <div v-else class="layout">
      <aside class="sidebar">
        <PollControls />
        <PrList
          :selected-number="selectedNumber"
          @select="(pr) => (selectedNumber = pr.number)"
        />
      </aside>
      <main class="content">
        <div
          v-if="showPrompt || dispatchError || configError"
          class="availability"
          role="alert"
        >
          <p v-if="configError" class="line dispatch-error">
            <span>⚠ 配置加载失败：{{ configError }}</span>
            <button type="button" class="dismiss" @click="goSettings">
              打开设置
            </button>
          </p>
          <p v-if="showPrompt" class="line">
            自动 review 已暂停 —
            <template v-if="ghBlocked">
              gh 未登录，请运行 <code>gh auth login</code>
            </template>
            <template v-if="ghBlocked && codexBlocked"> ；</template>
            <template v-if="codexBlocked">
              codex 不可用（{{ codex?.message }}）
            </template>
          </p>
          <p v-if="dispatchError" class="line dispatch-error">
            <span>⚠ 自动 review 异常：{{ dispatchError }}</span>
            <button
              type="button"
              class="dismiss"
              aria-label="关闭 / dismiss"
              @click="clearDispatchError"
            >
              ✕
            </button>
          </p>
        </div>
        <ReviewSessions />
        <ReviewPanel :selected-pr="selectedPr" />
      </main>
    </div>

    <StatusBar />
  </div>
</template>

<style>
:root {
  font-family: var(--font-sans);
  color: var(--color-text);
  background-color: var(--color-bg);
}

body {
  margin: 0;
}
</style>

<style scoped>
.app {
  display: flex;
  flex-direction: column;
  height: 100vh;
}

.app-bar {
  display: flex;
  align-items: center;
  gap: var(--space-4);
  padding: var(--space-5) var(--space-8);
  border-bottom: 1px solid var(--color-border-strong);
}

.app-bar small {
  color: var(--color-text-muted);
}

.spacer {
  flex: 1;
}

.nav-btn {
  padding: var(--space-2) var(--space-4);
  font: inherit;
  font-size: var(--font-size-sm);
  color: var(--color-accent);
  background: none;
  border: none;
  border-radius: var(--radius-sm);
  cursor: pointer;
}

.nav-btn:hover {
  background: var(--color-surface-hover);
}

.view {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
}

.layout {
  display: flex;
  flex: 1;
  min-height: 0;
}

.sidebar {
  width: var(--sidebar-width);
  border-right: 1px solid var(--color-border-strong);
  overflow-y: auto;
  padding: var(--space-6);
}

.content {
  flex: 1;
  overflow-y: auto;
  padding: var(--space-6);
}

.availability {
  margin-bottom: var(--space-6);
  padding: var(--space-4) var(--space-6);
  border: 1px solid var(--color-warn-border);
  border-radius: var(--radius-sm);
  background: var(--color-warn-bg);
  font-size: var(--font-size-md);
  color: var(--color-warn);
}

.availability code {
  font-family: var(--font-mono);
  font-size: var(--font-size-sm);
}

.availability .line {
  margin: 0;
}

.availability .line + .line {
  margin-top: var(--space-3);
}

/* Dispatch failures are error-toned (vs the warning-toned availability prompt). */
.availability .dispatch-error {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--space-4);
  color: var(--color-danger);
}

.availability .dismiss {
  flex: none;
  padding: 0 var(--space-2);
  border: none;
  background: transparent;
  color: inherit;
  font: inherit;
  line-height: 1;
  cursor: pointer;
}
</style>
