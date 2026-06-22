// Single field-definition source (#34): drives BOTH the grouped Settings form and
// the onboarding wizard, so the two surfaces never drift on labels/kinds/options.
// Plus two pure helpers — `validateStep` (per-step frontend gate) and `errorToStep`
// (route a backend AppError back to the wizard step that owns the offending field).
// Kept side-effect-free: no Pinia, no Vue — unit-tested in `fields.test.ts`.
//
// Multi-project (#35): the groups split into PROJECT_GROUPS (per-project fields,
// keys are `keyof Project`) and GLOBAL_GROUPS (the global webhook fields, keys are
// `keyof AppConfig`). `FieldGroup`/`FieldDef` are generic over the key type so each
// set is precisely typed and its coverage test can assert exactly that key set.
import { WEBHOOK_TUNNEL_MODES, type AppConfig, type Project } from "./types";
import {
  SOURCE_KINDS,
  ENGINE_KINDS,
  LABEL_SOURCES,
  UPDATE_MODES,
  assertNever,
  type UpdateMode,
} from "../types";

// A project field's key (per-project form/wizard) or a global AppConfig field's key
// (the webhook group). Generic `FieldDef<K>` keeps each group set precisely typed.
export type ProjectFieldKey = keyof Project;
export type GlobalFieldKey = keyof AppConfig;
export type FieldKey = ProjectFieldKey | GlobalFieldKey;
type FieldKind = "text" | "number" | "csv" | "select" | "checkbox";

export interface FieldDef<K extends FieldKey = FieldKey> {
  key: K;
  label: string;
  kind: FieldKind;
  hint?: string;
  // Only meaningful for kind "select"; mirrors the SourceKind/EngineKind/UpdateMode arms.
  // `options` carries the wire VALUES (single-sourced from the backing `as const` array
  // so it can't drift from the type). `optionLabels` is an optional display map (wire
  // value → human label); ConfigField falls back to the raw value when a label is absent.
  options?: readonly string[];
  optionLabels?: Record<string, string>;
  // Only meaningful for kind "number": the DOM `min` constraint. Defaults to 1 in the
  // renderer (`def.min ?? 1`) so ports/intervals stay ≥ 1; `localApiPort` sets `min: 0` so
  // its "0 = 关闭不监听" config semantic is actually enterable (AB#1043, codex F3).
  min?: number;
  // Reserved single-option enums (#11) are shown but not editable.
  readonly?: boolean;
  // text fields holding a credential (e.g. the webhook HMAC secret): rendered
  // masked (type=password) with a reveal toggle so it isn't exposed in screenshots
  // / screen-shares.
  secret?: boolean;
  // Conditional visibility (818 F14): a project field that only applies under some
  // sourceKind etc. (e.g. azureOrg/azureProject only for an Azure source). The Settings
  // ProjectCard filters its render loop on this against the live project draft; a field
  // without `visibleWhen` is always visible. Hidden fields keep their values (the
  // backend ignores them when irrelevant), so toggling back restores them. Only used by
  // per-project (Project-keyed) fields — the global webhook group never sets it.
  visibleWhen?: (p: Project) => boolean;
}

interface FieldGroup<K extends FieldKey = FieldKey> {
  id: string;
  title: string;
  fields: FieldDef<K>[];
}

// Display labels for each UpdateMode (818). Exhaustive over `UpdateMode` via the
// `assertNever` default (Medium穷尽 carrier, same as pollingEnabledForMode in
// src/types.ts), so a new mode forces a label here. UPDATE_MODE_LABELS below derives
// the select's display map from this, keeping the wire-value options single-sourced
// while showing Chinese text.
function updateModeLabel(mode: UpdateMode): string {
  switch (mode) {
    case "webhook-only":
      return "Webhook（默认）";
    case "pull-only":
      return "仅 CLI 轮询";
    case "hybrid":
      return "混合";
    case "manual":
      return "手动拉取";
    default:
      return assertNever(mode);
  }
}

