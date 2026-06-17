<script setup lang="ts">
// First-launch onboarding wizard (#34, multi-project #35): walks STEPS (repo →
// repoRoot → skill → source → autoReview → done), reusing the fields.ts PROJECT_GROUPS
// FieldDefs so labels/hints match Settings. It builds the FIRST project: the `draft`
// is a single reactive `Project`, seeded from the persisted config's first project
// (defaults prefill; on a first launch repoRoot is ""). "下一步" runs validateStep and
// blocks on a message; `finish()` composes the full AppConfig — one project plus the
// global webhook defaults — and the final save routes a backend error back to its
// owning step via errorToStep.
import { computed, onMounted, reactive, ref, watch } from "vue";
import { useConfigStore } from "./useConfigStore";
import type { AppConfig, Project } from "./types";
import {
  PROJECT_GROUPS,
  STEPS,
  validateStep,
  errorToStep,
  type FieldDef,
  type ProjectFieldKey,
  type StepId,
} from "./fields";
import ConfigField from "./ConfigField.vue";

const store = useConfigStore();

// Emitted only after a validated, successful save.
const emit = defineEmits<{ done: [] }>();

// The first project being created. Per-project fields only — identity (id/name) and
// the global webhook fields are filled in by `finish()` when composing the AppConfig.
const draft = reactive<Project>({
  id: "default",
  name: "默认项目",
  enabled: true,
  repo: "",
  repoRoot: "",
  pollIntervalSecs: 120,
  authors: [],
  reviewLabel: "pr-status/needs-review-again",
  checkLabel: "pr-status/needs-check-fix",
  skillRelPath: ".codex/skills/pr-review/SKILL.md",
  prCooldownSeconds: 1800,
  sourceKind: "github",
  engineKind: "codex",
  autoReview: false,
});

const authorsInput = ref("");

// Seed the project draft from the persisted config's FIRST project (#35) if present,
// so the wizard prefills sensible defaults. A first launch has a single default
// project whose repoRoot is "".
function hydrate(cfg: AppConfig) {
  const p = cfg.projects[0];
  if (!p) return;
  draft.id = p.id;
  draft.name = p.name;
  draft.enabled = p.enabled;
  draft.repo = p.repo;
  draft.repoRoot = p.repoRoot;
  draft.pollIntervalSecs = p.pollIntervalSecs;
  draft.authors = [...p.authors];
  draft.reviewLabel = p.reviewLabel;
  draft.checkLabel = p.checkLabel;
  draft.skillRelPath = p.skillRelPath;
  draft.prCooldownSeconds = p.prCooldownSeconds;
  draft.sourceKind = p.sourceKind;
  draft.engineKind = p.engineKind;
  draft.autoReview = p.autoReview;
  authorsInput.value = p.authors.join(", ");
}

// Seed defaults from the persisted config so the wizard prefills sensible values.
watch(
  () => store.config,
  (cfg) => {
    if (cfg) hydrate(cfg);
  },
  { immediate: true },
);

onMounted(() => store.load());

const stepIndex = ref(0);
const currentStep = computed<StepId>(() => STEPS[stepIndex.value]);
// Inline validation / backend error for the current step.
const stepError = ref<string | null>(null);

// All per-project field defs flattened, addressable by key — the wizard cherry-picks
// which FieldDefs each step shows (reusing the same definitions Settings groups use).
// Every STEP_FIELDS key is a `keyof Project`, so PROJECT_GROUPS is the right source.
const FIELD_BY_KEY = new Map<ProjectFieldKey, FieldDef>(
  PROJECT_GROUPS.flatMap((g) => g.fields).map((f) => [f.key, f]),
);

function defOf(key: ProjectFieldKey): FieldDef {
  const def = FIELD_BY_KEY.get(key);
  // Every per-project key is grouped (asserted in fields.test.ts), so this never
  // misses; throw rather than render a half-broken step if that invariant breaks.
  if (!def) throw new Error(`missing field def: ${key}`);
  return def;
}

// Which fields each step renders. Keys are `keyof Project` (#35).
const STEP_FIELDS: Record<StepId, ProjectFieldKey[]> = {
  repo: ["repo"],
  repoRoot: ["repoRoot"],
  skill: ["skillRelPath"],
  source: ["sourceKind"],
  autoReview: ["autoReview", "pollIntervalSecs", "prCooldownSeconds", "reviewLabel", "checkLabel", "authors"],
  done: [],
};

