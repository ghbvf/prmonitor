<script setup lang="ts">
// Composition-root layout: wires the slice views together and switches between the
// monitor / settings / onboarding views. Slices own their own UI + state; App only
// arranges them.
import { computed, onMounted, ref, watch } from "vue";
import { appVersion } from "./config/api";
import { useConfigStore } from "./config/useConfigStore";
import SettingsView from "./config/SettingsView.vue";
import OnboardingWizard from "./config/OnboardingWizard.vue";
import PollControls from "./pr/PollControls.vue";
import ProjectNav from "./pr/ProjectNav.vue";
import WebhookPanel from "./pr/WebhookPanel.vue";
import { usePrStore } from "./pr/usePrStore";
import StatusBar from "./StatusBar.vue";
import ReviewPanel from "./review/ReviewPanel.vue";
import ReviewSessions from "./review/ReviewSessions.vue";
import { useReviewStore } from "./review/useReviewStore";
import { reschedule, startPolling } from "./pr/api";
import { useAppView } from "./useAppView";
import { useProjects } from "./projects";
import { autoReviewSourceCli, periodicPollEligible } from "./types";

const version = ref("");
// Gate view selection until the config load resolves, so a first-launch user never
// flashes the monitor (and its poll-triggering children) before onboarding mounts.
const booting = ref(true);

const prStore = usePrStore();
const configStore = useConfigStore();
const { currentView, goMonitor, goSettings, goOnboarding } = useAppView();
// Active project (#35): the switcher rail flips it; the PR + review views below
// resolve their data against it. Persisted via useProjects().setActive.
const { activeProjectId } = useProjects();

// Login / availability banner (composition layer only): #8 auto-triggers reviews,
// which silently stall if the active project's SELECTED source CLI isn't authed or its
// SELECTED engine is unavailable. Reading BOTH slices' status (gh/az from the pr store,
// codex/claude from the review store) is legitimate here — App is the cross-slice wiring
// point, exactly like StatusBar. Hidden while a status is still loading (null) so a cold
// start doesn't flash a false warning; shown only on a confirmed unavailable signal.
// Keyed to the ACTIVE project (vs the StatusBar's enabled-aggregate): the banner is about
// the project you're looking at, which has exactly one source + one engine.
// `dispatchError` is the session-less auto-trigger notice (#8): the backend dispatcher
// emits it on a bad config / start failure / ledger-write failure, so the same banner that
// warns "auto review paused" also reports "auto review failed".
const { codex, claude, dispatchError, clearDispatchError, clearFocus } =
  useReviewStore();
const activeProject = computed(
  () =>
    configStore.config?.projects.find((p) => p.id === activeProjectId.value) ??
    null,
);
// A DISABLED project never auto-reviews (the backend skips it in scheduling / webhook
// routing), so its source/engine health must NOT drive the "paused" banner — gate every
// half on this. (Both halves share it; without it, switching to a disabled project would
// surface its cached gh/az/codex/claude failure as a spurious pause.)
const activeEnabled = computed(() => activeProject.value?.enabled === true);
// The source CLI the active project's AUTO-review pipeline depends on (gh / az / null).
const activeSourceCli = computed(() =>
  activeEnabled.value
    ? autoReviewSourceCli(
        activeProject.value!.sourceKind,
        activeProject.value!.updateMode,
      )
    : null,
);
const ghBlocked = computed(
  () => activeSourceCli.value === "gh" && prStore.gh?.authenticated === false,
);
const azBlocked = computed(
  () => activeSourceCli.value === "az" && prStore.az?.authenticated === false,
);
// Engine half — gate on the ACTIVE project's SELECTED engine (so codex's state never
// blocks a claude project, and vice versa) AND on `enabled` (a disabled project doesn't
// auto-review). Null-safe (`?.`) so a cold start (status still null) shows no false warn.
const codexBlocked = computed(
  () =>
    activeEnabled.value &&
    activeProject.value?.engineKind === "codex" &&
    codex.value?.available === false,
);
const claudeBlocked = computed(
  () =>
    activeEnabled.value &&
    activeProject.value?.engineKind === "claude" &&
    claude.value?.available === false,
);
const sourceBlocked = computed(() => ghBlocked.value || azBlocked.value);
const engineBlocked = computed(() => codexBlocked.value || claudeBlocked.value);
const showPrompt = computed(() => sourceBlocked.value || engineBlocked.value);
// dispatchError is keyed per project (#35): show the active project's notice.
const activeDispatchError = computed(
  () => dispatchError.value[activeProjectId.value] ?? null,
);
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
  // Seed the project switcher + active selection from the loaded config (#35).
  if (configStore.config) useProjects().hydrate(configStore.config);
  // First-launch detection is now "no projects yet": migration wraps a legacy
  // single-repo config into projects:[default], and onboarding creates the first
  // project — so an empty `projects` is the only fresh-install state that routes to
  // onboarding. A null config means the load failed (IPC error); degrade to the
  // monitor rather than forcing a returning user back through onboarding.
  if (configStore.config && configStore.config.projects.length === 0) {
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
  // A save may have added / removed / edited projects, so re-load and re-hydrate the
  // switcher + active selection before reconciling the loops (#35).
  await configStore.load();
  if (configStore.config) useProjects().hydrate(configStore.config);
  const projects = useProjects().projects.value;
  const ids = projects.map((p) => p.id);
  const activeId = activeProjectId.value;
  try {
    // start_polling reconciles ALL enabled projects' loops (idempotent); recovers a
    // project whose loop was gated off at launch by a previously-invalid config.
    await startPolling();
    // Reflect the backend reconcile per project: a loop runs only for poll-eligible
    // projects (enabled && pull/hybrid), so set the optimistic flag from eligibility
    // instead of a blanket true — otherwise disabled/webhook-only/manual projects would
    // show "监控中" in the nav until visited (#150 F1b).
    for (const p of projects) {
      prStore.polling[p.id] = periodicPollEligible(p.enabled, p.updateMode);
    }
  } catch (e) {
    // start_polling rejects under an invalid config (finding F1). Keep the flags
    // honest and surface the error (finding F3) so a saved-but-not-running monitor is
    // visible — not just a console line implying the loop resumed.
    for (const id of ids) prStore.polling[id] = false;
    prStore.error[activeId] = toMsg(e);
    return;
  }
  // The loops are running; a reschedule failure only delays the period rebuild (the
  // next poll still runs on the old period), so surface it without claiming a stop.
  try {
    await reschedule();
  } catch (e) {
    prStore.error[activeId] = toMsg(e);
  }
}