// Wire value → display label map for the updateMode select, derived from the
// single-sourced UPDATE_MODES array (no hand-copied list). ConfigField shows the label
// but emits the raw wire value, so the camelCase contract stays intact.
const UPDATE_MODE_LABELS: Record<UpdateMode, string> = Object.fromEntries(
  UPDATE_MODES.map((m) => [m, updateModeLabel(m)]),
) as Record<UpdateMode, string>;

// Per-project field groups (#35). Every `Project` key appears exactly once across
// these groups (asserted in fields.test.ts), so adding a project field forces a home
// here. `id`/`name`/`enabled` are project-identity fields managed by the project
// list/selector UI, not by these form groups.
export const PROJECT_GROUPS: FieldGroup<ProjectFieldKey>[] = [
  {
    id: "project",
    title: "项目",
    fields: [
      {
        key: "repo",
        label: "仓库",
        kind: "text",
        hint: "GitHub 源: owner/name（如 ghbvf/prmonitor）；Azure 源: 裸仓库名（org/project 见下方）；Bitbucket 源: 裸仓库 slug（项目 Key 见下方）",
      },
      { key: "repoRoot", label: "本地路径", kind: "text", hint: "本地 clone 的绝对路径" },
      {
        key: "skillRelPath",
        label: "Skill 路径",
        kind: "text",
        hint: "相对仓库根的 skill 路径，如 .codex/skills/pr-review/SKILL.md（仅 codex 引擎）",
        // codex-only (#718): the claude engine discovers `.claude/skills/` from the
        // repo cwd, so it needs no configured skill path. Hidden for a claude project,
        // mirroring the source-conditional azure/bitbucket fields below; the backend
        // `validate_project` likewise skips skillRelPath unless engineKind === codex.
        visibleWhen: (p) => p.engineKind === "codex",
      },
    ],
  },
  {
    id: "polling",
    title: "轮询",
    fields: [
      {
        key: "updateMode",
        label: "数据更新模式",
        kind: "select",
        // Single-sourced from UPDATE_MODES (818) — type & options can't drift; Chinese
        // labels via the optionLabels map (ConfigField emits the raw wire value).
        options: UPDATE_MODES,
        optionLabels: UPDATE_MODE_LABELS,
        hint: "默认 Webhook；选 pull/hybrid 才启动 CLI 定时拉取（可能触发账号风控）",
      },
      {
        key: "autoReview",
        label: "自动 review",
        kind: "checkbox",
        hint: "勾选=发现 dispatchable PR 自动起 review；取消=仅手动「开始 review」触发",
      },
      {
        key: "pollIntervalSecs",
        label: "轮询间隔（秒）",
        kind: "number",
        hint: "拉取 PR 列表的间隔",
      },
      {
        key: "prCooldownSeconds",
        label: "PR 冷却（秒）",
        kind: "number",
        hint: "同一 PR 两次自动 review 的最小间隔",
      },
    ],
  },
  {
    id: "labels",
    title: "标签",
    fields: [
      {
        key: "labelSource",
        label: "标签来源",
        kind: "select",
        // Single-sourced from LABEL_SOURCES (717) — type & options can't drift; Chinese
        // display via optionLabels (ConfigField emits the raw wire value).
        options: LABEL_SOURCES,
        optionLabels: { native: "PR 原生标签", title: "从标题解析 [..]" },
        hint: "native=用 PR 提供方自带标签；title=从标题方括号解析（如 [pr-status/need-fix]）。Bitbucket 源无原生标签，必须选「从标题解析」",
      },
      { key: "reviewLabel", label: "Review 触发标签", kind: "text", hint: "命中即触发 review" },
      { key: "checkLabel", label: "Check 触发标签", kind: "text", hint: "命中即触发 check 修复" },
    ],
  },
  {
    id: "filters",
    title: "过滤",
    fields: [
      {
        key: "authors",
        label: "作者过滤",
        kind: "csv",
        hint: "逗号分隔；留空表示不按作者过滤",
      },
    ],
  },
  {
    id: "engine",
    title: "引擎",
    fields: [
      // PR 来源 now selectable (818, 717): github (gh) / Azure DevOps (az) / Bitbucket
      // Server (REST). engineKind is now a real select too (#718): codex / claude.
      {
        key: "sourceKind",
        label: "PR 来源",
        kind: "select",
        // Single-sourced from SOURCE_KINDS (818, F11; 717) — type & options can't drift;
        // Chinese display via optionLabels (ConfigField emits the raw wire value).
        options: SOURCE_KINDS,
        optionLabels: {
          github: "GitHub (gh)",
          azure: "Azure DevOps (az)",
          bitbucket: "Bitbucket Server (REST)",
        },
        hint: "github=gh CLI；azure=az CLI（需填下方 org/project）；bitbucket=Bitbucket Server REST（需填下方 host/project/token）",
      },
      {
        key: "azureOrg",
        label: "Azure 组织",
        kind: "text",
        // 818 F14: only shown for an Azure source; hidden (but value preserved) for github.
        visibleWhen: (p) => p.sourceKind === "azure",
        hint: "(仅 Azure 源) Azure DevOps 组织名，如 shengming0923",
      },
      {
        key: "azureProject",
        label: "Azure 项目",
        kind: "text",
        // 818 F14: only shown for an Azure source; hidden (but value preserved) for github.
        visibleWhen: (p) => p.sourceKind === "azure",
        hint: "(仅 Azure 源) Azure DevOps 项目名，如 gocell",
      },
      {
        key: "bitbucketHost",
        label: "Bitbucket Host",
        kind: "text",
        // 717: only shown for a Bitbucket source; hidden (value preserved) otherwise.
        visibleWhen: (p) => p.sourceKind === "bitbucket",
        hint: "(仅 Bitbucket 源) Server/DC 基址，如 https://bitbucket.mycompany.com",
      },
      {
        key: "bitbucketProject",
        label: "Bitbucket 项目 Key",
        kind: "text",
        // 717: only shown for a Bitbucket source; hidden (value preserved) otherwise.
        visibleWhen: (p) => p.sourceKind === "bitbucket",
        hint: "(仅 Bitbucket 源) 项目 key，如 GOCELL；个人仓库用 ~username",
      },
      {
        key: "bitbucketToken",
        label: "Bitbucket Token",
        kind: "text",
        secret: true,
        // 717: only shown for a Bitbucket source; hidden (value preserved) otherwise.
        visibleWhen: (p) => p.sourceKind === "bitbucket",
        hint: "(仅 Bitbucket 源) HTTP access token（PAT），Bearer 认证",
      },
      {
        key: "engineKind",
        label: "Review 引擎",
        kind: "select",
        // Single-sourced from ENGINE_KINDS (#718) — type & options can't drift; Chinese
        // display via optionLabels (ConfigField emits the raw camelCase wire value).
        options: ENGINE_KINDS,
        optionLabels: { codex: "Codex", claude: "Claude (claude -p)" },
        hint: "codex=codex app-server；claude=claude -p（Claude Code headless，复用 claude CLI 登录）",
      },
      {
        key: "claudeModel",
        label: "Claude 模型",
        kind: "text",
        hint: "claude -p 的 --model，如 claude-opus-4-1 / sonnet；留空=claude CLI 默认（仅 claude 引擎）",
        // claude-only: mirrors skillRelPath's codex-only visibility. Hidden (value
        // preserved) for a codex project; empty = the claude CLI's default model.
        visibleWhen: (p) => p.engineKind === "claude",
      },
      {
        key: "codexModel",
        label: "Codex 模型",
        kind: "text",
        hint: "codex turn 的 model 覆盖，如 gpt-5.1-codex；留空=codex 默认（仅 codex 引擎）",
        // codex-only: the app-server is shared, so model rides the per-turn RPC. Hidden
        // (value preserved) for a claude project; empty = codex's configured default.
        visibleWhen: (p) => p.engineKind === "codex",
      },
    ],
  },
];