const currentFields = computed<FieldDef[]>(() =>
  STEP_FIELDS[currentStep.value].map(defOf),
);

function fieldValue(def: FieldDef): string | number | boolean | string[] {
  if (def.key === "authors") return authorsInput.value;
  return draft[def.key as ProjectFieldKey];
}

function setField(def: FieldDef, value: string | number | boolean | string[]) {
  if (def.key === "authors") {
    authorsInput.value = Array.isArray(value) ? value.join(", ") : String(value);
    return;
  }
  // FieldDef.kind matches its Project value type, so the assignment is type-correct
  // at runtime; the cast bridges the heterogeneous emit signature.
  (draft as Record<ProjectFieldKey, unknown>)[def.key as ProjectFieldKey] = value;
}

// Clear the inline error on real input so a fixed field stops showing stale text.
function onEdit() {
  stepError.value = null;
  store.error = null;
}

const isLast = computed(() => stepIndex.value === STEPS.length - 1);

function back() {
  if (stepIndex.value > 0) {
    stepIndex.value -= 1;
    stepError.value = null;
  }
}

function next() {
  const err = validateStep(currentStep.value, draft);
  if (err) {
    stepError.value = err;
    return;
  }
  stepError.value = null;
  if (stepIndex.value < STEPS.length - 1) stepIndex.value += 1;
}

function authorsArray(): string[] {
  return authorsInput.value
    .split(",")
    .map((a) => a.trim())
    .filter((a) => a.length > 0);
}

// Compose the full AppConfig from the single project draft plus the global webhook
// defaults. The first project gets the fixed id "default" (mirrors the backend
// migration's fixed id for symmetry) and `activeProjectId` points at it.
function composeConfig(): AppConfig {
  return {
    projects: [{ ...draft, id: "default", authors: authorsArray() }],
    activeProjectId: "default",
    webhookEnabled: false,
    webhookPort: 8787,
    webhookSecret: "",
    cloudflaredBin: "cloudflared",
    webhookTunnelMode: "quick",
    webhookTunnelCommand: "",
    webhookPublicUrl: "",
  };
}

async function finish() {
  // Re-validate every gated step before committing (defensive: the user could
  // reach `done` then edit a prior step's value via back-nav).
  for (const step of STEPS) {
    const err = validateStep(step, draft);
    if (err) {
      const target = STEPS.indexOf(step);
      stepIndex.value = target >= 0 ? target : stepIndex.value;
      stepError.value = err;
      return;
    }
  }

  await store.save(composeConfig());
  if (store.savedOk) {
    emit("done");
    return;
  }
  // Backend rejected: route to the owning step (fallback: stay on done) and show
  // the backend error inline so the user fixes the exact field.
  const msg = store.error ?? "保存失败";
  const target = errorToStep(msg);
  if (target) stepIndex.value = STEPS.indexOf(target);
  stepError.value = msg;
}
</script>