// Onboarding finished with a validated save. The backend gate did NOT auto-start
// the loop (config was invalid at launch), so start it now and reflect it in the pr
// store before entering the monitor view — this is the downstream that closes the
// poll-gate funnel (upstream = the lib.rs validity gate).
async function onOnboardingDone() {
  // Onboarding created the first project; re-load + hydrate so the switcher and the
  // active selection reflect it before the monitor view mounts (#35).
  await configStore.load();
  if (configStore.config) useProjects().hydrate(configStore.config);
  const projects = useProjects().projects.value;
  const ids = projects.map((p) => p.id);
  const activeId = activeProjectId.value;
  try {
    await startPolling();
    // Eligibility-based optimistic flag (#150 F1b), as in onConfigSaved.
    for (const p of projects) {
      prStore.polling[p.id] = periodicPollEligible(p.enabled, p.updateMode);
    }
  } catch (e) {
    // Keep the polling flags honest (PollControls won't claim the loop is running)
    // AND surface the error (finding F3) so the user sees why the monitor didn't
    // start — not just a console line — and can retry from the monitor controls.
    for (const id of ids) prStore.polling[id] = false;
    prStore.error[activeId] = toMsg(e);
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
    (prStore.prs[activeProjectId.value] ?? []).find(
      (p) => !p.archived && p.number === selectedNumber.value,
    ) ?? null,
);
// Switching projects clears the selection AND the focused review (#35). PR numbers
// collide across repos, so a carried-over selection could point ReviewPanel at the
// wrong PR; and the review focus is a global singleton, so without clearFocus the
// panel would keep showing/stopping the previous project's session (pr-review F5).
watch(activeProjectId, () => {
  selectedNumber.value = null;
  clearFocus();
});
// Switching the SELECTED PR (within a project) must also drop the focused review:
// the focus is a global singleton, so without this ReviewPanel keeps rendering the
// previous PR's session stream after you pick a different PR. Blank it; the user
// re-focuses by clicking a row in the now-PR-filtered ReviewSessions list. Watch
// `selectedNumber` (not `selectedPr`): selection is by number (#38), so a list
// refresh re-emitting the same row won't false-trigger — only a real PR switch does.
// Skip the → null transition: that fires only from the project-switch watch above
// (which already clears focus), so guarding it avoids a redundant double clearFocus.
// That same watch also covers the "no PR selected but a session was hydrated on
// launch" path, where `selectedNumber` stays null and this watch never fires.
watch(selectedNumber, (n) => {
  if (n !== null) clearFocus();
});
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
    >
      <!-- Composition root fills SettingsView's `webhook` slot with the pr-slice
           WebhookPanel: SettingsView (config) and WebhookPanel (pr) never import each
           other; App wires them, passing saved config + live draft + saving flag. -->
      <template #webhook="s">
        <WebhookPanel
          :saved-config="s.savedConfig"
          :draft="s.draft"
          :saving="s.saving"
        />
      </template>
    </SettingsView>

    <div v-else class="layout">
      <!-- Unified project → PR navigation (#67): one column (ProjectNav merges the old
           ProjectSwitcher rail + PrList sidebar) — projects as H2, the active project's
           PRs nested beneath. Sessions stay OUT of this column (the independent session
           panel lives in the content area), per the chosen layout. -->
      <aside class="nav-column">
        <PollControls />
        <ProjectNav
          :selected-number="selectedNumber"
          @select="(pr) => (selectedNumber = pr.number)"
        />
      </aside>
      <main class="content">
        <div
          v-if="showPrompt || activeDispatchError || configError"
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
            <template v-if="azBlocked">
              az 未登录，请运行 <code>az login</code>
            </template>
            <template v-if="sourceBlocked && engineBlocked"> ；</template>
            <template v-if="codexBlocked">
              codex 不可用（{{ codex?.message }}）
            </template>
            <template v-if="claudeBlocked">
              claude 不可用（{{ claude?.message }}）
            </template>
          </p>
          <p v-if="activeDispatchError" class="line dispatch-error">
            <span>⚠ 自动 review 异常：{{ activeDispatchError }}</span>
            <button
              type="button"
              class="dismiss"
              aria-label="关闭 / dismiss"
              @click="clearDispatchError(activeProjectId)"
            >
              ✕
            </button>
          </p>
        </div>
        <ReviewSessions
          :project-id="activeProjectId"
          :pr-number="selectedPr?.number ?? null"
        />
        <ReviewPanel :selected-pr="selectedPr" />
      </main>
    </div>

    <!-- Mounted in EVERY non-booting view (sibling of the view switch above): it owns the
         gated source/engine status probing the banner above reads, so it must stay here —
         don't move it inside a view branch. -->
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

.nav-column {
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