// Global (non-per-project) field groups (#35): the webhook receiver + tunnel serve
// all projects, so these stay top-level on AppConfig. Keys are `keyof AppConfig`;
// the webhook keys are asserted exhaustively in fields.test.ts.
export const GLOBAL_GROUPS: FieldGroup<GlobalFieldKey>[] = [
  {
    id: "webhook",
    title: "Webhook",
    fields: [
      {
        key: "webhookEnabled",
        label: "启用 Webhook",
        kind: "checkbox",
        hint: "勾选后可启动本地接收端 + Cloudflare 隧道，GitHub 加触发标签即时触发 review",
      },
      {
        key: "webhookPort",
        label: "本地端口",
        kind: "number",
        hint: "本地 127.0.0.1 监听端口（仅经隧道公网可达）",
      },
      {
        key: "webhookSecret",
        label: "Webhook Secret",
        kind: "text",
        secret: true,
        hint: "GitHub：与仓库 webhook 的 Secret 一致（HMAC 验签）。Azure DevOps：在 Service Hook 的「HTTP headers」加一行——名 Authorization，值 Bearer <此密钥>",
      },
      {
        key: "cloudflaredBin",
        label: "cloudflared 路径",
        kind: "text",
        hint: "cloudflared 可执行文件（默认 PATH 中的 cloudflared；未安装可 brew install cloudflared）",
      },
      {
        key: "webhookTunnelMode",
        label: "隧道模式",
        kind: "select",
        // Single-sourced from the union's backing array (#50 G9) — type & options
        // can't drift. command/listener also need 下方「公网 URL」(webhookPublicUrl).
        options: WEBHOOK_TUNNEL_MODES,
        hint: "quick=零配置随机 URL（App 起 Cloudflare Quick Tunnel）；command=自定义隧道命令、固定 URL；listener=仅监听、隧道全外置。command/listener 需同时填写下方「公网 URL」",
      },
      {
        key: "webhookTunnelCommand",
        label: "隧道命令",
        kind: "text",
        hint: "command 模式：App 拉起的隧道命令，{port} 占位（如 cloudflared tunnel run my-tunnel）",
      },
      {
        key: "webhookPublicUrl",
        label: "公网 URL",
        kind: "text",
        hint: "command/listener 模式：你的固定公网根 URL，面板据此显示要粘进 GitHub Webhook / Azure Service Hook 的 Payload URL",
      },
    ],
  },
  {
    // AB#1043: 本地 REST API（给本机 CLI/curl 触发 review 用）。与 webhook 严格分离、永不走隧道。
    id: "localApi",
    title: "本地 API (CLI)",
    fields: [
      {
        key: "localApiPort",
        label: "本地 API 端口",
        kind: "number",
        // 0 = 关闭不监听 (see backend `local_api_port` semantics); allow it as the min so the
        // number input doesn't mark the documented "off" value as invalid (codex F3).
        min: 0,
        hint: "仅绑 127.0.0.1 的触发端点（curl/CLI 用）；改端口需重启 App。0=关闭不监听",
      },
      {
        key: "localApiToken",
        label: "本地 API Token",
        kind: "text",
        secret: true,
        hint: "Bearer 鉴权：请求头 Authorization: Bearer <此 token>。留空=关闭该接口（一切请求 401）；即时生效、无需重启",
      },
    ],
  },
];