<template>
  <div class="wizard">
    <div class="card">
      <p class="step-indicator">步骤 {{ stepIndex + 1 }} / {{ STEPS.length }}</p>

      <div class="step-body">
        <template v-if="currentStep === 'repo'">
          <h2>连接仓库</h2>
          <p class="lead">填写要监控的 GitHub 仓库。</p>
        </template>
        <template v-else-if="currentStep === 'repoRoot'">
          <h2>本地仓库路径</h2>
          <p class="lead">填写本地 clone 的绝对路径，review 将在此运行。</p>
        </template>
        <template v-else-if="currentStep === 'skill'">
          <h2>Skill 路径</h2>
          <p class="lead">指向仓库内的 review skill（相对路径）。</p>
        </template>
        <template v-else-if="currentStep === 'source'">
          <h2>PR 来源</h2>
          <p class="lead">确认 PR 来源与 review 引擎。</p>
        </template>
        <template v-else-if="currentStep === 'autoReview'">
          <h2>自动 review</h2>
          <p class="lead">设置轮询节奏与触发标签（已填入默认值，可按需调整）。</p>
        </template>
        <template v-else>
          <h2>准备就绪</h2>
          <p class="lead">确认配置后开始监控。</p>
        </template>

        <div v-if="currentFields.length" class="fields">
          <ConfigField
            v-for="def in currentFields"
            :key="def.key"
            :def="def"
            :model-value="fieldValue(def)"
            @update:model-value="setField(def, $event)"
            @edit="onEdit"
          />
        </div>

        <dl v-if="currentStep === 'done'" class="summary">
          <div><dt>仓库</dt><dd>{{ draft.repo }}</dd></div>
          <div><dt>本地路径</dt><dd>{{ draft.repoRoot }}</dd></div>
          <div><dt>Skill</dt><dd>{{ draft.skillRelPath }}</dd></div>
          <div><dt>自动 review</dt><dd>{{ draft.autoReview ? "自动" : "手动" }}</dd></div>
          <div><dt>轮询间隔</dt><dd>{{ draft.pollIntervalSecs }} 秒</dd></div>
          <div><dt>PR 冷却</dt><dd>{{ draft.prCooldownSeconds }} 秒</dd></div>
          <div><dt>Review 标签</dt><dd>{{ draft.reviewLabel || "—" }}</dd></div>
          <div><dt>Check 标签</dt><dd>{{ draft.checkLabel || "—" }}</dd></div>
          <div><dt>作者过滤</dt><dd>{{ authorsInput || "（不过滤）" }}</dd></div>
        </dl>

        <p v-if="stepError" class="error">{{ stepError }}</p>
      </div>

      <div class="nav">
        <button
          type="button"
          class="ghost"
          :disabled="stepIndex === 0 || store.saving"
          @click="back"
        >
          上一步
        </button>
        <button
          v-if="!isLast"
          type="button"
          class="primary"
          @click="next"
        >
          下一步
        </button>
        <button
          v-else
          type="button"
          class="primary"
          :disabled="store.saving"
          @click="finish"
        >
          {{ store.saving ? "保存中…" : "完成并开始监控" }}
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.wizard {
  display: flex;
  align-items: center;
  justify-content: center;
  min-height: 100%;
  padding: var(--space-8);
  background: var(--color-bg);
  color: var(--color-text);
}
.card {
  display: flex;
  flex-direction: column;
  gap: var(--space-6);
  width: 100%;
  max-width: 480px;
  padding: var(--space-8);
  background: var(--color-surface);
  border: 1px solid var(--color-border);
  border-radius: var(--radius-md);
}
.step-indicator {
  margin: 0;
  font-size: var(--font-size-xs);
  color: var(--color-text-muted);
  text-transform: uppercase;
  letter-spacing: 0.05em;
}
.step-body {
  display: flex;
  flex-direction: column;
  gap: var(--space-5);
}
.step-body h2 {
  margin: 0;
  font-size: var(--font-size-lg);
}
.lead {
  margin: 0;
  font-size: var(--font-size-sm);
  color: var(--color-text-muted);
}
.fields {
  display: flex;
  flex-direction: column;
  gap: var(--space-5);
}
.summary {
  display: flex;
  flex-direction: column;
  gap: var(--space-3);
  margin: 0;
  padding: var(--space-4);
  background: var(--color-neutral-bg);
  border-radius: var(--radius-sm);
}
.summary > div {
  display: flex;
  justify-content: space-between;
  gap: var(--space-4);
  font-size: var(--font-size-sm);
}
.summary dt {
  color: var(--color-text-muted);
}
.summary dd {
  margin: 0;
  text-align: right;
  font-family: var(--font-mono);
  word-break: break-all;
}
.error {
  margin: 0;
  font-size: var(--font-size-sm);
  color: var(--color-danger);
}
.nav {
  display: flex;
  justify-content: space-between;
  gap: var(--space-4);
}
.ghost,
.primary {
  padding: var(--space-3) var(--space-6);
  font: inherit;
  font-size: var(--font-size-md);
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.ghost {
  background: none;
  color: var(--color-text);
  border: 1px solid var(--color-border-strong);
}
.ghost:hover:not(:disabled) {
  background: var(--color-surface-hover);
}
.primary {
  margin-left: auto;
  color: var(--color-surface);
  background: var(--color-accent);
  border: none;
}
.ghost:disabled,
.primary:disabled {
  opacity: 0.6;
  cursor: not-allowed;
}
</style>
