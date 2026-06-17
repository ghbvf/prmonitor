// Single field-definition source (#34): drives BOTH the grouped Settings form and
// the onboarding wizard, so the two surfaces never drift on labels/kinds/options.
// Plus two pure helpers — `validateStep` (per-step frontend gate) and `errorToStep`
// (route a backend AppError back to the wizard step that owns the offending field).
// Kept side-effect-free: no Pinia, no Vue — unit-tested in `fields.test.ts`.
import { WEBHOOK_TUNNEL_MODES, type AppConfig } from "./types";

export type FieldKey = keyof AppConfig;
type FieldKind = "text" | "number" | "csv" | "select" | "checkbox";

export interface FieldDef {
  key: FieldKey;
  label: string;
  kind: FieldKind;
  hint?: string;
  // Only meaningful for kind "select"; mirrors the SourceKind/EngineKind arms.
  options?: readonly string[];
  // Reserved single-option enums (#11) are shown but not editable.
  readonly?: boolean;
  // text fields holding a credential (e.g. the webhook HMAC secret): rendered
  // masked (type=password) with a reveal toggle so it isn't exposed in screenshots
  // / screen-shares.
  secret?: boolean;
}

interface FieldGroup {
  id: string;
  title: string;
  fields: FieldDef[];
}

// Every AppConfig key appears exactly once across the groups (asserted in
// fields.test.ts), so adding a config field forces a home here.
export const GROUPS: FieldGroup[] = [
  {
    id: "project",
    title: "项目",
    fields: [
      { key: "repo", label: "仓库", kind: "text", hint: "owner/name，如 ghbvf/prmonitor" },
      { key: "repoRoot", label: "本地路径", kind: "text", hint: "本地 clone 的绝对路径" },
      {
        key: "skillRelPath",
        label: "Skill 路径",
        kind: "text",
        hint: "相对仓库根的 skill 路径，如 .codex/skills/pr-review/SKILL.md",
      },
    ],
  },
  {
    id: "polling",
    title: "轮询",
    fields: [
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
        hint: "与 GitHub 仓库 webhook 的 Secret 一致；用于 HMAC 验签",
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
        hint: "command/listener 模式：你的固定公网根 URL，面板据此显示要粘进 GitHub 的 Payload URL",
      },
    ],
  },
  {
    id: "labels",
    title: "标签",
    fields: [
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
      // Single-arm enums today; widening tracked by #11 — kept read-only so the UI
      // never offers an option the backend can't honor.
      {
        key: "sourceKind",
        label: "PR 来源",
        kind: "select",
        options: ["github"],
        readonly: true,
        hint: "暂仅支持 github（#11）",
      },
      {
        key: "engineKind",
        label: "Review 引擎",
        kind: "select",
        options: ["codex"],
        readonly: true,
        hint: "暂仅支持 codex（#11）",
      },
    ],
  },
];

// Onboarding wizard step sequence. `source`/`done` are confirm-only steps.
export type StepId = "repo" | "repoRoot" | "skill" | "source" | "autoReview" | "done";
export const STEPS: StepId[] = ["repo", "repoRoot", "skill", "source", "autoReview", "done"];

// owner/name with no whitespace and exactly one slash.
const REPO_RE = /^[^/\s]+\/[^/\s]+$/;
// Windows drive-letter absolute path (C:\ or C:/).
const WIN_ABS_RE = /^[A-Za-z]:[\\/]/;

// Per-step frontend gate: null = ok, else a user-facing Chinese error string.
// Mirrors the backend AppError checks so most failures are caught before the
// round-trip; the backend remains the source of truth (see errorToStep).
export function validateStep(step: StepId, draft: AppConfig): string | null {
  switch (step) {
    case "repo":
      return REPO_RE.test(draft.repo) ? null : "仓库需为 owner/name 格式";
    case "repoRoot":
      return draft.repoRoot.trim() !== "" ? null : "请填写本地 clone 的绝对路径";
    case "skill": {
      const p = draft.skillRelPath;
      if (p.trim() === "") return "请填写 skill 相对路径";
      if (p.startsWith("/") || WIN_ABS_RE.test(p)) return "skill 必须是相对路径";
      return null;
    }
    case "source":
      // Single-option confirm step — nothing to validate.
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
export function errorToStep(message: string): StepId | null {
  const m = message.trimStart();
  if (m.startsWith("skill")) return "skill";
  if (m.startsWith("repoRoot")) return "repoRoot";
  if (m.startsWith("repo")) return "repo";
  if (
    m.startsWith("pollIntervalSecs") ||
    m.startsWith("prCooldownSeconds") ||
    m.startsWith("reviewLabel") ||
    m.startsWith("checkLabel")
  ) {
    return "autoReview";
  }
  return null;
}