// Onboarding wizard step sequence. `done` is a confirm-only step; `source` is
// confirm-only for github but gates the azure org/project for an azure source (F2) and
// the bitbucket host/project/token for a bitbucket source (717).
//
// `source` comes FIRST (717 F8): `validateStep("repo")` branches the repo-shape check on
// `draft.sourceKind` (github → owner/name; azure/bitbucket → bare slug). If `repo` were
// gated before the user picked the source, a valid Bitbucket/Azure bare slug would be
// checked against the default github `owner/name` rule and the user could never advance.
// Picking the source first means the repo step always validates against the right shape.
export type StepId = "repo" | "repoRoot" | "skill" | "source" | "autoReview" | "done";
export const STEPS: StepId[] = ["source", "repo", "repoRoot", "skill", "autoReview", "done"];

// Which per-project fields each onboarding step renders (818 F2/F3; 717). Single-sourced
// here (not in OnboardingWizard.vue) so the wizard and these unit tests agree. Keys are
// `keyof Project`; the FieldDefs come from PROJECT_GROUPS. `source` lists the azure +
// bitbucket connection fields too — they only render under their matching source via the
// FieldDef `visibleWhen` predicate (see visibleStepFields). `autoReview` surfaces
// `updateMode` (so the user picks the data-update mode during onboarding instead of
// silently defaulting, F3) and `labelSource` (717, alongside the trigger labels).
export const STEP_FIELDS: Record<StepId, ProjectFieldKey[]> = {
  repo: ["repo"],
  repoRoot: ["repoRoot"],
  skill: ["skillRelPath"],
  // source lists the azure + bitbucket connection fields too (717) — they only render
  // under their matching source via the FieldDef `visibleWhen` predicate (see
  // visibleStepFields).
  source: [
    "sourceKind",
    "azureOrg",
    "azureProject",
    "bitbucketHost",
    "bitbucketProject",
    "bitbucketToken",
  ],
  autoReview: [
    "updateMode",
    "autoReview",
    "pollIntervalSecs",
    "prCooldownSeconds",
    "labelSource",
    "reviewLabel",
    "checkLabel",
    "authors",
  ],
  done: [],
};

// All per-project FieldDefs addressable by key (PROJECT_GROUPS is the single source).
const FIELD_BY_KEY = new Map<ProjectFieldKey, FieldDef<ProjectFieldKey>>(
  PROJECT_GROUPS.flatMap((g) => g.fields).map((f) => [f.key, f]),
);

// The FieldDef for a per-project key. Throws if a STEP_FIELDS key isn't grouped (a
// coverage invariant asserted in fields.test.ts), rather than render a half-broken step.
export function fieldDefOf(key: ProjectFieldKey): FieldDef<ProjectFieldKey> {
  const def = FIELD_BY_KEY.get(key);
  if (!def) throw new Error(`missing field def: ${key}`);
  return def;
}

// The FieldDefs a wizard step should render for a given draft (818 F2): STEP_FIELDS
// filtered by each FieldDef's `visibleWhen` predicate (the same conditional-visibility
// rule ProjectCard applies in Settings), so e.g. the source step shows azureOrg/
// azureProject only for an azure source. A field without `visibleWhen` is always shown.
export function visibleStepFields(
  step: StepId,
  draft: Project,
): FieldDef<ProjectFieldKey>[] {
  return STEP_FIELDS[step]
    .map(fieldDefOf)
    .filter((f) => !f.visibleWhen || f.visibleWhen(draft));
}

// owner/name with no whitespace and exactly one slash.
const REPO_RE = /^[^/\s]+\/[^/\s]+$/;
// A bare repo slug: one or more non-slash, non-whitespace chars (azure/bitbucket repos,
// where the org/project key lives in its own field). Mirrors the backend bare-slug check.
const BARE_SLUG_RE = /^[^/\s]+$/;
// Windows drive-letter absolute path (C:\ or C:/).
const WIN_ABS_RE = /^[A-Za-z]:[\\/]/;

// Per-step frontend gate: null = ok, else a user-facing Chinese error string.
// Mirrors the backend AppError checks so most failures are caught before the
// round-trip; the backend remains the source of truth (see errorToStep). The draft
// is a single `Project` (#35) — every field it reads now lives on `Project`.
export function validateStep(step: StepId, draft: Project): string | null {
  switch (step) {
    case "repo":
      // Repo shape depends on the source (818 F4; 717): a github repo is `owner/name`; an
      // Azure repo and a Bitbucket repo are BARE slugs (no slash, no whitespace — the
      // org/project key come from the dedicated azureOrg/azureProject or bitbucketProject
      // fields), so a slash there is wrong. Mirrors the backend validate().
      if (draft.sourceKind === "azure") {
        if (draft.repo.trim() === "") return "请填写 Azure 仓库名";
        return draft.repo.includes("/")
          ? "Azure 仓库为裸名称，不含 /（org/project 在下方单独填写）"
          : null;
      }
      if (draft.sourceKind === "bitbucket") {
        if (draft.repo.trim() === "") return "请填写 Bitbucket 仓库名";
        return BARE_SLUG_RE.test(draft.repo)
          ? null
          : "Bitbucket 仓库为裸名称，不含 / 或空白（项目 key 在下方单独填写）";
      }
      return REPO_RE.test(draft.repo) ? null : "仓库需为 owner/name 格式";
    case "repoRoot":
      return draft.repoRoot.trim() !== "" ? null : "请填写本地 clone 的绝对路径";
    case "skill": {
      // codex-only (#718): the claude engine discovers `.claude/skills/` from cwd, so
      // skillRelPath is unused — skip the gate (the field is hidden via visibleWhen and
      // the backend `validate_project` skips it for a non-codex engine).
      if (draft.engineKind !== "codex") return null;
      const p = draft.skillRelPath;
      if (p.trim() === "") return "请填写 skill 相对路径";
      if (p.startsWith("/") || WIN_ABS_RE.test(p)) return "skill 必须是相对路径";
      return null;
    }
    case "source":
      // Azure source (818 F2): the backend `validate_project` requires a non-empty
      // org + project, so gate them here too (errors start with the field token so
      // errorToStep routes a backend rejection back to this step). github → nothing to
      // validate (it's the single-option confirm path).
      if (draft.sourceKind === "azure") {
        if (draft.azureOrg.trim() === "") return "azureOrg 不能为空（azure 源需填组织名）";
        if (draft.azureProject.trim() === "")
          return "azureProject 不能为空（azure 源需填项目名）";
      }
      // Bitbucket source (717): the backend `validate_project` requires a non-empty
      // host/project/token. Gate them here too; errors start with the field token so
      // errorToStep routes a backend rejection back to this step.
      if (draft.sourceKind === "bitbucket") {
        if (draft.bitbucketHost.trim() === "")
          return "bitbucketHost 不能为空（bitbucket 源需填 Server/DC 基址）";
        if (draft.bitbucketProject.trim() === "")
          return "bitbucketProject 不能为空（bitbucket 源需填项目 key）";
        if (draft.bitbucketToken.trim() === "")
          return "bitbucketToken 不能为空（bitbucket 源需填 access token）";
      }
      return null;
    case "autoReview": {
      // `Number.isFinite` rejects NaN (a blank number input yields NaN, and
      // `NaN <= 0` is false — without this guard NaN would pass the frontend gate
      // and then break the backend deserializer).
      const { pollIntervalSecs: interval, prCooldownSeconds: cooldown } = draft;
      if (
        !Number.isFinite(interval) ||
        !Number.isFinite(cooldown) ||
        interval <= 0 ||
        cooldown <= 0
      ) {
        return "轮询间隔与冷却必须大于 0";
      }
      // Labels feed `gh pr list --label`; a blank one matches nothing. Early
      // feedback here mirrors the backend validate() boundary (the source of truth).
      if (draft.reviewLabel.trim() === "" || draft.checkLabel.trim() === "") {
        return "Review 与 Check 触发标签不能为空";
      }
      // Bitbucket source (717): the backend `validate_project` enforces two extra
      // constraints, surfaced in this step (labelSource lives in the labels group;
      // updateMode in the polling group — both rendered here). Pre-gate them so the
      // user is caught before the round-trip; errors start with the field token so
      // errorToStep routes a backend rejection back to this step. Backend remains the
      // source of truth.
      if (draft.sourceKind === "bitbucket") {
        // Bitbucket Server has no native PR labels, so labels MUST come from the title.
        if (draft.labelSource !== "title") {
          return "labelSource 必须选「从标题解析」（Bitbucket 源无原生标签）";
        }
        // Bitbucket has no inbound webhook, so webhook-driven modes are invalid —
        // only pull-only / manual make sense.
        if (draft.updateMode === "webhook-only" || draft.updateMode === "hybrid") {
          return "updateMode：Bitbucket 源不支持 webhook（无入站），请选 pull-only 或 manual";
        }
      }
      return null;
    }
    case "done":
      return null;
  }
}

// Route a backend AppError message back to the wizard step that owns the field.
// The backend (src-tauri/src/config/model.rs validate()) starts each message with
// the offending field's wire name, so we match on that LEADING token, not anywhere
// in the message — `includes` would mis-route on the interpolated value (e.g. a
// repoRoot path containing "skill", or a repo value containing "repoRoot").
//
// Order matters: `skill` first (the path-escape message "skillRelPath 不能逃逸
// repoRoot" names both fields but is owned by the skill step); then `repoRoot`
// before `repo` (since "repoRoot" itself starts with "repo"). Returns null for
// unrecognized messages so the caller falls back to `done`.
//
// This is the downstream of the cross-end routing funnel (PR #41 F4, Medium): the
// field tokens here mirror the message prefixes the backend emits. The upstream is
// locked by `validate_error_messages_start_with_routing_field_token` in model.rs
// (asserts each message starts with its token); the downstream cases are locked in
// fields.test.ts. Drift on either side fails CI. Future Hard path (issue): codegen
// the tokens / a structured `{ field }` error so the contract can't be expressed
// wrong at all — keep both ends in sync until then.
//
// Webhook fields (`webhookSecret` / `webhookPort`) are deliberately NOT routed here:
// they are Settings-only (no onboarding step owns them — see STEPS), and `webhook_enabled`
// defaults false so the wizard's save never triggers their `validate()` errors. They
// therefore fall through to `null` (→ done) by design; SettingsView surfaces those
// backend errors directly without `errorToStep`. Locked by an explicit
// "webhook messages → null" case in fields.test.ts so this stays intentional, not a gap.
//
// Azure fields (`azureOrg` / `azureProject`, 818 F2): the wizard `source` step NOW owns
// them — it renders them (visibleWhen sourceKind==="azure") and `validateStep("source")`
// gates them — so a backend rejection routes BACK to the source step (not null). Both
// tokens are azure-prefixed and collide with no other field token, so order among them
// is moot. Locked by an "azure messages → source" case in fields.test.ts.
//
// Bitbucket fields (`bitbucketHost` / `bitbucketProject` / `bitbucketToken`, 717): same
// story — owned by the source step (visibleWhen sourceKind==="bitbucket", gated by
// validateStep) so backend rejections route to source. `labelSource` and `updateMode`
// (717) route to the autoReview step: labelSource sits beside its labels-group siblings
// (reviewLabel/checkLabel), and updateMode sits in the polling group — both surfaced in
// the autoReview step, where the backend's Bitbucket-only constraints (labelSource must be
// "title"; updateMode may not be webhook-only/hybrid) are also pre-gated by validateStep.
// Locked by fields.test.ts.
export function errorToStep(message: string): StepId | null {
  const m = message.trimStart();
  if (m.startsWith("skill")) return "skill";
  if (m.startsWith("repoRoot")) return "repoRoot";
  if (m.startsWith("repo")) return "repo";
  if (m.startsWith("azureOrg") || m.startsWith("azureProject")) return "source";
  // Bitbucket connection fields (717) are owned by the source step alongside the azure
  // fields (rendered via visibleWhen sourceKind==="bitbucket", gated by validateStep).
  if (
    m.startsWith("bitbucketHost") ||
    m.startsWith("bitbucketProject") ||
    m.startsWith("bitbucketToken")
  ) {
    return "source";
  }
  if (
    m.startsWith("pollIntervalSecs") ||
    m.startsWith("prCooldownSeconds") ||
    m.startsWith("reviewLabel") ||
    m.startsWith("checkLabel") ||
    // labelSource (717) lives in the labels group, surfaced in the autoReview step
    // alongside reviewLabel/checkLabel (see STEP_FIELDS).
    m.startsWith("labelSource") ||
    // updateMode (717) lives in the polling group, surfaced in the autoReview step;
    // the backend rejects webhook-only/hybrid for a Bitbucket source (no inbound
    // webhook), so its rejection routes back here.
    m.startsWith("updateMode")
  ) {
    return "autoReview";
  }
  return null;
}
