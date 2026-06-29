//! Config slice domain model.

use std::path::Path;

use base64::{engine::general_purpose, Engine as _};
use lettre::message::Mailbox;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{AppError, AppResult};
use crate::model::{
    EngineKind, EventType, LabelSource, MessagingProviderKind, NotificationKind,
    ReviewLifecycleEvent, SourceKind, UpdateMode, WebhookTunnelMode,
};

/// One monitored project (#35). What was previously the flat per-repo subset of
/// [`AppConfig`] is now a list element: each project carries its own repo, paths,
/// labels, source/engine kind, and auto-review toggle, so the app can poll and
/// review several repos in parallel. Global webhook/shell settings stay on
/// [`AppConfig`] (one receiver serves all projects).
///
/// `#[serde(default)]` mirrors [`AppConfig`]'s forward-compat contract: a project
/// object missing fields fills them from [`Default`]. Field defaults match the
/// historical single-project [`AppConfig`] defaults (the gocell repo the app was
/// built to serve) so a migrated config keeps identical behavior.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Project {
    /// Stable identifier (the migration assigns `"default"` to the lifted
    /// single-project config; new projects get a fresh id). Used as the routing
    /// key on every PR/review event and by [`super::service::project`] lookup.
    pub id: String,
    /// Human-readable label shown in the project switcher.
    pub name: String,
    /// Whether this project is polled/reviewed. Disabled projects are skipped by
    /// `validate` (their fields are not checked) and by the scheduler.
    pub enabled: bool,
    /// Monitored repo. Shape is source-dependent: `owner/name` for
    /// [`SourceKind::Github`]; a bare repository name/slug for [`SourceKind::Azure`]
    /// (org/project come from `azure_org`/`azure_project`) and [`SourceKind::Bitbucket`]
    /// (project key comes from `bitbucket_project`). Validated per-source in `validate_project`.
    pub repo: String,
    /// Absolute path to the local clone codex runs the pr-review skill against.
    pub repo_root: String,
    /// Scheduled-pull period in seconds.
    pub poll_interval_secs: u64,
    /// PR author allowlist (mirrors the dispatcher's author gate).
    pub authors: Vec<String>,
    /// Path (relative to `repo_root`) of the codex pr-review skill to invoke.
    pub skill_rel_path: String,
    /// Per-PR cooldown between dispatches of the same `(pr, kind)`.
    pub pr_cooldown_seconds: u64,
    /// Which PR source backs the monitor: [`SourceKind::Github`] (`gh` CLI),
    /// [`SourceKind::Azure`] (`az` CLI), or [`SourceKind::Bitbucket`] (REST). #11
    /// reservation: future variant gates GitLab.
    pub source_kind: SourceKind,
    /// Which review engine runs against a PR. #11 reservation: today only
    /// [`EngineKind::Codex`]; future variant gates Claude.
    pub engine_kind: EngineKind,
    /// 手填的 codex 模型名（仅 [`EngineKind::Codex`] 用）。非空时作为 `turn/start` 的
    /// `model` 覆盖（codex app-server 是单进程，模型只能 per-turn 选）；留空=codex 默认。
    /// 自由文本不校验（模型列表多变）。
    pub codex_model: String,
    /// 手填的 claude 模型名（仅 [`EngineKind::Claude`] 用）。非空时作为 `claude -p` 的
    /// `--model` 参数；留空=claude CLI 默认。自由文本不校验。
    pub claude_model: String,
    /// 本项目 PR 列表的更新模式（#818）。默认 [`UpdateMode::WebhookOnly`]：**启动不自动
    /// 轮询 CLI**，列表仅由 webhook 推送更新。`pull-only`/`hybrid` 才起周期轮询；`manual`
    /// 只在「立即拉取」时跑一次性发现。调度器据此 gate 是否为本项目起轮询 loop。
    pub update_mode: UpdateMode,
    /// Azure DevOps 组织名（#818，仅 [`SourceKind::Azure`] 用）。`az repos pr list
    /// --organization https://dev.azure.com/<org>` 的 `<org>`。GitHub 源留空。
    pub azure_org: String,
    /// Azure DevOps 项目名（#818，仅 [`SourceKind::Azure`] 用）。`az repos pr list
    /// --project <project>` 的 `<project>`。GitHub 源留空。
    pub azure_project: String,
    /// 标签来源（AB#717）：`native`=用来源方自带的 PR 标签；`title`=从 PR 标题的方括号
    /// 片段解析（如 `[pr-status/need-fix]`）。默认 [`LabelSource::Native`]（向后兼容）。
    /// Bitbucket Server 无原生 PR 标签，故 [`SourceKind::Bitbucket`] 项目**必须**用
    /// [`LabelSource::Title`]（由 [`validate_project`] 强制）。
    pub label_source: LabelSource,
    /// Bitbucket Server/DC 基址（AB#717，仅 [`SourceKind::Bitbucket`] 用），如
    /// `https://bitbucket.mycompany.com`。被插值进 REST URL，故须 URL 安全。其余源留空。
    pub bitbucket_host: String,
    /// Bitbucket 项目 key（AB#717，仅 [`SourceKind::Bitbucket`] 用），如 `GOCELL`；个人
    /// 仓库用 `~username`。REST 路径段 `.../projects/{project}/repos/{repo}/...`。其余源留空。
    pub bitbucket_project: String,
    /// Bitbucket HTTP access token（PAT，AB#717，仅 [`SourceKind::Bitbucket`] 用），用作
    /// `Authorization: Bearer <token>`。无 CLI 登录，故凭据存配置。其余源留空。
    pub bitbucket_token: String,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            repo: "ghbvf/gocell".to_string(),
            repo_root: String::new(),
            poll_interval_secs: 120,
            authors: Vec::new(),
            skill_rel_path: ".codex/skills/pr-review/SKILL.md".to_string(),
            pr_cooldown_seconds: 1800,
            source_kind: SourceKind::default(),
            engine_kind: EngineKind::default(),
            // 模型留空 = 各引擎用自身默认（不注入 --model / turn model）。
            codex_model: String::new(),
            claude_model: String::new(),
            // #818: boot defaults to webhook-only — NO automatic CLI polling at startup
            // (the scheduler does not start a loop for this mode). Flip-back guarded by
            // `default_update_mode_is_webhook_only`.
            update_mode: UpdateMode::WebhookOnly,
            azure_org: String::new(),
            azure_project: String::new(),
            // AB#717: default to the status-quo (provider's own PR labels). A Bitbucket
            // project must override this to `Title` (enforced by `validate_project`).
            label_source: LabelSource::default(),
            bitbucket_host: String::new(),
            bitbucket_project: String::new(),
            bitbucket_token: String::new(),
        }
    }
}

/// Default staleness TTL for a `notification` action (seconds) = 2h (AB#1182). A persisted
/// `pending` notification older than this (by `created_at`) is dead-lettered instead of fired, so a
/// restart that drains hours-old rows doesn't surface ghost "review 完成" notifications. Single-
/// sourced here so [`OutboxConfig::default`] and the worker's load-failure fallback (which reads
/// `AppConfig::default().outbox`) agree on one value.
pub const DEFAULT_NOTIFICATION_TTL_SECS: u64 = 2 * 60 * 60;
const MAX_CONFIGURED_DELAY_SECS: u64 = 365 * 24 * 60 * 60;

/// Outbox worker policy (AB#1182): the per-kind staleness TTLs for the durable action queue.
/// GLOBAL (one policy serves every project) on purpose — staleness is an infrastructure concern,
/// not project domain, and the worker's `claim_due` drains every project's `pending` rows in one
/// batch (a per-project TTL would also have no project to resolve for the project-less deeplink
/// notifications). `#[serde(default)]` keeps it forward-compatible: a config persisted before this
/// struct existed (or a partial `{"outbox":{}}`) fills absent fields from [`Default`] rather than
/// failing to load.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OutboxConfig {
    /// Staleness TTL for a `notification` action (seconds). A `pending` notification whose
    /// `created_at` is older than this is dead-lettered instead of executed. `0` DISABLES the TTL
    /// (the action never expires) — a safe sentinel so a misconfigured `0` can't expire every
    /// queued action instantly. The per-kind resolver in `outbox::service` (`ttl_secs`, an
    /// exhaustive `match ActionKind`) is the Hard carrier forcing a new kind to declare its own TTL.
    pub notification_ttl_secs: u64,
}

impl Default for OutboxConfig {
    fn default() -> Self {
        Self {
            notification_ttl_secs: DEFAULT_NOTIFICATION_TTL_SECS,
        }
    }
}

/// The action a rule produces when it matches an inbound event (#1371).
///
/// Exhaustive routing is intentionally split: the rule engine plans one or more
/// [`RuleActionKind`] values, then the outbox executor's existing exhaustive
/// `match ActionKind` in `lib.rs` remains the Hard carrier for actual side effects.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleActionKind {
    Review,
    Check,
    Notify,
}

#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum RuleActionDedupePolicy {
    #[default]
    Event,
    Action,
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum RuleActionTarget {
    #[default]
    None,
    NotificationChannels {
        #[serde(rename = "channelIds")]
        channel_ids: Vec<String>,
    },
    MessagingConversation {
        #[serde(rename = "integrationId")]
        integration_id: String,
        #[serde(rename = "conversationId")]
        conversation_id: String,
    },
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RuleActionConfig {
    pub id: String,
    pub kind: RuleActionKind,
    pub enabled: bool,
    pub target: RuleActionTarget,
    pub dedupe_policy: RuleActionDedupePolicy,
    pub delay_secs: u64,
    pub depends_on: Vec<String>,
    pub level: String,
}

impl RuleActionConfig {
    pub fn new(id: impl Into<String>, kind: RuleActionKind) -> Self {
        Self {
            id: id.into(),
            kind,
            enabled: true,
            target: RuleActionTarget::None,
            dedupe_policy: RuleActionDedupePolicy::default(),
            delay_secs: 0,
            depends_on: Vec::new(),
            level: "action".to_string(),
        }
    }
}

impl Default for RuleActionConfig {
    fn default() -> Self {
        Self::new(String::new(), RuleActionKind::Review)
    }
}

/// One configurable event→action rule (#1371).
///
/// Empty matcher fields are wildcards. Text matchers are case-insensitive `contains`;
/// no regex/expr/DSL is accepted in v1. This keeps the form shape small and makes the
/// matcher deterministic and testable.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct RuleConfig {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub source: Option<SourceKind>,
    pub event_type: Option<EventType>,
    pub project_id: String,
    pub repo: String,
    pub labels_any: Vec<String>,
    pub labels_all: Vec<String>,
    pub title_contains: String,
    pub body_contains: String,
    pub actions: Vec<RuleActionConfig>,
    pub allow_action_kinds: Vec<RuleActionKind>,
    pub deny_action_kinds: Vec<RuleActionKind>,
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ReviewLifecycleTarget {
    NotificationChannels {
        #[serde(rename = "channelIds")]
        channel_ids: Vec<String>,
    },
    MessagingConversation {
        #[serde(rename = "integrationId")]
        integration_id: String,
        #[serde(rename = "conversationId")]
        conversation_id: String,
    },
}

impl Default for ReviewLifecycleTarget {
    fn default() -> Self {
        Self::NotificationChannels {
            channel_ids: Vec::new(),
        }
    }
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReviewLifecycleNotificationConfig {
    pub enabled: bool,
    pub events: Vec<ReviewLifecycleEvent>,
    pub targets: Vec<ReviewLifecycleTarget>,
    pub start_delay_secs: u64,
    pub end_delay_secs: u64,
}

impl Default for ReviewLifecycleNotificationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            events: vec![
                ReviewLifecycleEvent::Started,
                ReviewLifecycleEvent::Completed,
                ReviewLifecycleEvent::Failed,
                ReviewLifecycleEvent::Interrupted,
            ],
            targets: vec![ReviewLifecycleTarget::default()],
            start_delay_secs: 0,
            end_delay_secs: 0,
        }
    }
}

/// Global notification delivery configuration (AB#1459). The outbox stores only a channel id and
/// kind in its panel-visible payload; adapters live-load this config at execution time.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NotificationSettings {
    pub channels: Vec<NotificationChannel>,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            channels: vec![NotificationChannel::desktop_default()],
        }
    }
}

/// Global bidirectional messaging/bot integration configuration (#1559).
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MessagingSettings {
    pub integrations: Vec<MessagingIntegration>,
}

/// One configured bidirectional messaging integration (#1559).
///
/// The shape is intentionally flat like [`NotificationChannel`]: Settings edits whole
/// `AppConfig` snapshots and should preserve provider-specific hidden fields while validation uses
/// exhaustive `kind` matches to decide which fields are meaningful.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MessagingIntegration {
    pub id: String,
    pub name: String,
    pub kind: MessagingProviderKind,
    pub enabled: bool,
    pub verification_token: String,
    pub encrypt_key: String,
    pub app_id: String,
    pub app_secret: String,
    pub bot_open_id: String,
    /// Stable provider conversation ids allowed to execute commands. Empty fail-closes when enabled.
    pub allowed_conversation_ids: Vec<String>,
    /// Whether group chat events must mention the bot before command parsing.
    pub require_mention: bool,
    pub timeout_secs: u64,
}

impl MessagingIntegration {
    pub fn feishu_default() -> Self {
        Self {
            id: String::new(),
            name: "飞书".to_string(),
            kind: MessagingProviderKind::Feishu,
            enabled: false,
            verification_token: String::new(),
            encrypt_key: String::new(),
            app_id: String::new(),
            app_secret: String::new(),
            bot_open_id: String::new(),
            allowed_conversation_ids: Vec::new(),
            require_mention: true,
            timeout_secs: DEFAULT_NOTIFICATION_TIMEOUT_SECS,
        }
    }
}

impl Default for MessagingIntegration {
    fn default() -> Self {
        Self::feishu_default()
    }
}

/// One configured outbound notification channel.
///
/// The shape is intentionally flat instead of a serde-tagged enum because the existing Settings
/// screen edits whole `AppConfig` snapshots. A flat shape lets the frontend keep hidden fields
/// round-tripped while the backend's exhaustive `match kind` in [`validate_notification_channel`]
/// decides which fields are meaningful for each channel.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NotificationChannel {
    pub id: String,
    pub name: String,
    pub kind: NotificationKind,
    pub enabled: bool,
    pub webhook_url: String,
    pub webhook_secret: String,
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_username: String,
    pub smtp_password: String,
    pub smtp_from: String,
    pub smtp_to: String,
    pub timeout_secs: u64,
}

impl NotificationChannel {
    pub fn desktop_default() -> Self {
        Self {
            id: "desktop".to_string(),
            name: "Desktop".to_string(),
            kind: NotificationKind::Desktop,
            enabled: true,
            webhook_url: String::new(),
            webhook_secret: String::new(),
            telegram_bot_token: String::new(),
            telegram_chat_id: String::new(),
            smtp_host: String::new(),
            smtp_port: 587,
            smtp_username: String::new(),
            smtp_password: String::new(),
            smtp_from: String::new(),
            smtp_to: String::new(),
            timeout_secs: DEFAULT_NOTIFICATION_TIMEOUT_SECS,
        }
    }
}

#[allow(clippy::derivable_impls)]
impl Default for NotificationChannel {
    fn default() -> Self {
        Self {
            enabled: false,
            ..Self::desktop_default()
        }
    }
}

pub const DEFAULT_NOTIFICATION_TIMEOUT_SECS: u64 = 15;

/// Persisted application configuration (#35: multi-project). Holds the list of
/// monitored [`Project`]s plus the GLOBAL webhook/shell settings (one webhook
/// receiver serves every project).
///
/// `#[serde(default)]` makes deserialization forward-compatible: a persisted
/// config missing fields (older versions, or before a #11 field is added) fills
/// absent fields from [`Default`] instead of failing. Do not add
/// `#[serde(deny_unknown_fields)]` — it would break that forward-compat. The
/// legacy flat single-project shape is upgraded by `super::service::migrate_value`
/// before deserialization, so old stored configs still load.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppConfig {
    /// Monitored projects. Empty on first launch (onboarding then adds the first).
    pub projects: Vec<Project>,
    /// `id` of the project the UI currently focuses. Empty when `projects` is empty.
    pub active_project_id: String,
    /// 是否启用 webhook 接收端（#9）。开关本身只 gate「能否启动」本地接收端 + Cloudflare
    /// 隧道（手动 `start_webhook` 命令）——不自动起、不影响轮询路径。
    pub webhook_enabled: bool,
    /// webhook 本地 HTTP 监听端口（仅绑 127.0.0.1；公网经 cloudflared 隧道代理到此）。
    pub webhook_port: u16,
    /// GitHub webhook 的 HMAC secret（`X-Hub-Signature-256` 验签）。`webhook_enabled`
    /// 时必填——公网端点没有验签即可被任意 POST 伪造触发 review。
    pub webhook_secret: String,
    /// `cloudflared` 可执行文件（PATH 名或绝对路径）。Quick Tunnel 子进程由此拉起。
    pub cloudflared_bin: String,
    /// 隧道暴露模式（#9）。`quick`=Cloudflare Quick Tunnel（默认，现状）；
    /// `command`=自定义隧道命令（见 `webhook_tunnel_command`）；`listener`=只监听本地端口、
    /// 隧道完全外置。接收端只管 bind/验签/派发，隧道如何暴露公网由本字段分支。
    pub webhook_tunnel_mode: WebhookTunnelMode,
    /// `command` 模式下拉起的自定义隧道命令（按空白分词；首 token 为程序、其余为参数；
    /// 字面量 `{port}` 替换成实际监听端口）。**直接 exec、不过 shell**（防注入）。仅
    /// `command` 模式必填（由 `validate` 强制）；其他模式留空。
    pub webhook_tunnel_command: String,
    /// `command` / `listener` 模式下手填的公网 URL 根（隧道由外部暴露，接收端无从抓取）。
    /// 非空则作为 `WebhookStatus.public_url`，UI 据此拼出 GitHub Payload URL；为空则 `None`。
    /// `quick` 模式忽略本字段（URL 从 cloudflared 日志抓取）。
    pub webhook_public_url: String,
    /// 本地 REST API 的 Bearer token（AB#1043）。**空 = fail-closed 禁用**：entrypoint 仍绑定，但
    /// handler 每个请求实时读取本字段做常量时间比较，空 token 一律 401。设 / 清即时生效、无需
    /// 重启。loopback-only 仍是触发端点（本机任意进程 + DNS rebinding 可达），故非空时按
    /// `LOCAL_API_TOKEN_MIN_LEN` 强制最小长度（与 `webhook_secret` 同理由）。
    pub local_api_token: String,
    /// Outbox worker policy (AB#1182): per-kind staleness TTLs for the durable action queue. Global
    /// (one policy serves every project). Forward-compat via the nested `#[serde(default)]`.
    pub outbox: OutboxConfig,
    /// Configurable Event → Action rules (#1371). This replaces the old per-project
    /// `reviewLabel` / `checkLabel` / `autoReview` runtime path; old persisted fields are
    /// migrated into rules on load.
    pub rules: Vec<RuleConfig>,
    /// Outbound notification channels (AB#1459). Forward-compatible default seeds a local desktop
    /// channel; external channels are user-added/disabled until configured.
    pub notifications: NotificationSettings,
    /// Bidirectional messaging/bot integrations (#1559). Separate from outbound notifications.
    pub messaging: MessagingSettings,
    /// Review lifecycle notifications. Routes review started / terminal events to notification
    /// channels or messaging conversations through the durable outbox.
    pub review_lifecycle_notifications: ReviewLifecycleNotificationConfig,
    /// Declarative Remote Access entrypoints and tunnels. This is the single runtime source:
    /// each entrypoint owns one bound port and mounts one or more capability routes.
    pub remote_access: RemoteAccessConfig,
    /// Legacy pre-#1553 remote-access fields. Kept only so old tests / migration helpers can build;
    /// they are never serialized, deserialized, generated to TS, or consumed by the runtime.
    #[serde(skip)]
    #[cfg_attr(test, ts(skip))]
    pub listeners: Vec<Listener>,
    #[serde(skip)]
    #[cfg_attr(test, ts(skip))]
    pub tunnels: Vec<Tunnel>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            projects: Vec::new(),
            active_project_id: String::new(),
            webhook_enabled: false,
            webhook_port: 8787,
            webhook_secret: String::new(),
            cloudflared_bin: "cloudflared".to_string(),
            webhook_tunnel_mode: WebhookTunnelMode::default(),
            webhook_tunnel_command: String::new(),
            webhook_public_url: String::new(),
            local_api_token: String::new(),
            outbox: OutboxConfig::default(),
            rules: Vec::new(),
            notifications: NotificationSettings::default(),
            messaging: MessagingSettings::default(),
            review_lifecycle_notifications: ReviewLifecycleNotificationConfig::default(),
            remote_access: RemoteAccessConfig::default(),
            listeners: Vec::new(),
            tunnels: Vec::new(),
        }
    }
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RemoteAccessConfig {
    pub entrypoints: Vec<RemoteEntrypoint>,
    pub tunnels: Vec<RemoteTunnel>,
}

impl Default for RemoteAccessConfig {
    fn default() -> Self {
        Self {
            entrypoints: vec![RemoteEntrypoint::default_local_api()],
            tunnels: Vec::new(),
        }
    }
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RemoteEntrypoint {
    pub id: String,
    pub name: String,
    pub bind_host: String,
    pub port: u16,
    pub enabled: bool,
    pub source_policy: SourcePolicy,
    pub allowed_origins: Vec<String>,
    pub trusted_proxies: Vec<String>,
    pub routes: Vec<RemoteRoute>,
}

impl RemoteEntrypoint {
    pub fn default_local_api() -> Self {
        Self {
            id: "local-api".to_string(),
            name: "Local API".to_string(),
            bind_host: "127.0.0.1".to_string(),
            port: 8788,
            enabled: true,
            source_policy: SourcePolicy::default(),
            allowed_origins: Vec::new(),
            trusted_proxies: Vec::new(),
            routes: vec![RemoteRoute::local_api()],
        }
    }
}

impl Default for RemoteEntrypoint {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            bind_host: "127.0.0.1".to_string(),
            port: 0,
            enabled: false,
            source_policy: SourcePolicy::default(),
            allowed_origins: Vec::new(),
            trusted_proxies: Vec::new(),
            routes: Vec::new(),
        }
    }
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RemoteRoute {
    pub id: String,
    pub name: String,
    pub path: String,
    pub capability: RemoteCapability,
    pub enabled: bool,
    pub auth_token: String,
    pub terminal_read: bool,
    pub terminal_write: bool,
    pub terminal_create: bool,
    pub terminal_admin: bool,
}

impl RemoteRoute {
    pub fn local_api() -> Self {
        Self {
            id: "local-api".to_string(),
            name: "Local API".to_string(),
            path: "/api".to_string(),
            capability: RemoteCapability::LocalApi,
            enabled: true,
            auth_token: String::new(),
            terminal_read: false,
            terminal_write: false,
            terminal_create: false,
            terminal_admin: false,
        }
    }

    pub fn terminal() -> Self {
        Self {
            id: "terminal".to_string(),
            name: "Terminal".to_string(),
            path: "/terminal".to_string(),
            capability: RemoteCapability::Terminal,
            enabled: true,
            auth_token: String::new(),
            terminal_read: true,
            terminal_write: false,
            terminal_create: false,
            terminal_admin: false,
        }
    }

    pub fn messaging() -> Self {
        Self {
            id: "messaging".to_string(),
            name: "Messaging".to_string(),
            path: "/messaging".to_string(),
            capability: RemoteCapability::Messaging,
            enabled: true,
            auth_token: String::new(),
            terminal_read: false,
            terminal_write: false,
            terminal_create: false,
            terminal_admin: false,
        }
    }
}

impl Default for RemoteRoute {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            path: "/terminal".to_string(),
            capability: RemoteCapability::Terminal,
            enabled: false,
            auth_token: String::new(),
            terminal_read: false,
            terminal_write: false,
            terminal_create: false,
            terminal_admin: false,
        }
    }
}

#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteCapability {
    #[default]
    Terminal,
    LocalApi,
    Messaging,
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SourcePolicy {
    pub mode: SourcePolicyMode,
    pub allow: Vec<String>,
}

#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SourcePolicyMode {
    #[default]
    Loopback,
    Lan,
    Custom,
}

#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RemoteTunnel {
    pub id: String,
    pub name: String,
    pub mode: RemoteTunnelMode,
    pub target_entrypoint_id: String,
    pub bind_host: String,
    pub port: u16,
    pub command: String,
    pub public_url: String,
    pub enabled: bool,
}

impl Default for RemoteTunnel {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            mode: RemoteTunnelMode::Quick,
            target_entrypoint_id: String::new(),
            bind_host: "0.0.0.0".to_string(),
            port: 0,
            command: String::new(),
            public_url: String::new(),
            enabled: false,
        }
    }
}

#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteTunnelMode {
    #[default]
    Quick,
    Command,
    Listener,
    Lan,
}

/// Listener kind (AB#1064). kebab-case wire values mirror the work item's literal naming.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ListenerKind {
    #[default]
    LocalApi, // "local-api"
    RemoteWeb,    // "remote-web"
    EventIngress, // "event-ingress"
    Terminal,     // "terminal"
}

/// Per-listener auth mode. Terminal listeners require bearer auth plus a strong `authToken`;
/// local-api still uses the legacy global `local_api_token` at runtime.
#[cfg_attr(test, derive(ts_rs::TS, strum::EnumIter))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ListenerAuthMode {
    #[default]
    None, // "none"
    Bearer, // "bearer"
}

/// A declarative network listener descriptor. Supported kinds are bound by the remote supervisor
/// and must remain loopback-only.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Listener {
    pub id: String,
    pub name: String,
    pub kind: ListenerKind,
    pub bind_host: String,
    pub port: u16,
    pub enabled: bool,
    pub auth: ListenerAuthMode,
    pub auth_token: String,
    pub terminal_read: bool,
    pub terminal_write: bool,
    pub terminal_create: bool,
    pub terminal_admin: bool,
    pub allowed_origins: Vec<String>,
    pub public_url: String,
}

/// Legacy pre-#1553 tunnel descriptor, kept as migration input only.
#[cfg_attr(test, derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Tunnel {
    pub id: String,
    pub name: String,
    pub mode: WebhookTunnelMode,
    pub target_listener_id: String,
    pub command: String,
    pub public_url: String,
    pub enabled: bool,
}

/// Minimum `webhook_secret` length (trimmed chars) when the receiver is enabled. The
/// secret is the SOLE gate on a public HMAC-SHA256 endpoint, so a 1–2 char value is
/// brute-forceable; require a floor (GitHub recommends a long random secret).
const WEBHOOK_SECRET_MIN_LEN: usize = 16;

/// Minimum `local_api_token` length (trimmed chars) when the local REST API token is set
/// (AB#1043). The local API binds loopback-only, but a low-entropy token is still
/// brute-forceable by any local process (and reachable cross-site via DNS rebinding before
/// the Host gate), so a non-empty token must clear the same floor as `webhook_secret`. An
/// EMPTY token is the "disabled" sentinel (fail-closed 401), so it is exempt from this check.
const LOCAL_API_TOKEN_MIN_LEN: usize = 16;

/// Terminal routes can be exposed through public tunnels, so their bearer token floor is higher than
/// the loopback-only local API token. Medium enforcement: validate + runtime guard + tests.
const REMOTE_TERMINAL_TOKEN_MIN_LEN: usize = 32;

pub(crate) fn terminal_auth_token_is_strong(token: &str) -> bool {
    token.trim().chars().count() >= REMOTE_TERMINAL_TOKEN_MIN_LEN
}

fn is_https_url_without_userinfo(value: &str) -> bool {
    Url::parse(value.trim())
        .map(|u| u.scheme() == "https" && u.username().is_empty() && u.password().is_none())
        .unwrap_or(false)
}

fn is_valid_mailbox(value: &str) -> bool {
    value.trim().parse::<Mailbox>().is_ok()
}

pub(crate) fn validate_notification_channel(channel: &NotificationChannel) -> AppResult<()> {
    if channel.id.trim().is_empty()
        || channel.id.contains(':')
        || channel.id.chars().any(char::is_whitespace)
    {
        return Err(AppError::new(format!(
            "notificationChannelId 非法（不能为空、含 `:` 或空白字符）: {:?}",
            channel.id
        )));
    }
    if channel.timeout_secs == 0 {
        return Err(AppError::new(format!(
            "notificationTimeoutSecs 必须大于 0（通知渠道「{}」）",
            channel.name
        )));
    }
    if !channel.enabled {
        return Ok(());
    }

    match channel.kind {
        NotificationKind::Desktop => Ok(()),
        NotificationKind::Email => {
            if channel.smtp_host.trim().is_empty() {
                return Err(AppError::new(format!(
                    "smtpHost 不能为空（通知渠道「{}」）",
                    channel.name
                )));
            }
            if channel.smtp_port == 0 {
                return Err(AppError::new(format!(
                    "smtpPort 必须大于 0（通知渠道「{}」）",
                    channel.name
                )));
            }
            if channel.smtp_from.trim().is_empty() {
                return Err(AppError::new(format!(
                    "smtpFrom 不能为空（通知渠道「{}」）",
                    channel.name
                )));
            }
            if !is_valid_mailbox(&channel.smtp_from) {
                return Err(AppError::new(format!(
                    "smtpFrom 必须是有效邮件地址（通知渠道「{}」）",
                    channel.name
                )));
            }
            if channel.smtp_to.trim().is_empty() {
                return Err(AppError::new(format!(
                    "smtpTo 不能为空（通知渠道「{}」）",
                    channel.name
                )));
            }
            if channel
                .smtp_to
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .any(|addr| !is_valid_mailbox(addr))
            {
                return Err(AppError::new(format!(
                    "smtpTo 必须是逗号分隔的有效邮件地址（通知渠道「{}」）",
                    channel.name
                )));
            }
            Ok(())
        }
        NotificationKind::Slack
        | NotificationKind::WeChatWork
        | NotificationKind::Feishu
        | NotificationKind::DingTalk => {
            if channel.webhook_url.trim().is_empty() {
                return Err(AppError::new(format!(
                    "notificationWebhookUrl 不能为空（通知渠道「{}」）",
                    channel.name
                )));
            }
            if !is_https_url_without_userinfo(&channel.webhook_url) {
                return Err(AppError::new(format!(
                    "notificationWebhookUrl 必须是 https:// URL 且不能内嵌 userinfo（通知渠道「{}」）",
                    channel.name
                )));
            }
            Ok(())
        }
        NotificationKind::Telegram => {
            if channel.telegram_bot_token.trim().is_empty() {
                return Err(AppError::new(format!(
                    "telegramBotToken 不能为空（通知渠道「{}」）",
                    channel.name
                )));
            }
            if channel.telegram_chat_id.trim().is_empty() {
                return Err(AppError::new(format!(
                    "telegramChatId 不能为空（通知渠道「{}」）",
                    channel.name
                )));
            }
            Ok(())
        }
    }
}

fn validate_notifications(settings: &NotificationSettings) -> AppResult<()> {
    let mut seen_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for channel in &settings.channels {
        let id = channel.id.trim();
        if !seen_ids.insert(id) {
            return Err(AppError::new(format!(
                "notificationChannelId 重复: {id}（每个通知渠道 id 必须唯一）"
            )));
        }
        validate_notification_channel(channel)?;
    }
    Ok(())
}

fn validate_wechat_work_encoding_aes_key(value: &str) -> AppResult<()> {
    let padded = match value.len() % 4 {
        0 => value.to_string(),
        n => format!("{value}{}", "=".repeat(4 - n)),
    };
    let decoded = general_purpose::STANDARD
        .decode(padded)
        .map_err(|e| AppError::new(format!("weChatWorkEncodingAesKey base64 解码失败: {e}")))?;
    if decoded.len() != 32 {
        return Err(AppError::new("weChatWorkEncodingAesKey 必须解码为 32 字节"));
    }
    Ok(())
}

fn validate_messaging(settings: &MessagingSettings) -> AppResult<()> {
    let mut seen_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for integration in &settings.integrations {
        let id = integration.id.trim();
        if id.is_empty() || id.contains(':') || id.chars().any(char::is_whitespace) {
            return Err(AppError::new(format!(
                "messagingIntegrationId 非法（不能为空、含 `:` 或空白字符）: {:?}",
                integration.id
            )));
        }
        if !seen_ids.insert(id) {
            return Err(AppError::new(format!(
                "messagingIntegrationId 重复: {id}（每个消息集成 id 必须唯一）"
            )));
        }
        if integration.timeout_secs == 0 {
            return Err(AppError::new(format!(
                "messagingTimeoutSecs 必须大于 0（消息集成「{}」）",
                integration.name
            )));
        }
        if !integration.enabled {
            continue;
        }
        if integration.allowed_conversation_ids.is_empty()
            || integration
                .allowed_conversation_ids
                .iter()
                .any(|value| value.trim().is_empty())
        {
            return Err(AppError::new(format!(
                "messagingAllowedConversationIds 不能为空（启用消息集成「{}」时必须显式允许会话）",
                integration.name
            )));
        }
        match integration.kind {
            MessagingProviderKind::Feishu => {
                if integration.verification_token.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "feishuVerificationToken 不能为空（消息集成「{}」）",
                        integration.name
                    )));
                }
                if integration.verification_token.trim().chars().count() < WEBHOOK_SECRET_MIN_LEN {
                    return Err(AppError::new(format!(
                        "feishuVerificationToken 太短（至少 {WEBHOOK_SECRET_MIN_LEN} 个字符；消息集成「{}」）",
                        integration.name
                    )));
                }
                if integration.encrypt_key.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "feishuEncryptKey 不能为空（消息集成「{}」）",
                        integration.name
                    )));
                }
                if integration.encrypt_key.trim().chars().count() < WEBHOOK_SECRET_MIN_LEN {
                    return Err(AppError::new(format!(
                        "feishuEncryptKey 太短（至少 {WEBHOOK_SECRET_MIN_LEN} 个字符；消息集成「{}」）",
                        integration.name
                    )));
                }
                if integration.app_id.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "feishuAppId 不能为空（消息集成「{}」）",
                        integration.name
                    )));
                }
                if integration.app_secret.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "feishuAppSecret 不能为空（消息集成「{}」）",
                        integration.name
                    )));
                }
                if integration.require_mention && integration.bot_open_id.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "feishuBotOpenId 不能为空（消息集成「{}」启用 @Bot 触发时必须配置）",
                        integration.name
                    )));
                }
            }
            MessagingProviderKind::WeChatWork => {
                if integration.verification_token.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "weChatWorkToken 不能为空（消息集成「{}」）",
                        integration.name
                    )));
                }
                if integration.verification_token.trim().chars().count() < WEBHOOK_SECRET_MIN_LEN {
                    return Err(AppError::new(format!(
                        "weChatWorkToken 太短（至少 {WEBHOOK_SECRET_MIN_LEN} 个字符；消息集成「{}」）",
                        integration.name
                    )));
                }
                if integration.encrypt_key.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "weChatWorkEncodingAesKey 不能为空（消息集成「{}」）",
                        integration.name
                    )));
                }
                validate_wechat_work_encoding_aes_key(integration.encrypt_key.trim())?;
                if integration.app_id.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "weChatWorkCorpId 不能为空（消息集成「{}」）",
                        integration.name
                    )));
                }
                if integration.app_secret.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "weChatWorkCorpSecret 不能为空（消息集成「{}」）",
                        integration.name
                    )));
                }
                if integration.bot_open_id.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "weChatWorkAgentId 不能为空（消息集成「{}」）",
                        integration.name
                    )));
                }
            }
            MessagingProviderKind::DingTalk => {
                if integration.verification_token.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "dingTalkToken 不能为空（消息集成「{}」）",
                        integration.name
                    )));
                }
                if integration.verification_token.trim().chars().count() < WEBHOOK_SECRET_MIN_LEN {
                    return Err(AppError::new(format!(
                        "dingTalkToken 太短（至少 {WEBHOOK_SECRET_MIN_LEN} 个字符；消息集成「{}」）",
                        integration.name
                    )));
                }
                if integration.app_secret.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "dingTalkAppSecret 不能为空（消息集成「{}」）",
                        integration.name
                    )));
                }
                if integration.bot_open_id.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "dingTalkRobotCode 不能为空（消息集成「{}」）",
                        integration.name
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Validates one [`Project`]'s fields (hard-reject on failure).
///
/// The `repo` (and, for Azure, `azureOrg` / `azureProject`) check branches on
/// `source_kind` (#818) via an EXHAUSTIVE `match` (no wildcard), so a new
/// [`SourceKind`] variant fails to compile until its repo-shape rule is added:
/// - [`SourceKind::Github`]: `repo` is `owner/name` (one slash, both sides
///   non-empty, no whitespace — the backend boundary `gh pr list --repo` consumes,
///   mirroring the frontend `REPO_RE`).
/// - [`SourceKind::Azure`]: `repo` is a bare non-empty repository name, and
///   `azure_org` / `azure_project` are non-empty (`az repos pr list --organization
///   <org> --project <project> --repository <repo>` consumes all three).
///
/// The source-agnostic checks then run: `repo_root` is a non-empty, **absolute**
/// path to an existing directory (absolute so resolution never depends on the
/// process CWD, matching the field's doc contract); `skill_rel_path` resolves to an
/// existing file that stays **inside** `repo_root` (the skill check defends two
/// `Path::join` pitfalls: an absolute `skill_rel_path` would discard `repo_root`,
/// and `..` traversal could escape the clone — both would let a later engine read
/// arbitrary files); positive poll/cooldown intervals; and non-empty
/// `review_label` / `check_label` (each is fed to the source's label filter, so a
/// blank one makes every poll match nothing / fail).
///
/// Errors funnel through [`AppError`], and each message **starts with** the
/// offending field's wire name (`repo` / `azureOrg` / `azureProject` /
/// `bitbucketHost` / `bitbucketProject` / `bitbucketToken` / `labelSource` /
/// `repoRoot` / `skillRelPath` / `skill` / `pollIntervalSecs` / `prCooldownSeconds` /
/// `reviewLabel` / `checkLabel`). That prefix is the cross-end routing contract the
/// onboarding wizard's `errorToStep` (src/config/fields.ts) keys on — locked at
/// this end by the `validate_error_*` test below (PR #41 F4, Medium). Checks run in
/// wizard-step order so the first failure routes to the earliest offending step.
pub fn validate_project(project: &Project) -> AppResult<()> {
    // Repo-shape check branches on the source (#818). EXHAUSTIVE match (no wildcard):
    // a new `SourceKind` variant fails to compile here until its repo rule is added.
    match project.source_kind {
        SourceKind::Github => {
            // owner/name: exactly one slash, both sides non-empty, no whitespace anywhere
            // (mirrors the frontend REPO_RE `^[^/\s]+\/[^/\s]+$`).
            let repo_parts: Vec<&str> = project.repo.split('/').collect();
            let repo_ok = repo_parts.len() == 2
                && !repo_parts[0].is_empty()
                && !repo_parts[1].is_empty()
                && !project.repo.chars().any(char::is_whitespace);
            if !repo_ok {
                return Err(AppError::new(format!(
                    "repo 必须是 owner/name 格式: {}",
                    project.repo
                )));
            }
        }
        SourceKind::Azure => {
            // Azure: org/project/repo are three separate `az repos pr list` args, so the
            // repo is a bare non-empty name (NOT owner/name). All three must be non-empty;
            // messages keep the camelCase field prefix the wizard routes on.
            if project.azure_org.trim().is_empty() {
                return Err(AppError::new("azureOrg 不能为空（Azure 源必填）"));
            }
            // `azure_org` ALONE is interpolated into the discovery URL
            // (`https://dev.azure.com/{org}`), so it must be URL-safe (#818 F2): reject
            // whitespace, path/query/userinfo delimiters (`/ # ? @`), and control chars —
            // any would split the URL or smuggle a different host/path. Azure org names are
            // restricted to URL-safe characters anyway, so this rejects nothing legitimate.
            // `azure_project` / `repo` are NOT char-checked: they go as separate argv (no
            // URL/shell parsing) and Azure project/repo names may contain spaces / unicode.
            if project
                .azure_org
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '#' | '?' | '@'))
            {
                return Err(AppError::new(format!(
                    "azureOrg 含非法字符（不能有空白、控制字符或 / # ? @）: {}",
                    project.azure_org
                )));
            }
            if project.azure_project.trim().is_empty() {
                return Err(AppError::new("azureProject 不能为空（Azure 源必填）"));
            }
            if project.repo.trim().is_empty() {
                return Err(AppError::new("repo 不能为空（Azure 源必填仓库名）"));
            }
        }
        SourceKind::Bitbucket => {
            // AB#717: host/project/repo/token are separate REST inputs, so the repo is a
            // bare non-empty slug (NOT owner/name). Messages keep the camelCase field
            // prefix the wizard routes on.
            if project.bitbucket_host.trim().is_empty() {
                return Err(AppError::new("bitbucketHost 不能为空（Bitbucket 源必填）"));
            }
            // `bitbucket_host` is interpolated into the REST URL base, so it must be
            // URL-safe (parity with the azure_org guard #818 F2): reject whitespace,
            // control chars, and query/fragment/userinfo delimiters (`# ? @`) that would
            // split the URL or smuggle a different target. `/` and `:` ARE allowed (the
            // host is a full base URL like `https://bitbucket.example.com:7990/ctx`).
            if project
                .bitbucket_host
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '#' | '?' | '@'))
            {
                return Err(AppError::new(format!(
                    "bitbucketHost 含非法字符（不能有空白、控制字符或 # ? @）: {}",
                    project.bitbucket_host
                )));
            }
            // The Bearer PAT must never traverse a plaintext connection, so the host MUST be
            // an `https://` URL. The char check above allows `:` / `/` (a base URL needs them),
            // so it can't catch an `http://` scheme — this does. (The client also sets
            // `https_only(true)` as belt-and-braces.)
            if !project
                .bitbucket_host
                .trim()
                .to_ascii_lowercase()
                .starts_with("https://")
            {
                return Err(AppError::new(format!(
                    "bitbucketHost 必须是 https:// 开头的 URL（Bearer 凭据不能走明文 HTTP）: {}",
                    project.bitbucket_host
                )));
            }
            if project.bitbucket_project.trim().is_empty() {
                return Err(AppError::new(
                    "bitbucketProject 不能为空（Bitbucket 源必填）",
                ));
            }
            // `bitbucket_project` and `repo` are interpolated as URL PATH SEGMENTS
            // (`.../projects/{project}/repos/{repo}/...`), so a `/` would forge extra path
            // segments and `# ? @` would split the URL — reject them (plus whitespace /
            // control). `~` (personal project key) is unreserved and allowed.
            if project
                .bitbucket_project
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '#' | '?' | '@'))
            {
                return Err(AppError::new(format!(
                    "bitbucketProject 含非法字符（不能有空白、控制字符或 / # ? @）: {}",
                    project.bitbucket_project
                )));
            }
            if project.bitbucket_token.trim().is_empty() {
                return Err(AppError::new("bitbucketToken 不能为空（Bitbucket 源必填）"));
            }
            if project.repo.trim().is_empty() {
                return Err(AppError::new("repo 不能为空（Bitbucket 源必填仓库 slug）"));
            }
            if project
                .repo
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '#' | '?' | '@'))
            {
                return Err(AppError::new(format!(
                    "repo 含非法字符（Bitbucket 仓库 slug 不能有空白、控制字符或 / # ? @）: {}",
                    project.repo
                )));
            }
            // Bitbucket Server PRs carry NO native labels, so `Native` would never match a
            // trigger label → nothing is ever monitored. Require `Title` (parse labels from
            // the PR title). Machine-checkable robust constraint, not a comment-only rule.
            if project.label_source != LabelSource::Title {
                return Err(AppError::new(
                    "labelSource 必须为「从标题解析」（Bitbucket 源无原生标签）",
                ));
            }
            // Bitbucket has NO inbound webhook (poll/API discovery only), so webhook-only /
            // hybrid would never update the list (silent dead config — the default is
            // webhook-only). Require a polling mode (pull-only or manual).
            if matches!(
                project.update_mode,
                UpdateMode::WebhookOnly | UpdateMode::Hybrid
            ) {
                return Err(AppError::new(
                    "updateMode：Bitbucket 源无入站 webhook，请改用 pull-only 或 manual",
                ));
            }
        }
    }

    let repo_root = project.repo_root.trim();
    let root = Path::new(repo_root);
    if repo_root.is_empty() || !root.is_absolute() || !root.is_dir() {
        return Err(AppError::new(format!(
            "repoRoot 必须是存在的绝对目录路径: {}",
            project.repo_root
        )));
    }

    // `skillRelPath` is the codex pr-review skill path (`.codex/skills/...`) the codex
    // engine attaches to a turn. The claude engine (#718) discovers `.claude/skills/`
    // from the turn cwd instead, so the field is unused for it — validate it ONLY for
    // a codex project, mirroring how source-specific fields (azure org/project,
    // bitbucket host/project/token) are validated only under their matching source.
    // A claude project keeps whatever value sits in `skill_rel_path` (the field is
    // hidden in the UI via `engineKind==="codex"` visibleWhen) but it is never used.
    if project.engine_kind == EngineKind::Codex {
        let skill_rel = Path::new(&project.skill_rel_path);
        if skill_rel.is_absolute() {
            return Err(AppError::new(format!(
                "skillRelPath 必须是相对路径: {}",
                project.skill_rel_path
            )));
        }
        let skill = root.join(skill_rel);
        if !skill.is_file() {
            return Err(AppError::new(format!(
                "skill 路径不存在: {}",
                skill.display()
            )));
        }
        let root_canon = root
            .canonicalize()
            .map_err(|e| AppError::new(format!("repoRoot 规范化失败: {e}")))?;
        let skill_canon = skill
            .canonicalize()
            .map_err(|e| AppError::new(format!("skill 路径规范化失败: {e}")))?;
        if !skill_canon.starts_with(&root_canon) {
            return Err(AppError::new(format!(
                "skillRelPath 不能逃逸 repoRoot: {}",
                project.skill_rel_path
            )));
        }
    }

    if project.poll_interval_secs == 0 {
        return Err(AppError::new("pollIntervalSecs 必须大于 0"));
    }
    if project.pr_cooldown_seconds == 0 {
        return Err(AppError::new("prCooldownSeconds 必须大于 0"));
    }

    Ok(())
}

fn validate_rule(
    rule: &RuleConfig,
    notifications: &NotificationSettings,
    messaging: &MessagingSettings,
) -> AppResult<()> {
    if rule.id.trim().is_empty()
        || rule.id.contains(':')
        || rule.id.chars().any(char::is_whitespace)
    {
        return Err(AppError::new(format!(
            "id 非法（不能为空、含 `:` 或空白字符）: {:?}",
            rule.id
        )));
    }
    if rule.name.trim().is_empty() {
        return Err(AppError::new("name 不能为空"));
    }
    if rule.enabled && rule.actions.is_empty() {
        return Err(AppError::new(
            "actions 不能为空（启用规则至少需要一个动作）",
        ));
    }
    let mut seen_action_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (idx, action) in rule.actions.iter().enumerate() {
        validate_rule_action(action, notifications, messaging)
            .map_err(|e| AppError::new(format!("actions[{idx}].{}", e.message)))?;
        if !seen_action_ids.insert(action.id.as_str()) {
            return Err(AppError::new(format!(
                "actions[{idx}].id 动作 id 重复: {}",
                action.id
            )));
        }
        if rule.deny_action_kinds.contains(&action.kind)
            || (!rule.allow_action_kinds.is_empty()
                && !rule.allow_action_kinds.contains(&action.kind))
        {
            return Err(AppError::new(format!(
                "actions[{idx}].kind 被组合策略拒绝: {:?}",
                action.kind
            )));
        }
    }
    validate_rule_action_dag(&rule.actions)?;
    Ok(())
}

fn validate_rule_action(
    action: &RuleActionConfig,
    notifications: &NotificationSettings,
    messaging: &MessagingSettings,
) -> AppResult<()> {
    if action.id.trim().is_empty()
        || action.id.contains(':')
        || action.id.chars().any(char::is_whitespace)
    {
        return Err(AppError::new(format!(
            "id 非法（不能为空、含 `:` 或空白字符）: {:?}",
            action.id
        )));
    }
    if action.level.trim().is_empty()
        || action.level.contains(':')
        || action.level.chars().any(char::is_whitespace)
    {
        return Err(AppError::new(format!(
            "level 非法（不能为空、含 `:` 或空白字符）: {:?}",
            action.level
        )));
    }
    validate_delay_secs("delaySecs", action.delay_secs)?;
    validate_rule_action_target(&action.target, notifications, messaging)?;
    validate_id_list("dependsOn", &action.depends_on)?;
    Ok(())
}

fn validate_rule_action_target(
    target: &RuleActionTarget,
    notifications: &NotificationSettings,
    messaging: &MessagingSettings,
) -> AppResult<()> {
    match target {
        RuleActionTarget::None => {}
        RuleActionTarget::NotificationChannels { channel_ids } => {
            validate_id_list("target.channelIds", channel_ids)?;
            if channel_ids.is_empty()
                && !notifications.channels.iter().any(|channel| channel.enabled)
            {
                return Err(AppError::new(
                    "target.channelIds 未指定且没有启用的通知渠道",
                ));
            }
            for channel_id in channel_ids {
                let Some(channel) = notifications
                    .channels
                    .iter()
                    .find(|channel| channel.id == *channel_id)
                else {
                    return Err(AppError::new(format!(
                        "target.channelIds 不存在: {channel_id}"
                    )));
                };
                if !channel.enabled {
                    return Err(AppError::new(format!(
                        "target.channelIds 已禁用: {channel_id}"
                    )));
                }
            }
        }
        RuleActionTarget::MessagingConversation {
            integration_id,
            conversation_id,
        } => {
            if integration_id.trim().is_empty() {
                return Err(AppError::new("target.integrationId 不能为空"));
            }
            if conversation_id.trim().is_empty() {
                return Err(AppError::new("target.conversationId 不能为空"));
            }
            let Some(integration) = messaging
                .integrations
                .iter()
                .find(|integration| integration.id == *integration_id)
            else {
                return Err(AppError::new(format!(
                    "target.integrationId 不存在: {integration_id}"
                )));
            };
            if !integration.enabled {
                return Err(AppError::new(format!(
                    "target.integrationId 已禁用: {integration_id}"
                )));
            }
            if !integration
                .allowed_conversation_ids
                .iter()
                .any(|allowed| allowed.trim() == conversation_id.trim())
            {
                return Err(AppError::new(format!(
                    "target.conversationId 未授权: {conversation_id}"
                )));
            }
        }
    }
    Ok(())
}

fn validate_delay_secs(field: &str, delay_secs: u64) -> AppResult<()> {
    if delay_secs > MAX_CONFIGURED_DELAY_SECS {
        return Err(AppError::new(format!(
            "{field} 不能超过 {MAX_CONFIGURED_DELAY_SECS} 秒"
        )));
    }
    Ok(())
}

fn validate_id_list(field: &str, ids: &[String]) -> AppResult<()> {
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        let id = id.trim();
        if id.is_empty() {
            return Err(AppError::new(format!("{field} 不能包含空值")));
        }
        if !seen.insert(id.to_string()) {
            return Err(AppError::new(format!("{field} 不能包含重复值: {id}")));
        }
    }
    Ok(())
}

fn validate_rule_action_dag(actions: &[RuleActionConfig]) -> AppResult<()> {
    let ids: std::collections::HashSet<&str> =
        actions.iter().map(|action| action.id.as_str()).collect();
    for action in actions {
        for dep in &action.depends_on {
            if dep == &action.id {
                return Err(AppError::new(format!("actions DAG 自依赖: {}", action.id)));
            }
            if !ids.contains(dep.as_str()) {
                return Err(AppError::new(format!(
                    "actions DAG 依赖未知动作: {} -> {}",
                    action.id, dep
                )));
            }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        New,
        Visiting,
        Done,
    }
    let mut marks: std::collections::HashMap<&str, Mark> = actions
        .iter()
        .map(|action| (action.id.as_str(), Mark::New))
        .collect();
    let by_id: std::collections::HashMap<&str, &RuleActionConfig> = actions
        .iter()
        .map(|action| (action.id.as_str(), action))
        .collect();

    fn visit<'a>(
        id: &'a str,
        marks: &mut std::collections::HashMap<&'a str, Mark>,
        by_id: &std::collections::HashMap<&'a str, &'a RuleActionConfig>,
    ) -> AppResult<()> {
        match marks.get(id).copied().unwrap_or(Mark::New) {
            Mark::Done => return Ok(()),
            Mark::Visiting => {
                return Err(AppError::new(format!("actions DAG 存在环路: {id}")));
            }
            Mark::New => {}
        }
        marks.insert(id, Mark::Visiting);
        if let Some(action) = by_id.get(id) {
            for dep in &action.depends_on {
                visit(dep, marks, by_id)?;
            }
        }
        marks.insert(id, Mark::Done);
        Ok(())
    }

    for action in actions {
        visit(action.id.as_str(), &mut marks, &by_id)?;
    }
    Ok(())
}

fn validate_review_lifecycle_notifications(
    config: &ReviewLifecycleNotificationConfig,
    notifications: &NotificationSettings,
    messaging: &MessagingSettings,
) -> AppResult<()> {
    if !config.enabled {
        return Ok(());
    }
    validate_review_lifecycle_events(&config.events)?;
    validate_delay_secs(
        "reviewLifecycleNotifications.startDelaySecs",
        config.start_delay_secs,
    )?;
    validate_delay_secs(
        "reviewLifecycleNotifications.endDelaySecs",
        config.end_delay_secs,
    )?;
    if config.enabled && config.targets.is_empty() {
        return Err(AppError::new(
            "reviewLifecycleNotifications.targets 不能为空（启用后至少需要一个目标）",
        ));
    }
    for (idx, target) in config.targets.iter().enumerate() {
        match target {
            ReviewLifecycleTarget::NotificationChannels { channel_ids } => {
                if channel_ids.is_empty() {
                    if !notifications.channels.iter().any(|channel| channel.enabled) {
                        return Err(AppError::new(format!(
                            "reviewLifecycleNotifications.targets[{idx}].channelIds 未指定且没有启用的通知渠道"
                        )));
                    }
                } else {
                    validate_id_list(
                        &format!("reviewLifecycleNotifications.targets[{idx}].channelIds"),
                        channel_ids,
                    )?;
                }
                for channel_id in channel_ids {
                    let Some(channel) = notifications
                        .channels
                        .iter()
                        .find(|channel| channel.id == *channel_id)
                    else {
                        return Err(AppError::new(format!(
                            "reviewLifecycleNotifications.targets[{idx}].channelIds 不存在: {channel_id}"
                        )));
                    };
                    if !channel.enabled {
                        return Err(AppError::new(format!(
                            "reviewLifecycleNotifications.targets[{idx}].channelIds 已禁用: {channel_id}"
                        )));
                    }
                }
            }
            ReviewLifecycleTarget::MessagingConversation {
                integration_id,
                conversation_id,
            } => {
                if integration_id.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "reviewLifecycleNotifications.targets[{idx}].integrationId 不能为空"
                    )));
                }
                if conversation_id.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "reviewLifecycleNotifications.targets[{idx}].conversationId 不能为空"
                    )));
                }
                let Some(integration) = messaging
                    .integrations
                    .iter()
                    .find(|integration| integration.id == *integration_id)
                else {
                    return Err(AppError::new(format!(
                        "reviewLifecycleNotifications.targets[{idx}].integrationId 不存在: {integration_id}"
                    )));
                };
                if !integration.enabled {
                    return Err(AppError::new(format!(
                        "reviewLifecycleNotifications.targets[{idx}].integrationId 已禁用: {integration_id}"
                    )));
                }
                if !integration
                    .allowed_conversation_ids
                    .iter()
                    .any(|allowed| allowed.trim() == conversation_id.trim())
                {
                    return Err(AppError::new(format!(
                        "reviewLifecycleNotifications.targets[{idx}].conversationId 未授权: {conversation_id}"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_review_lifecycle_events(events: &[ReviewLifecycleEvent]) -> AppResult<()> {
    if events.is_empty() {
        return Err(AppError::new(
            "reviewLifecycleNotifications.events 不能为空",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for event in events {
        if !seen.insert(*event) {
            return Err(AppError::new(format!(
                "reviewLifecycleNotifications.events 不能重复: {:?}",
                event
            )));
        }
    }
    Ok(())
}

pub(crate) fn normalize_route_path(path: &str) -> AppResult<String> {
    let trimmed = path.trim().trim_end_matches('/');
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return Err(AppError::new(format!(
            "routePath 必须以 / 开头且不能为空: {:?}",
            path
        )));
    }
    if trimmed == "/" || trimmed.split('/').skip(1).any(|seg| seg.is_empty()) {
        return Err(AppError::new(format!(
            "routePath 不能为根路径且不能包含空路径段: {trimmed}"
        )));
    }
    if !trimmed
        .split('/')
        .skip(1)
        .all(|seg| seg.bytes().all(is_route_path_segment_byte))
    {
        return Err(AppError::new(format!(
            "routePath 只能包含 URL path-safe 字符（A-Z a-z 0-9 . _ ~ -）: {trimmed}"
        )));
    }
    Ok(trimmed.to_string())
}

fn is_route_path_segment_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-')
}

pub(crate) fn route_paths_conflict(a: &str, b: &str) -> bool {
    a == b
        || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
}

fn parse_ip_or_cidr(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let Some((ip, prefix)) = trimmed.split_once('/') else {
        return false;
    };
    let Ok(addr) = ip.parse::<std::net::IpAddr>() else {
        return false;
    };
    let Ok(prefix) = prefix.parse::<u8>() else {
        return false;
    };
    match addr {
        std::net::IpAddr::V4(_) => prefix <= 32,
        std::net::IpAddr::V6(_) => prefix <= 128,
    }
}

fn validate_source_policy(policy: &SourcePolicy, label: &str) -> AppResult<()> {
    match policy.mode {
        SourcePolicyMode::Loopback | SourcePolicyMode::Lan => {
            if !policy.allow.is_empty() {
                return Err(AppError::new(format!(
                    "sourcePolicy allow 仅 custom 模式可填写（{label}）"
                )));
            }
        }
        SourcePolicyMode::Custom => {
            if policy.allow.is_empty() {
                return Err(AppError::new(format!(
                    "sourcePolicy custom 模式必须填写至少一个 IP/CIDR（{label}）"
                )));
            }
            for item in &policy.allow {
                if !parse_ip_or_cidr(item) {
                    return Err(AppError::new(format!(
                        "sourcePolicy 包含非法 IP/CIDR（{label}）: {item}"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_remote_access(config: &AppConfig) -> AppResult<()> {
    let remote_access = &config.remote_access;

    let mut ports: std::collections::HashMap<u16, String> = std::collections::HashMap::new();
    let mut claim = |port: u16, label: String| -> AppResult<()> {
        if let Some(existing) = ports.insert(port, label.clone()) {
            return Err(AppError::new(format!(
                "port 冲突：端口 {port} 被 {existing} 与 {label} 同时占用（启用的监听端口必须互不相同）"
            )));
        }
        Ok(())
    };
    if config.webhook_enabled {
        claim(config.webhook_port, "webhookPort".to_string())?;
    }

    let mut enabled_entrypoints: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut entrypoint_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for entrypoint in &remote_access.entrypoints {
        let id = entrypoint.id.trim();
        if id.is_empty() {
            return Err(AppError::new(format!(
                "entrypointId 不能为空（入口「{}」需要一个唯一 id）",
                entrypoint.name
            )));
        }
        if !entrypoint_ids.insert(id) {
            return Err(AppError::new(format!(
                "entrypointId 重复: {id}（每个远程入口的 id 必须唯一）"
            )));
        }
        if !entrypoint.enabled {
            continue;
        }
        enabled_entrypoints.insert(id);
        if entrypoint.port == 0 {
            return Err(AppError::new(format!(
                "port 必须大于 0（入口「{}」已启用但端口为 0/未设置）",
                entrypoint.name
            )));
        }
        if entrypoint.bind_host.trim().is_empty() {
            return Err(AppError::new(format!(
                "bindHost 不能为空（入口「{}」已启用）",
                entrypoint.name
            )));
        }
        validate_source_policy(&entrypoint.source_policy, &entrypoint.name)?;
        for proxy in &entrypoint.trusted_proxies {
            if !parse_ip_or_cidr(proxy) {
                return Err(AppError::new(format!(
                    "trustedProxies 包含非法 IP/CIDR（入口「{}」）: {proxy}",
                    entrypoint.name
                )));
            }
        }
        claim(
            entrypoint.port,
            format!("远程入口「{}」", entrypoint.name.trim()),
        )?;

        let mut route_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut paths: Vec<String> = Vec::new();
        for route in &entrypoint.routes {
            let route_id = route.id.trim();
            if route_id.is_empty() {
                return Err(AppError::new(format!(
                    "routeId 不能为空（入口「{}」中的路由需要一个唯一 id）",
                    entrypoint.name
                )));
            }
            if !route_ids.insert(route_id) {
                return Err(AppError::new(format!(
                    "routeId 重复: {route_id}（入口「{}」内路由 id 必须唯一）",
                    entrypoint.name
                )));
            }
            if !route.enabled {
                continue;
            }
            let path = normalize_route_path(&route.path)?;
            if paths
                .iter()
                .any(|existing| route_paths_conflict(existing, &path))
            {
                return Err(AppError::new(format!(
                    "routePath 冲突（入口「{}」中存在重复或前缀重叠）: {path}",
                    entrypoint.name
                )));
            }
            paths.push(path);
            match route.capability {
                RemoteCapability::Terminal => {
                    if !terminal_auth_token_is_strong(&route.auth_token) {
                        return Err(AppError::new(format!(
                            "authToken 终端路由「{}」必须配置至少 {REMOTE_TERMINAL_TOKEN_MIN_LEN} 个字符的随机 Bearer token",
                            route.name
                        )));
                    }
                    if !route.terminal_read {
                        return Err(AppError::new(format!(
                            "terminalRead 终端路由「{}」必须至少开启读取权限",
                            route.name
                        )));
                    }
                }
                RemoteCapability::LocalApi | RemoteCapability::Messaging => {}
            }
        }
    }

    let mut tunnel_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for tunnel in &remote_access.tunnels {
        let id = tunnel.id.trim();
        if id.is_empty() {
            return Err(AppError::new(format!(
                "tunnelId 不能为空（隧道「{}」需要一个唯一 id）",
                tunnel.name
            )));
        }
        if !tunnel_ids.insert(id) {
            return Err(AppError::new(format!(
                "tunnelId 重复: {id}（每个隧道的 id 必须唯一）"
            )));
        }
        let public_url = tunnel.public_url.trim();
        if !public_url.is_empty() && !is_https_url_without_userinfo(public_url) {
            return Err(AppError::new(format!(
                "publicUrl 必须是 https:// 开头的 URL（隧道「{}」，不能走明文 HTTP/不能内嵌凭据）: {public_url}",
                tunnel.name
            )));
        }
        if !tunnel.enabled {
            continue;
        }
        let target = tunnel.target_entrypoint_id.trim();
        if target.is_empty() {
            return Err(AppError::new(format!(
                "targetEntrypointId 不能为空（隧道「{}」已启用，需指定目标入口）",
                tunnel.name
            )));
        }
        if !enabled_entrypoints.contains(target) {
            return Err(AppError::new(format!(
                "targetEntrypointId 不指向已启用入口（隧道「{}」: {target}）",
                tunnel.name
            )));
        }
        match tunnel.mode {
            RemoteTunnelMode::Command => {
                if tunnel.command.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "command 不能为空（隧道「{}」为 command 模式时需填写隧道命令，可用 {{port}} 占位）",
                        tunnel.name
                    )));
                }
            }
            RemoteTunnelMode::Listener => {}
            RemoteTunnelMode::Quick => {}
            RemoteTunnelMode::Lan => {
                if tunnel.port == 0 {
                    return Err(AppError::new(format!(
                        "port 必须大于 0（LAN 隧道「{}」已启用但端口为 0/未设置）",
                        tunnel.name
                    )));
                }
                if tunnel.bind_host.trim().is_empty() {
                    return Err(AppError::new(format!(
                        "bindHost 不能为空（LAN 隧道「{}」已启用）",
                        tunnel.name
                    )));
                }
                claim(tunnel.port, format!("LAN 隧道「{}」", tunnel.name.trim()))?;
            }
        }
    }
    Ok(())
}

/// Validates the whole [`AppConfig`] before persisting (hard-reject on failure).
///
/// Validates the GLOBAL webhook fields (only when the receiver is enabled), then
/// every `enabled` [`Project`] via [`validate_project`], and rejects duplicate
/// project `id`s or `repo`s (each would make event routing / dedup ambiguous). An
/// empty `projects` list (first launch, before onboarding adds one) is VALID —
/// onboarding is the gate that fills it.
pub fn validate(config: &AppConfig) -> AppResult<()> {
    // Webhook fields are only constrained when the receiver is enabled: a public
    // endpoint (reached via the cloudflared tunnel) MUST have a secret or any POST
    // could forge a review trigger; a zero port can't bind. Disabled → unconstrained
    // (defaults stay valid). Messages keep the field-token prefix the wizard's
    // `errorToStep` contract relies on (locked by `validate_error_messages_*`).
    if config.webhook_enabled {
        let secret = config.webhook_secret.trim();
        if secret.is_empty() {
            return Err(AppError::new(
                "webhookSecret 不能为空（启用 webhook 时必填）",
            ));
        }
        // Minimum strength (F8): empty-only was insufficient — a low-entropy secret on a
        // public endpoint is brute-forceable. Count chars on the trimmed value so leading/
        // trailing whitespace can't pad a weak secret to length.
        if secret.chars().count() < WEBHOOK_SECRET_MIN_LEN {
            return Err(AppError::new(format!(
                "webhookSecret 太短（至少 {WEBHOOK_SECRET_MIN_LEN} 个字符；请使用更长的随机串）"
            )));
        }
        if config.webhook_port == 0 {
            return Err(AppError::new("webhookPort 必须大于 0"));
        }
        // `command` 模式必须有命令可 spawn；空命令到 `start()` 会被防御性短路成 AppError，
        // 在此上游拦住给出可路由的字段前缀错误（与 `quick`/`listener` 无关，故仅此分支约束）。
        if config.webhook_tunnel_mode == WebhookTunnelMode::Command
            && config.webhook_tunnel_command.trim().is_empty()
        {
            return Err(AppError::new(
                "webhookTunnelCommand 不能为空（command 模式需填隧道命令，可用 {port} 占位）",
            ));
        }
    }

    // AB#1043 local REST API token: unlike webhook there is no enable flag — an EMPTY token
    // is itself the "disabled" sentinel (the resident listener fail-closes 401), so blank is
    // always valid. Only a NON-empty token is constrained: it must clear the same length
    // floor as `webhook_secret` (a short token is brute-forceable by a local process). Message
    // keeps the `localApiToken` field-token prefix for consistency with the other rules.
    let local_token = config.local_api_token.trim();
    if !local_token.is_empty() && local_token.chars().count() < LOCAL_API_TOKEN_MIN_LEN {
        return Err(AppError::new(format!(
            "localApiToken 太短（至少 {LOCAL_API_TOKEN_MIN_LEN} 个字符；请使用更长的随机串）"
        )));
    }

    validate_notifications(&config.notifications)?;
    validate_messaging(&config.messaging)?;
    validate_review_lifecycle_notifications(
        &config.review_lifecycle_notifications,
        &config.notifications,
        &config.messaging,
    )?;

    // Per-project fields: validate each ENABLED project; disabled ones are skipped
    // (their fields may be intentionally incomplete). The id/repo of every project
    // (enabled or not) is a routing/dedup key, so duplicates are rejected regardless.
    let mut seen_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut seen_repos: std::collections::HashSet<String> = std::collections::HashSet::new();
    for project in &config.projects {
        // id-format lock (Medium carrier): the id is stored as a `project_id` column
        // value that scopes every dedup/retention partition (`WHERE project_id = ?1` on
        // `dispatch_key` / `dispatch_event` / `tracked_pr`). It is still validated
        // (reject empty / whitespace / colon) so ids stay unambiguous, and so the
        // one-time legacy import — which parses the old JSON store keys (`tracked:{id}` /
        // `dispatched:{id}` / `events:{id}`) by splitting on `:` — can recover each
        // project's id unambiguously; an empty / whitespace / colon-bearing id would
        // alias two projects' partitions → dedup failure → re-review storm.
        // Checked for EVERY project (not just enabled): a disabled project's rows
        // persist and its id re-enters the key space the moment it is re-enabled. The
        // Hard path (future) is a `ProjectId` newtype whose typed constructor rejects
        // these at the type level, making the bad shape unexpressible.
        if project.id.is_empty()
            || project.id.contains(':')
            || project.id.chars().any(char::is_whitespace)
        {
            return Err(AppError::new(format!(
                "projectId 非法（不能为空、含 `:` 或空白字符）: {:?}",
                project.id
            )));
        }
        if !seen_ids.insert(project.id.as_str()) {
            return Err(AppError::new(format!(
                "项目 id 重复: {}（每个项目的 id 必须唯一）",
                project.id
            )));
        }
        // GitHub repo names are case-INSENSITIVE (`Owner/Repo` and `owner/repo` are the
        // same repository), and the webhook router matches them with
        // `eq_ignore_ascii_case` — so dedup must normalize too, or two case variants
        // would each register a project for the SAME repo (duplicate PR rows, double
        // dispatch). Normalize to lowercase before the uniqueness check.
        //
        // This bare-repo uniqueness is ALSO the "reject ambiguity" guarantee for Azure
        // (AB#822 F2): the Azure webhook routes a Service Hook by bare repo name (+ project
        // guard), so two Azure projects sharing a repo name across different org/project
        // would make routing ambiguous. Rejecting duplicate bare repo names here forecloses
        // that — the app intentionally does NOT support same-named repos across Azure
        // orgs/projects (no org dimension in the route).
        if !seen_repos.insert(project.repo.to_ascii_lowercase()) {
            return Err(AppError::new(format!(
                "项目 repo 重复: {}（同一仓库不能监控两次，大小写不敏感）",
                project.repo
            )));
        }
        if project.enabled {
            validate_project(project)?;
        }
    }

    let mut seen_rule_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (idx, rule) in config.rules.iter().enumerate() {
        validate_rule(rule, &config.notifications, &config.messaging)
            .map_err(|e| AppError::new(format!("rules[{idx}].{}", e.message)))?;
        if !seen_rule_ids.insert(rule.id.as_str()) {
            return Err(AppError::new(format!(
                "rules[{idx}].id 规则 id 重复: {}（每条规则的 id 必须唯一）",
                rule.id
            )));
        }
        if !rule.project_id.is_empty() && !seen_ids.contains(rule.project_id.as_str()) {
            return Err(AppError::new(format!(
                "rules[{idx}].projectId 不指向任何现有项目: {:?}",
                rule.project_id
            )));
        }
    }

    // When there ARE projects, `active_project_id` must point at one of them. A dangling
    // active id (e.g. the active project was deleted but the pointer wasn't updated) would
    // leave the UI `hydrate`d on a non-existent project and make `active_repo_root` silently
    // fall back to "" (codex handshake cwd lost). An empty `projects` keeps the first-launch
    // semantics (active id "" + no projects → onboarding), so only guard the non-empty case.
    if !config.projects.is_empty()
        && !config
            .projects
            .iter()
            .any(|p| p.id == config.active_project_id)
    {
        return Err(AppError::new(format!(
            "activeProjectId 不指向任何现有项目: {:?}",
            config.active_project_id
        )));
    }

    validate_remote_access(config)?;

    Ok(())
}

/// Serde wire-shape lock for `AppConfig`.
///
/// This is the **Medium carrier** for the `config/model.rs` ↔ `src/config/types.ts`
/// serde contract per `.claude/rules/prmonitor/ai-robust.md` (same spirit as the
/// `model.rs` lock). It is a contract LOCK (characterization) test: it passes on
/// current code and only fails if a field is renamed or the camelCase
/// serialization breaks. When a key here changes, the downstream `src/config/types.ts`
/// `AppConfig` mirror must be updated in lockstep — that downstream is the open
/// end of this funnel (no machine check on the TS side yet; future Hard path =
/// codegen `types.ts` from the Rust models + `git diff --exit-code`).
#[cfg(test)]
mod tests {
    use super::*;

    /// A [`Project`] whose filesystem-dependent fields point at this crate (so
    /// `validate_project` passes) — the per-project analogue of the old `valid_base`.
    fn valid_project() -> Project {
        Project {
            id: "default".to_string(),
            name: "default".to_string(),
            repo_root: env!("CARGO_MANIFEST_DIR").to_string(),
            skill_rel_path: "Cargo.toml".to_string(),
            ..Project::default()
        }
    }

    /// A valid [`AppConfig`] containing exactly one valid project — the new analogue
    /// of the old `valid_base`. Tests that exercise a bad per-project field mutate
    /// `..valid_project()` into the single slot.
    fn valid_base() -> AppConfig {
        AppConfig {
            projects: vec![valid_project()],
            active_project_id: "default".to_string(),
            ..AppConfig::default()
        }
    }

    /// Wraps one project as the sole element of an otherwise-default `AppConfig`,
    /// so `validate(&with_project(p))` routes through the per-project loop.
    fn with_project(project: Project) -> AppConfig {
        AppConfig {
            projects: vec![project],
            active_project_id: "default".to_string(),
            ..AppConfig::default()
        }
    }

    fn remote_entrypoint(id: &str, port: u16) -> RemoteEntrypoint {
        RemoteEntrypoint {
            id: id.to_string(),
            name: id.to_string(),
            bind_host: "127.0.0.1".to_string(),
            port,
            enabled: true,
            routes: vec![RemoteRoute::local_api()],
            ..RemoteEntrypoint::default()
        }
    }

    fn remote_tunnel(id: &str, target_entrypoint_id: &str) -> RemoteTunnel {
        RemoteTunnel {
            id: id.to_string(),
            name: id.to_string(),
            enabled: true,
            target_entrypoint_id: target_entrypoint_id.to_string(),
            ..RemoteTunnel::default()
        }
    }

    #[test]
    fn app_config_wire_shape_is_camel_case() {
        let config = AppConfig {
            projects: vec![Project::default()],
            active_project_id: "default".to_string(),
            webhook_enabled: false,
            webhook_port: 8787,
            webhook_secret: "shh".to_string(),
            cloudflared_bin: "cloudflared".to_string(),
            webhook_tunnel_mode: WebhookTunnelMode::default(),
            webhook_tunnel_command: String::new(),
            webhook_public_url: String::new(),
            local_api_token: "local-api-token-0123456789".to_string(),
            outbox: OutboxConfig::default(),
            notifications: NotificationSettings::default(),
            messaging: MessagingSettings::default(),
            review_lifecycle_notifications: ReviewLifecycleNotificationConfig::default(),
            remote_access: RemoteAccessConfig::default(),
            listeners: Vec::new(),
            tunnels: Vec::new(),
            rules: Vec::new(),
        };

        let v = serde_json::to_value(&config).expect("AppConfig serializes");

        // Multi-project keys present (camelCase).
        assert!(v.get("projects").is_some());
        assert!(v.get("activeProjectId").is_some());
        // AB#1182: the nested outbox policy serializes camelCase (a drift in the nested struct's
        // wire shape surfaces here too — the TS `AppConfig.outbox` mirror must stay in lockstep).
        assert!(v.get("outbox").is_some());
        assert!(v["outbox"].get("notificationTtlSecs").is_some());
        assert!(v["outbox"].get("notification_ttl_secs").is_none());
        assert_eq!(
            v["outbox"]["notificationTtlSecs"],
            DEFAULT_NOTIFICATION_TTL_SECS
        );
        assert!(v.get("notifications").is_some());
        assert_eq!(v["notifications"]["channels"][0]["kind"], "desktop");
        assert_eq!(v["notifications"]["channels"][0]["id"], "desktop");
        assert!(v["notifications"]["channels"][0]
            .get("webhookUrl")
            .is_some());
        assert!(v["notifications"]["channels"][0]
            .get("webhook_url")
            .is_none());
        // Global webhook keys stay at the top level.
        assert!(v.get("webhookEnabled").is_some());
        assert!(v.get("webhookPort").is_some());
        assert!(v.get("webhookSecret").is_some());
        assert!(v.get("cloudflaredBin").is_some());
        assert!(v.get("webhookTunnelMode").is_some());
        assert_eq!(v["webhookTunnelMode"], "quick");
        assert!(v.get("webhookTunnelCommand").is_some());
        assert!(v.get("webhookPublicUrl").is_some());
        // AB#1043: local REST API token present (camelCase) at the top level. The port now lives
        // in `remoteAccess.entrypoints[]`.
        assert!(v.get("localApiToken").is_some());
        // #1553: Remote Access is a nested entrypoints/routes/tunnels model.
        assert!(v.get("remoteAccess").is_some());
        assert!(v["remoteAccess"].get("entrypoints").is_some());
        assert!(v["remoteAccess"].get("tunnels").is_some());
        assert!(v.get("listeners").is_none());
        assert!(v.get("tunnels").is_none());
        assert!(v.get("rules").is_some());

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("active_project_id").is_none());
        assert!(v.get("webhook_enabled").is_none());
        assert!(v.get("webhook_port").is_none());
        assert!(v.get("webhook_secret").is_none());
        assert!(v.get("cloudflared_bin").is_none());
        assert!(v.get("webhook_tunnel_mode").is_none());
        assert!(v.get("webhook_tunnel_command").is_none());
        assert!(v.get("webhook_public_url").is_none());
        assert!(v.get("local_api_token").is_none());
        assert!(v.get("remote_access").is_none());

        // The per-project fields must NOT have leaked back to the top level (they
        // moved into `Project` — a regression that re-flattened them surfaces here).
        assert!(v.get("repo").is_none());
        assert!(v.get("repoRoot").is_none());
        assert!(v.get("autoReview").is_none());
    }

    #[test]
    fn rule_config_wire_shape_is_camel_case_and_round_trips() {
        let rule = RuleConfig {
            id: "r1".to_string(),
            name: "Ready".to_string(),
            enabled: true,
            source: None,
            event_type: Some(EventType::PullRequest),
            project_id: "p1".to_string(),
            repo: "owner/repo".to_string(),
            labels_any: vec!["needs-review".to_string()],
            labels_all: vec!["ready".to_string()],
            title_contains: "ship".to_string(),
            body_contains: "details".to_string(),
            actions: vec![
                RuleActionConfig::new("review", RuleActionKind::Review),
                RuleActionConfig::new("check", RuleActionKind::Check),
                RuleActionConfig::new("notify", RuleActionKind::Notify),
            ],
            allow_action_kinds: Vec::new(),
            deny_action_kinds: Vec::new(),
        };

        let v = serde_json::to_value(&rule).expect("RuleConfig serializes");
        assert_eq!(
            v,
            serde_json::json!({
                "id": "r1",
                "name": "Ready",
                "enabled": true,
                "source": null,
                "eventType": "pullRequest",
                "projectId": "p1",
                "repo": "owner/repo",
                "labelsAny": ["needs-review"],
                "labelsAll": ["ready"],
                "titleContains": "ship",
                "bodyContains": "details",
                "actions": [
                    {
                        "id": "review",
                        "kind": "review",
                        "enabled": true,
                        "target": { "kind": "none" },
                        "dedupePolicy": "event",
                        "delaySecs": 0,
                        "dependsOn": [],
                        "level": "action"
                    },
                    {
                        "id": "check",
                        "kind": "check",
                        "enabled": true,
                        "target": { "kind": "none" },
                        "dedupePolicy": "event",
                        "delaySecs": 0,
                        "dependsOn": [],
                        "level": "action"
                    },
                    {
                        "id": "notify",
                        "kind": "notify",
                        "enabled": true,
                        "target": { "kind": "none" },
                        "dedupePolicy": "event",
                        "delaySecs": 0,
                        "dependsOn": [],
                        "level": "action"
                    }
                ],
                "allowActionKinds": [],
                "denyActionKinds": []
            })
        );

        let parsed: RuleConfig = serde_json::from_value(v).expect("RuleConfig deserializes");
        assert_eq!(parsed.id, rule.id);
        assert_eq!(parsed.source, None);
        assert_eq!(parsed.event_type, Some(EventType::PullRequest));
        assert_eq!(parsed.actions, rule.actions);
    }

    // AB#1182 forward-compat lock: a config persisted BEFORE the `outbox` field existed (key
    // absent), and a PARTIAL `{"outbox":{}}` (key present, inner field absent), both deserialize
    // by filling the missing pieces from `Default` rather than failing — the `#[serde(default)]`
    // on both `AppConfig` and `OutboxConfig`. A regression that dropped either default would make
    // old stored configs un-loadable, so lock it.
    #[test]
    fn outbox_config_deserializes_forward_compatibly() {
        // Older config: no `outbox` key at all → the whole struct defaults.
        let without: AppConfig =
            serde_json::from_str(r#"{"activeProjectId":"default"}"#).expect("missing outbox loads");
        assert_eq!(
            without.outbox.notification_ttl_secs,
            DEFAULT_NOTIFICATION_TTL_SECS
        );

        // Partial: `outbox` present but its field absent → the field defaults.
        let partial: AppConfig =
            serde_json::from_str(r#"{"outbox":{}}"#).expect("partial outbox loads");
        assert_eq!(
            partial.outbox.notification_ttl_secs,
            DEFAULT_NOTIFICATION_TTL_SECS
        );

        // An explicit value round-trips (and `0` — the disable sentinel — is preserved verbatim).
        let explicit: AppConfig = serde_json::from_str(r#"{"outbox":{"notificationTtlSecs":0}}"#)
            .expect("explicit loads");
        assert_eq!(explicit.outbox.notification_ttl_secs, 0);
    }

    #[test]
    fn validate_notification_channels_enforces_ids_timeout_and_kind_fields() {
        let mut cfg = valid_base();
        let mut slack = valid_notification_channel(NotificationKind::Slack);
        slack.webhook_url = String::new();
        cfg.notifications.channels = vec![slack.clone()];
        assert_error_prefix(validate(&cfg), "notificationWebhookUrl 不能为空");

        slack.enabled = false;
        cfg.notifications.channels = vec![slack.clone()];
        validate(&cfg).expect("disabled channels skip kind-specific required fields");

        slack.enabled = true;
        slack.webhook_url = "http://example.com/hook".to_string();
        cfg.notifications.channels = vec![slack.clone()];
        assert_error_prefix(validate(&cfg), "notificationWebhookUrl 必须是 https:// URL");

        slack.webhook_url = "https://user@example.com/hook".to_string();
        cfg.notifications.channels = vec![slack.clone()];
        assert_error_prefix(validate(&cfg), "notificationWebhookUrl 必须是 https:// URL");

        slack.webhook_url = "https://example.com/hook".to_string();
        slack.timeout_secs = 0;
        cfg.notifications.channels = vec![slack.clone()];
        assert_error_prefix(validate(&cfg), "notificationTimeoutSecs 必须大于 0");

        slack.timeout_secs = DEFAULT_NOTIFICATION_TIMEOUT_SECS;
        cfg.notifications.channels = vec![slack.clone(), slack.clone()];
        assert_error_prefix(validate(&cfg), "notificationChannelId 重复");

        let mut telegram = valid_notification_channel(NotificationKind::Telegram);
        telegram.telegram_bot_token = String::new();
        cfg.notifications.channels = vec![telegram];
        assert_error_prefix(validate(&cfg), "telegramBotToken 不能为空");

        let mut email = valid_notification_channel(NotificationKind::Email);
        email.smtp_to = String::new();
        cfg.notifications.channels = vec![email];
        assert_error_prefix(validate(&cfg), "smtpTo 不能为空");

        let mut email = valid_notification_channel(NotificationKind::Email);
        email.smtp_from = "not-an-email".to_string();
        cfg.notifications.channels = vec![email];
        assert_error_prefix(validate(&cfg), "smtpFrom 必须是有效邮件地址");

        let mut email = valid_notification_channel(NotificationKind::Email);
        email.smtp_to = "ok@example.com,not-an-email".to_string();
        cfg.notifications.channels = vec![email];
        assert_error_prefix(validate(&cfg), "smtpTo 必须是逗号分隔的有效邮件地址");
    }

    #[test]
    fn validate_messaging_enforces_ids_timeout_allowlist_and_feishu_fields() {
        let mut cfg = valid_base();
        let mut integration = valid_messaging_integration();

        integration.enabled = false;
        integration.verification_token = String::new();
        integration.encrypt_key = String::new();
        integration.app_id = String::new();
        integration.app_secret = String::new();
        integration.bot_open_id = String::new();
        integration.allowed_conversation_ids = Vec::new();
        cfg.messaging.integrations = vec![integration.clone()];
        validate(&cfg).expect("disabled messaging integrations skip provider fields and allowlist");

        integration.enabled = true;
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "messagingAllowedConversationIds 不能为空");

        integration.allowed_conversation_ids = vec!["oc_123".to_string()];
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "feishuVerificationToken 不能为空");

        integration.verification_token = "short".to_string();
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "feishuVerificationToken 太短");

        integration.verification_token = "verify-token-1234".to_string();
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "feishuEncryptKey 不能为空");

        integration.encrypt_key = "short".to_string();
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "feishuEncryptKey 太短");

        integration.encrypt_key = "encrypt-key-1234".to_string();
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "feishuAppId 不能为空");

        integration.app_id = "cli_xxx".to_string();
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "feishuAppSecret 不能为空");

        integration.app_secret = "app-secret".to_string();
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "feishuBotOpenId 不能为空");

        integration.bot_open_id = "ou_bot".to_string();
        integration.timeout_secs = 0;
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "messagingTimeoutSecs 必须大于 0");

        integration.timeout_secs = DEFAULT_NOTIFICATION_TIMEOUT_SECS;
        cfg.messaging.integrations = vec![integration.clone(), integration.clone()];
        assert_error_prefix(validate(&cfg), "messagingIntegrationId 重复");

        integration.id = "bad id".to_string();
        cfg.messaging.integrations = vec![integration];
        assert_error_prefix(validate(&cfg), "messagingIntegrationId 非法");
    }

    #[test]
    fn validate_messaging_enforces_wechat_work_and_dingtalk_fields() {
        let mut cfg = valid_base();
        let mut integration = valid_messaging_integration();

        integration.kind = MessagingProviderKind::WeChatWork;
        integration.id = "wecom-main".to_string();
        integration.name = "WeCom".to_string();
        integration.verification_token = String::new();
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "weChatWorkToken 不能为空");

        integration.verification_token = "verify-token-1234".to_string();
        integration.encrypt_key = String::new();
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "weChatWorkEncodingAesKey 不能为空");

        integration.encrypt_key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string();
        integration.app_id = String::new();
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "weChatWorkCorpId 不能为空");

        integration.app_id = "corp-id".to_string();
        integration.app_secret = String::new();
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "weChatWorkCorpSecret 不能为空");

        integration.app_secret = "corp-secret".to_string();
        integration.bot_open_id = String::new();
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "weChatWorkAgentId 不能为空");

        integration.bot_open_id = "1000002".to_string();
        integration.encrypt_key = "AAAA".to_string();
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(
            validate(&cfg),
            "weChatWorkEncodingAesKey 必须解码为 32 字节",
        );

        integration.kind = MessagingProviderKind::DingTalk;
        integration.id = "dingtalk-main".to_string();
        integration.name = "DingTalk".to_string();
        integration.verification_token = String::new();
        integration.app_secret = "dingtalk-secret".to_string();
        integration.bot_open_id = "robot-code".to_string();
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "dingTalkToken 不能为空");

        integration.verification_token = "token-1234567890".to_string();
        integration.app_secret = String::new();
        cfg.messaging.integrations = vec![integration.clone()];
        assert_error_prefix(validate(&cfg), "dingTalkAppSecret 不能为空");

        integration.app_secret = "dingtalk-secret".to_string();
        integration.bot_open_id = String::new();
        cfg.messaging.integrations = vec![integration];
        assert_error_prefix(validate(&cfg), "dingTalkRobotCode 不能为空");
    }

    fn valid_notification_channel(kind: NotificationKind) -> NotificationChannel {
        NotificationChannel {
            id: format!("{kind:?}").to_lowercase(),
            name: format!("{kind:?}"),
            kind,
            enabled: true,
            webhook_url: "https://example.com/hook".to_string(),
            telegram_bot_token: "telegram-token".to_string(),
            telegram_chat_id: "chat".to_string(),
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            smtp_from: "from@example.com".to_string(),
            smtp_to: "to@example.com".to_string(),
            ..NotificationChannel::default()
        }
    }

    fn valid_messaging_integration() -> MessagingIntegration {
        MessagingIntegration {
            id: "feishu-main".to_string(),
            name: "Feishu".to_string(),
            enabled: true,
            verification_token: "verify-token-1234".to_string(),
            encrypt_key: "encrypt-key-1234".to_string(),
            app_id: "cli_xxx".to_string(),
            app_secret: "app-secret".to_string(),
            bot_open_id: "ou_bot".to_string(),
            allowed_conversation_ids: vec!["oc_123".to_string()],
            ..MessagingIntegration::feishu_default()
        }
    }

    fn assert_error_prefix(result: AppResult<()>, prefix: &str) {
        let err = result.expect_err("validation should fail");
        assert!(
            err.message.starts_with(prefix),
            "expected error prefix {prefix:?}, got {:?}",
            err.message
        );
    }

    #[test]
    fn project_wire_shape_is_camel_case() {
        let project = Project {
            id: "p1".to_string(),
            name: "Project One".to_string(),
            enabled: true,
            repo: "owner/name".to_string(),
            repo_root: "/path/to/repo".to_string(),
            poll_interval_secs: 120,
            authors: vec!["octocat".to_string()],
            skill_rel_path: ".codex/skills/pr-review/SKILL.md".to_string(),
            pr_cooldown_seconds: 1800,
            source_kind: SourceKind::default(),
            engine_kind: EngineKind::default(),
            codex_model: "gpt-5.1-codex".to_string(),
            claude_model: "claude-opus-4-1".to_string(),
            update_mode: UpdateMode::WebhookOnly,
            azure_org: "myorg".to_string(),
            azure_project: "myproject".to_string(),
            label_source: LabelSource::default(),
            bitbucket_host: "https://bitbucket.example.com".to_string(),
            bitbucket_project: "GOCELL".to_string(),
            bitbucket_token: "secret-pat".to_string(),
        };

        let v = serde_json::to_value(&project).expect("Project serializes");

        // camelCase keys present.
        assert!(v.get("id").is_some());
        assert!(v.get("name").is_some());
        assert!(v.get("enabled").is_some());
        assert!(v.get("repo").is_some());
        assert!(v.get("repoRoot").is_some());
        assert!(v.get("pollIntervalSecs").is_some());
        assert!(v.get("authors").is_some());
        assert!(v.get("reviewLabel").is_none());
        assert!(v.get("checkLabel").is_none());
        assert!(v.get("skillRelPath").is_some());
        assert!(v.get("prCooldownSeconds").is_some());
        assert!(v.get("sourceKind").is_some());
        assert_eq!(v["sourceKind"], "github");
        assert!(v.get("engineKind").is_some());
        assert_eq!(v["engineKind"], "codex");
        // 手填模型字段 wire camelCase + 值（Medium 载体；与 engineKind 的值断言风格一致）。
        assert_eq!(v["codexModel"], "gpt-5.1-codex");
        assert_eq!(v["claudeModel"], "claude-opus-4-1");
        assert!(v.get("autoReview").is_none());
        // #818: the new data-source-mode fields.
        assert!(v.get("updateMode").is_some());
        assert_eq!(v["updateMode"], "webhook-only");
        assert!(v.get("azureOrg").is_some());
        assert!(v.get("azureProject").is_some());
        // AB#717: the label-source toggle + Bitbucket source fields.
        assert!(v.get("labelSource").is_some());
        assert_eq!(v["labelSource"], "native");
        assert!(v.get("bitbucketHost").is_some());
        assert!(v.get("bitbucketProject").is_some());
        assert!(v.get("bitbucketToken").is_some());

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("repo_root").is_none());
        assert!(v.get("poll_interval_secs").is_none());
        assert!(v.get("review_label").is_none());
        assert!(v.get("check_label").is_none());
        assert!(v.get("skill_rel_path").is_none());
        assert!(v.get("pr_cooldown_seconds").is_none());
        assert!(v.get("source_kind").is_none());
        assert!(v.get("engine_kind").is_none());
        assert!(v.get("codex_model").is_none());
        assert!(v.get("claude_model").is_none());
        assert!(v.get("auto_review").is_none());
        // #818: snake_case forms of the new fields absent.
        assert!(v.get("update_mode").is_none());
        assert!(v.get("azure_org").is_none());
        assert!(v.get("azure_project").is_none());
        // AB#717: snake_case forms of the new fields absent.
        assert!(v.get("label_source").is_none());
        assert!(v.get("bitbucket_host").is_none());
        assert!(v.get("bitbucket_project").is_none());
        assert!(v.get("bitbucket_token").is_none());
    }

    /// First-launch marker lock (Medium). The frontend routes a fresh install into
    /// onboarding by detecting that no project exists yet (`config.projects` empty)
    /// — that empty default is the contract. If a future change gave `AppConfig` a
    /// pre-populated project, the frontend would silently skip onboarding and the
    /// poll-loop gate would change behavior; this assertion fails first so the
    /// coupling is machine-checked rather than comment-only.
    #[test]
    fn default_has_no_projects() {
        assert!(AppConfig::default().projects.is_empty());
        assert_eq!(AppConfig::default().active_project_id, "");
    }

    /// A fully-populated [`Listener`] for the wire-shape lock.
    fn sample_listener() -> Listener {
        Listener {
            id: "l1".to_string(),
            name: "Local API".to_string(),
            kind: ListenerKind::LocalApi,
            bind_host: "127.0.0.1".to_string(),
            port: 8788,
            enabled: true,
            auth: ListenerAuthMode::Bearer,
            auth_token: "listener-token-0123456789".to_string(),
            terminal_read: true,
            terminal_write: true,
            terminal_create: true,
            terminal_admin: false,
            allowed_origins: vec!["https://app.example.com".to_string()],
            public_url: "https://api.example.com".to_string(),
        }
    }

    /// Serde wire-shape lock for [`Listener`] (AB#1064, **Medium carrier**): locks the
    /// camelCase wire shape (same spirit as `app_config_wire_shape_is_camel_case`).
    #[test]
    fn listener_wire_shape_is_camel_case() {
        let v = serde_json::to_value(sample_listener()).expect("Listener serializes");

        // camelCase keys present.
        assert!(v.get("id").is_some());
        assert!(v.get("name").is_some());
        assert!(v.get("kind").is_some());
        assert!(v.get("bindHost").is_some());
        assert!(v.get("port").is_some());
        assert!(v.get("enabled").is_some());
        assert!(v.get("auth").is_some());
        assert!(v.get("authToken").is_some());
        assert!(v.get("terminalRead").is_some());
        assert!(v.get("terminalWrite").is_some());
        assert!(v.get("terminalCreate").is_some());
        assert!(v.get("terminalAdmin").is_some());
        assert!(v.get("allowedOrigins").is_some());
        assert!(v.get("publicUrl").is_some());

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("bind_host").is_none());
        assert!(v.get("auth_token").is_none());
        assert!(v.get("terminal_read").is_none());
        assert!(v.get("allowed_origins").is_none());
        assert!(v.get("public_url").is_none());
    }

    /// Serde wire-shape lock for [`Tunnel`] (AB#1064, **Medium carrier**): locks the
    /// camelCase wire shape (same spirit as `app_config_wire_shape_is_camel_case`).
    #[test]
    fn tunnel_wire_shape_is_camel_case() {
        let tunnel = Tunnel {
            id: "t1".to_string(),
            name: "Quick".to_string(),
            mode: WebhookTunnelMode::default(),
            target_listener_id: "l1".to_string(),
            command: "cloudflared tunnel --url http://127.0.0.1:{port}".to_string(),
            public_url: "https://t.example.com".to_string(),
            enabled: true,
        };

        let v = serde_json::to_value(&tunnel).expect("Tunnel serializes");

        // camelCase keys present.
        assert!(v.get("id").is_some());
        assert!(v.get("name").is_some());
        assert!(v.get("mode").is_some());
        assert!(v.get("targetListenerId").is_some());
        assert!(v.get("command").is_some());
        assert!(v.get("publicUrl").is_some());
        assert!(v.get("enabled").is_some());

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("target_listener_id").is_none());
        assert!(v.get("public_url").is_none());
    }

    /// [`ListenerKind`] kebab-case wire-value lock (AB#1064, **Medium carrier**): the
    /// literal wire strings mirror the work item's naming and are the cross-end contract.
    #[test]
    fn listener_kind_wire_values_are_kebab() {
        use serde_json::json;
        assert_eq!(
            serde_json::to_value(ListenerKind::LocalApi).unwrap(),
            json!("local-api")
        );
        assert_eq!(
            serde_json::to_value(ListenerKind::RemoteWeb).unwrap(),
            json!("remote-web")
        );
        assert_eq!(
            serde_json::to_value(ListenerKind::EventIngress).unwrap(),
            json!("event-ingress")
        );
        assert_eq!(
            serde_json::to_value(ListenerKind::Terminal).unwrap(),
            json!("terminal")
        );
    }

    /// Default local-api entrypoint lock (#1553, Medium): a fresh install ships exactly ONE
    /// entrypoint — the local trigger API, enabled at `127.0.0.1:8788` with `/api` route.
    /// No tunnels are seeded, so nothing public is exposed until the user adds one.
    #[test]
    fn default_seeds_local_api_entrypoint() {
        let remote = AppConfig::default().remote_access;
        assert_eq!(remote.entrypoints.len(), 1);
        let entrypoint = &remote.entrypoints[0];
        assert_eq!(entrypoint.id, "local-api");
        assert!(entrypoint.enabled);
        assert_eq!(entrypoint.port, 8788);
        assert_eq!(entrypoint.bind_host, "127.0.0.1");
        assert_eq!(entrypoint.routes.len(), 1);
        assert_eq!(entrypoint.routes[0].capability, RemoteCapability::LocalApi);
        assert_eq!(entrypoint.routes[0].path, "/api");
        assert!(remote.tunnels.is_empty());
    }

    /// #1553: entrypoint allowedOrigins are origins, not public URLs; HTTPS origins validate.
    #[test]
    fn validate_accepts_entrypoint_https_allowed_origin() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![RemoteEntrypoint {
                    allowed_origins: vec!["https://example.com".to_string()],
                    ..remote_entrypoint("entry", 9100)
                }],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        };
        assert!(validate(&config).is_ok());
    }

    /// AB#1073 port-conflict (Medium runtime guard): two enabled listeners on the same
    /// non-zero port are rejected; the message keeps the `port` prefix.
    #[test]
    fn validate_rejects_port_conflict() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![remote_entrypoint("a", 9000), remote_entrypoint("b", 9000)],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("port"), "{err}");
    }

    /// Default-rule lock (Medium). A fresh config must not auto-dispatch unless
    /// rules have been explicitly configured or migrated from legacy settings.
    #[test]
    fn default_rules_are_empty() {
        assert!(AppConfig::default().rules.is_empty());
    }

    /// Default-update-mode lock (#818, Medium). A fresh project must default to
    /// `WebhookOnly` — the safe boot behavior that runs NO automatic CLI polling at
    /// startup (the core safety change of #818). A silent flip to a polling mode would
    /// re-introduce unsolicited CLI polls at launch; locking the default here makes that
    /// intent machine-checked.
    #[test]
    fn default_update_mode_is_webhook_only() {
        assert_eq!(Project::default().update_mode, UpdateMode::WebhookOnly);
    }

    #[test]
    fn validate_accepts_empty_projects_first_launch() {
        // No project yet (onboarding not run) — valid; onboarding is the gate.
        assert!(validate(&AppConfig::default()).is_ok());
    }

    #[test]
    fn validate_accepts_existing_repo_root_and_skill() {
        assert!(validate(&valid_base()).is_ok());
        // The per-project validator agrees directly.
        assert!(validate_project(&valid_project()).is_ok());
    }

    #[test]
    fn validate_skips_disabled_projects() {
        // A disabled project with a bogus repo_root must NOT fail validation — its
        // fields are not checked (only enabled projects are).
        let disabled = Project {
            enabled: false,
            repo_root: "/no/such/dir/xyz".to_string(),
            skill_rel_path: "definitely_missing.md".to_string(),
            ..valid_project()
        };
        assert!(validate(&with_project(disabled)).is_ok());
    }

    #[test]
    fn validate_rejects_duplicate_project_ids() {
        let config = AppConfig {
            projects: vec![
                Project {
                    repo: "owner/a".to_string(),
                    ..valid_project()
                },
                Project {
                    repo: "owner/b".to_string(),
                    ..valid_project()
                },
            ],
            active_project_id: "default".to_string(),
            ..AppConfig::default()
        };
        assert!(validate(&config).is_err());
    }

    #[test]
    fn validate_rejects_duplicate_project_repos() {
        let config = AppConfig {
            projects: vec![
                Project {
                    id: "a".to_string(),
                    ..valid_project()
                },
                Project {
                    id: "b".to_string(),
                    ..valid_project()
                },
            ],
            active_project_id: "a".to_string(),
            ..AppConfig::default()
        };
        // Both share the default `repo` (ghbvf/gocell) → reject.
        assert!(validate(&config).is_err());
    }

    #[test]
    fn validate_rejects_duplicate_project_repos_case_insensitive() {
        // GitHub repo names are case-insensitive and the webhook router matches with
        // `eq_ignore_ascii_case`, so `Owner/Repo` and `owner/repo` are the SAME repo →
        // monitoring both must be rejected (else duplicate rows + double dispatch).
        let config = AppConfig {
            projects: vec![
                Project {
                    id: "a".to_string(),
                    repo: "Owner/Repo".to_string(),
                    ..valid_project()
                },
                Project {
                    id: "b".to_string(),
                    repo: "owner/repo".to_string(),
                    ..valid_project()
                },
            ],
            active_project_id: "a".to_string(),
            ..AppConfig::default()
        };
        assert!(validate(&config).is_err());
    }

    #[test]
    fn validate_rejects_dangling_active_project_id() {
        // Non-empty projects but `active_project_id` matches none → reject (a dangling
        // pointer would strand the UI / lose the codex handshake cwd).
        let config = AppConfig {
            active_project_id: "ghost".to_string(),
            ..valid_base()
        };
        assert!(validate(&config).is_err());

        // The pointer matching an existing project is accepted.
        assert!(validate(&valid_base()).is_ok());

        // Empty projects keeps first-launch semantics: active id "" + no projects is OK.
        assert!(validate(&AppConfig::default()).is_ok());
    }

    #[test]
    fn validate_rejects_project_id_with_colon_or_whitespace() {
        // The id is stored as a `project_id` column value scoping every partition
        // (`WHERE project_id = ?1` on `dispatch_key` / `dispatch_event` / `tracked_pr`),
        // and the one-time legacy import recovers it by splitting the old JSON store keys
        // (`dispatched:{id}` / `events:{id}` / `tracked:{id}`) on `:`; a `:`, whitespace,
        // or empty id aliases two projects' partitions → dedup failure → re-review storm.
        // Rejected for EVERY project (even disabled), since a disabled project's id
        // re-enters the key space when re-enabled.
        for bad in ["", "a:b", "has space", "tab\tid", "\n"] {
            assert!(
                validate(&with_project(Project {
                    id: bad.to_string(),
                    ..valid_project()
                }))
                .is_err(),
                "expected id {bad:?} to be rejected"
            );
        }
        // A disabled project with a bad id is STILL rejected (its partition rows persist).
        assert!(validate(&with_project(Project {
            id: "a:b".to_string(),
            enabled: false,
            ..valid_project()
        }))
        .is_err());
        // A clean id (no `:`, no whitespace, non-empty) is accepted.
        assert!(validate(&with_project(Project {
            id: "default".to_string(),
            ..valid_project()
        }))
        .is_ok());
    }

    #[test]
    fn validate_rejects_empty_repo_root() {
        assert!(validate(&with_project(Project {
            repo_root: String::new(),
            ..valid_project()
        }))
        .is_err());
    }

    #[test]
    fn validate_rejects_missing_repo_root() {
        assert!(validate(&with_project(Project {
            repo_root: "/no/such/dir/xyz".to_string(),
            ..valid_project()
        }))
        .is_err());
    }

    #[test]
    fn validate_rejects_missing_skill() {
        assert!(validate(&with_project(Project {
            skill_rel_path: "definitely_missing.md".to_string(),
            ..valid_project()
        }))
        .is_err());
    }

    #[test]
    fn validate_rejects_relative_repo_root() {
        // `repo_root` must be absolute (doc contract) regardless of CWD.
        assert!(validate(&with_project(Project {
            repo_root: "src".to_string(),
            ..valid_project()
        }))
        .is_err());
    }

    #[test]
    fn validate_rejects_absolute_skill_rel_path() {
        // An absolute skill path would let `Path::join` discard `repo_root`.
        assert!(validate(&with_project(Project {
            skill_rel_path: "/etc/hosts".to_string(),
            ..valid_project()
        }))
        .is_err());
    }

    #[test]
    fn validate_rejects_skill_escaping_repo_root() {
        // `repo_root`/src + `../Cargo.toml` resolves to repo_root/Cargo.toml,
        // which is outside repo_root/src — must be rejected.
        assert!(validate(&with_project(Project {
            repo_root: format!("{}/src", env!("CARGO_MANIFEST_DIR")),
            skill_rel_path: "../Cargo.toml".to_string(),
            ..valid_project()
        }))
        .is_err());
    }

    #[test]
    fn validate_skips_skill_path_for_claude_engine() {
        // #718: the claude engine discovers `.claude/skills/` from the turn cwd, so
        // `skillRelPath` is unused for it — `validate_project` must NOT reject a claude
        // project for a missing/bad skill path. The SAME bad path under codex still
        // rejects, proving the gate is engine-conditional, not a blanket skip.
        assert!(validate(&with_project(Project {
            skill_rel_path: "definitely_missing.md".to_string(),
            engine_kind: EngineKind::Claude,
            ..valid_project()
        }))
        .is_ok());
        assert!(validate(&with_project(Project {
            skill_rel_path: "definitely_missing.md".to_string(),
            engine_kind: EngineKind::Codex,
            ..valid_project()
        }))
        .is_err());
    }

    #[test]
    fn validate_rejects_zero_intervals() {
        assert!(validate(&with_project(Project {
            poll_interval_secs: 0,
            ..valid_project()
        }))
        .is_err());
        assert!(validate(&with_project(Project {
            pr_cooldown_seconds: 0,
            ..valid_project()
        }))
        .is_err());
    }

    #[test]
    fn validate_rejects_bad_repo() {
        // owner/name boundary the `gh pr list --repo` call consumes (PR #41 F2).
        for bad in [
            "",
            "owner",
            "owner/",
            "/name",
            "a/b/c",
            "own er/name",
            "owner/na me",
            " ghbvf/gocell",
        ] {
            assert!(
                validate(&with_project(Project {
                    repo: bad.to_string(),
                    ..valid_project()
                }))
                .is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        assert!(validate(&with_project(Project {
            repo: "ghbvf/gocell".to_string(),
            ..valid_project()
        }))
        .is_ok());
    }

    #[test]
    fn validate_rejects_bad_rules() {
        let mut config = valid_base();
        config.rules.push(RuleConfig {
            id: " ".to_string(),
            name: "Rule".to_string(),
            enabled: false,
            ..RuleConfig::default()
        });
        assert!(validate(&config)
            .unwrap_err()
            .message
            .starts_with("rules[0].id"));

        let mut config = valid_base();
        config.rules.push(RuleConfig {
            id: "r1".to_string(),
            name: String::new(),
            enabled: false,
            ..RuleConfig::default()
        });
        assert!(validate(&config)
            .unwrap_err()
            .message
            .starts_with("rules[0].name"));

        let mut config = valid_base();
        config.rules.push(RuleConfig {
            id: "r1".to_string(),
            name: "No action".to_string(),
            enabled: true,
            actions: Vec::new(),
            ..RuleConfig::default()
        });
        assert!(validate(&config)
            .unwrap_err()
            .message
            .starts_with("rules[0].actions"));

        let mut config = valid_base();
        config.rules.push(RuleConfig {
            id: "r1".to_string(),
            name: "Duplicate action".to_string(),
            enabled: true,
            actions: vec![
                RuleActionConfig::new("dup", RuleActionKind::Review),
                RuleActionConfig::new("dup", RuleActionKind::Check),
            ],
            ..RuleConfig::default()
        });
        assert!(validate(&config)
            .unwrap_err()
            .message
            .starts_with("rules[0].actions"));

        let mut config = valid_base();
        config.rules.push(RuleConfig {
            id: "r1".to_string(),
            name: "Dangling project".to_string(),
            enabled: true,
            project_id: "missing".to_string(),
            actions: vec![RuleActionConfig::new("notify", RuleActionKind::Notify)],
            ..RuleConfig::default()
        });
        assert!(validate(&config)
            .unwrap_err()
            .message
            .starts_with("rules[0].projectId"));
    }

    #[test]
    fn validate_rule_action_dag_and_combination_policy_fail_fast() {
        let mut config = valid_base();
        let mut action = RuleActionConfig::new("review", RuleActionKind::Review);
        action.depends_on = vec!["missing".to_string()];
        config.rules.push(RuleConfig {
            id: "r1".to_string(),
            name: "Unknown dep".to_string(),
            enabled: true,
            actions: vec![action],
            ..RuleConfig::default()
        });
        assert!(validate(&config)
            .unwrap_err()
            .message
            .contains("依赖未知动作"));

        let mut config = valid_base();
        let mut action = RuleActionConfig::new("review", RuleActionKind::Review);
        action.depends_on = vec!["review".to_string()];
        config.rules.push(RuleConfig {
            id: "r1".to_string(),
            name: "Self dep".to_string(),
            enabled: true,
            actions: vec![action],
            ..RuleConfig::default()
        });
        assert!(validate(&config).unwrap_err().message.contains("自依赖"));

        let mut config = valid_base();
        let mut a = RuleActionConfig::new("a", RuleActionKind::Notify);
        a.depends_on = vec!["c".to_string()];
        let mut b = RuleActionConfig::new("b", RuleActionKind::Notify);
        b.depends_on = vec!["a".to_string()];
        let mut c = RuleActionConfig::new("c", RuleActionKind::Notify);
        c.depends_on = vec!["b".to_string()];
        config.rules.push(RuleConfig {
            id: "r1".to_string(),
            name: "Cycle".to_string(),
            enabled: true,
            actions: vec![a, b, c],
            ..RuleConfig::default()
        });
        assert!(validate(&config).unwrap_err().message.contains("存在环路"));

        let mut config = valid_base();
        config.rules.push(RuleConfig {
            id: "r1".to_string(),
            name: "Allow only review".to_string(),
            enabled: true,
            actions: vec![RuleActionConfig::new("notify", RuleActionKind::Notify)],
            allow_action_kinds: vec![RuleActionKind::Review],
            ..RuleConfig::default()
        });
        assert!(validate(&config)
            .unwrap_err()
            .message
            .contains("被组合策略拒绝"));

        let mut config = valid_base();
        config.rules.push(RuleConfig {
            id: "r1".to_string(),
            name: "Deny wins".to_string(),
            enabled: true,
            actions: vec![RuleActionConfig::new("review", RuleActionKind::Review)],
            allow_action_kinds: vec![RuleActionKind::Review],
            deny_action_kinds: vec![RuleActionKind::Review],
            ..RuleConfig::default()
        });
        assert!(validate(&config)
            .unwrap_err()
            .message
            .contains("被组合策略拒绝"));
    }

    #[test]
    fn validate_rule_action_targets_reject_missing_or_disabled_references() {
        let mut config = valid_base();
        config.notifications.channels = vec![valid_notification_channel(NotificationKind::Slack)];
        let mut notify = RuleActionConfig::new("notify", RuleActionKind::Notify);
        notify.target = RuleActionTarget::NotificationChannels {
            channel_ids: vec!["missing".to_string()],
        };
        config.rules.push(RuleConfig {
            id: "r1".to_string(),
            name: "Notify missing channel".to_string(),
            enabled: true,
            actions: vec![notify],
            ..RuleConfig::default()
        });
        assert!(validate(&config)
            .unwrap_err()
            .message
            .contains("target.channelIds 不存在"));

        let mut config = valid_base();
        let mut disabled = valid_notification_channel(NotificationKind::Slack);
        disabled.id = "slack-main".to_string();
        disabled.enabled = false;
        config.notifications.channels = vec![disabled];
        let mut notify = RuleActionConfig::new("notify", RuleActionKind::Notify);
        notify.target = RuleActionTarget::NotificationChannels {
            channel_ids: vec!["slack-main".to_string()],
        };
        config.rules.push(RuleConfig {
            id: "r1".to_string(),
            name: "Notify disabled channel".to_string(),
            enabled: true,
            actions: vec![notify],
            ..RuleConfig::default()
        });
        assert!(validate(&config)
            .unwrap_err()
            .message
            .contains("target.channelIds 已禁用"));

        let mut config = valid_base();
        config.messaging.integrations = vec![valid_messaging_integration()];
        let mut notify = RuleActionConfig::new("notify", RuleActionKind::Notify);
        notify.target = RuleActionTarget::MessagingConversation {
            integration_id: "feishu-main".to_string(),
            conversation_id: "oc_missing".to_string(),
        };
        config.rules.push(RuleConfig {
            id: "r1".to_string(),
            name: "Notify unauthorized conversation".to_string(),
            enabled: true,
            actions: vec![notify],
            ..RuleConfig::default()
        });
        assert!(validate(&config)
            .unwrap_err()
            .message
            .contains("target.conversationId 未授权"));
    }

    #[test]
    fn validate_rejects_configured_delay_overflow_guardrail() {
        let mut config = valid_base();
        let mut action = RuleActionConfig::new("notify", RuleActionKind::Notify);
        action.delay_secs = MAX_CONFIGURED_DELAY_SECS + 1;
        config.rules.push(RuleConfig {
            id: "r1".to_string(),
            name: "Too much delay".to_string(),
            enabled: true,
            actions: vec![action],
            ..RuleConfig::default()
        });
        assert!(validate(&config)
            .unwrap_err()
            .message
            .contains("delaySecs 不能超过"));

        let mut config = valid_base();
        config.review_lifecycle_notifications = ReviewLifecycleNotificationConfig {
            enabled: true,
            events: vec![ReviewLifecycleEvent::Started],
            targets: vec![ReviewLifecycleTarget::NotificationChannels {
                channel_ids: Vec::new(),
            }],
            start_delay_secs: MAX_CONFIGURED_DELAY_SECS + 1,
            end_delay_secs: 0,
        };
        assert!(validate(&config)
            .unwrap_err()
            .message
            .contains("reviewLifecycleNotifications.startDelaySecs 不能超过"));
    }

    #[test]
    fn validate_review_lifecycle_targets_only_when_enabled() {
        let mut config = valid_base();
        config.review_lifecycle_notifications = ReviewLifecycleNotificationConfig {
            enabled: false,
            events: Vec::new(),
            targets: vec![ReviewLifecycleTarget::MessagingConversation {
                integration_id: String::new(),
                conversation_id: String::new(),
            }],
            start_delay_secs: 0,
            end_delay_secs: 0,
        };
        validate(&config).expect("disabled lifecycle does not validate draft targets");
    }

    #[test]
    fn validate_review_lifecycle_rejects_unusable_targets() {
        let mut config = valid_base();
        config.notifications.channels = vec![valid_notification_channel(NotificationKind::Slack)];
        config.review_lifecycle_notifications = ReviewLifecycleNotificationConfig {
            enabled: true,
            events: vec![ReviewLifecycleEvent::Started],
            targets: vec![ReviewLifecycleTarget::NotificationChannels {
                channel_ids: vec!["missing".to_string()],
            }],
            start_delay_secs: 0,
            end_delay_secs: 0,
        };
        assert!(validate(&config)
            .unwrap_err()
            .message
            .contains("channelIds 不存在"));

        let mut config = valid_base();
        let mut disabled = valid_notification_channel(NotificationKind::Slack);
        disabled.id = "slack-main".to_string();
        disabled.enabled = false;
        config.notifications.channels = vec![disabled];
        config.review_lifecycle_notifications = ReviewLifecycleNotificationConfig {
            enabled: true,
            events: vec![ReviewLifecycleEvent::Started],
            targets: vec![ReviewLifecycleTarget::NotificationChannels {
                channel_ids: vec!["slack-main".to_string()],
            }],
            start_delay_secs: 0,
            end_delay_secs: 0,
        };
        assert!(validate(&config)
            .unwrap_err()
            .message
            .contains("channelIds 已禁用"));

        let mut config = valid_base();
        config.notifications.channels = vec![NotificationChannel {
            enabled: false,
            ..NotificationChannel::desktop_default()
        }];
        config.review_lifecycle_notifications = ReviewLifecycleNotificationConfig {
            enabled: true,
            events: vec![ReviewLifecycleEvent::Started],
            targets: vec![ReviewLifecycleTarget::NotificationChannels {
                channel_ids: Vec::new(),
            }],
            start_delay_secs: 0,
            end_delay_secs: 0,
        };
        assert!(validate(&config)
            .unwrap_err()
            .message
            .contains("没有启用的通知渠道"));

        let mut config = valid_base();
        config.review_lifecycle_notifications = ReviewLifecycleNotificationConfig {
            enabled: true,
            events: vec![ReviewLifecycleEvent::Started],
            targets: vec![ReviewLifecycleTarget::MessagingConversation {
                integration_id: "missing".to_string(),
                conversation_id: "oc_123".to_string(),
            }],
            start_delay_secs: 0,
            end_delay_secs: 0,
        };
        assert!(validate(&config)
            .unwrap_err()
            .message
            .contains("integrationId 不存在"));

        let mut config = valid_base();
        let mut disabled = valid_messaging_integration();
        disabled.enabled = false;
        config.messaging.integrations = vec![disabled];
        config.review_lifecycle_notifications = ReviewLifecycleNotificationConfig {
            enabled: true,
            events: vec![ReviewLifecycleEvent::Started],
            targets: vec![ReviewLifecycleTarget::MessagingConversation {
                integration_id: "feishu-main".to_string(),
                conversation_id: "oc_123".to_string(),
            }],
            start_delay_secs: 0,
            end_delay_secs: 0,
        };
        assert!(validate(&config)
            .unwrap_err()
            .message
            .contains("integrationId 已禁用"));

        let mut config = valid_base();
        config.messaging.integrations = vec![valid_messaging_integration()];
        config.review_lifecycle_notifications = ReviewLifecycleNotificationConfig {
            enabled: true,
            events: vec![ReviewLifecycleEvent::Started],
            targets: vec![ReviewLifecycleTarget::MessagingConversation {
                integration_id: "feishu-main".to_string(),
                conversation_id: "oc_missing".to_string(),
            }],
            start_delay_secs: 0,
            end_delay_secs: 0,
        };
        assert!(validate(&config)
            .unwrap_err()
            .message
            .contains("conversationId 未授权"));
    }

    #[test]
    fn validate_azure_source_requires_org_and_project() {
        // #818: an Azure-source project must supply azureOrg, azureProject, and repo. An
        // empty org or project is rejected, with the message starting with the camelCase
        // wire field token so the wizard's `errorToStep` routes it.
        let azure_base = Project {
            source_kind: SourceKind::Azure,
            repo: "myrepo".to_string(),
            azure_org: "myorg".to_string(),
            azure_project: "myproject".to_string(),
            ..valid_project()
        };
        // A complete Azure project validates.
        assert!(validate_project(&azure_base).is_ok());

        // Empty org → rejected, message starts with `azureOrg`.
        let org_err = validate_project(&Project {
            azure_org: "   ".to_string(),
            ..azure_base.clone()
        })
        .unwrap_err()
        .message;
        assert!(org_err.starts_with("azureOrg"), "{org_err}");

        // Empty project → rejected, message starts with `azureProject`.
        let proj_err = validate_project(&Project {
            azure_project: String::new(),
            ..azure_base.clone()
        })
        .unwrap_err()
        .message;
        assert!(proj_err.starts_with("azureProject"), "{proj_err}");

        // Empty repo → rejected, message starts with `repo` (Azure repo is a bare name,
        // so it does NOT go through the GitHub owner/name check).
        let repo_err = validate_project(&Project {
            repo: "   ".to_string(),
            ..azure_base.clone()
        })
        .unwrap_err()
        .message;
        assert!(repo_err.starts_with("repo"), "{repo_err}");
    }

    #[test]
    fn validate_azure_org_rejects_url_unsafe_chars() {
        // #818 F2: azure_org is interpolated into the discovery URL, so URL-unsafe chars
        // (whitespace, control, `/ # ? @`) are rejected — a `/` could smuggle a path. The
        // message keeps the `azureOrg` routing prefix.
        let azure_base = Project {
            source_kind: SourceKind::Azure,
            repo: "myrepo".to_string(),
            azure_org: "myorg".to_string(),
            azure_project: "myproject".to_string(),
            ..valid_project()
        };
        for bad in ["my/org", "my org", "my#org", "my?org", "my@org", "my\torg"] {
            let err = validate_project(&Project {
                azure_org: bad.to_string(),
                ..azure_base.clone()
            })
            .unwrap_err()
            .message;
            assert!(
                err.starts_with("azureOrg"),
                "org {bad:?} rejected with azureOrg prefix, got {err}"
            );
        }
        // azure_project / repo may contain spaces (separate argv, no URL parsing) → accepted.
        assert!(validate_project(&Project {
            azure_project: "my project".to_string(),
            repo: "my repo".to_string(),
            ..azure_base
        })
        .is_ok());
    }

    #[test]
    fn validate_github_source_unaffected_by_azure_fields() {
        // #818: a GitHub-source project keeps the owner/name repo validation and ignores
        // the (empty) Azure fields — the new branch must not change GitHub behavior.
        assert!(validate_project(&Project {
            source_kind: SourceKind::Github,
            azure_org: String::new(),
            azure_project: String::new(),
            ..valid_project()
        })
        .is_ok());
        // A bad owner/name repo is still rejected for GitHub.
        assert!(validate_project(&Project {
            source_kind: SourceKind::Github,
            repo: "not-a-repo".to_string(),
            ..valid_project()
        })
        .is_err());
    }

    #[test]
    fn validate_bitbucket_source_requires_host_project_token_and_title_labels() {
        // AB#717: a Bitbucket-source project must supply host/project/token + a bare repo
        // slug, and MUST use title-parsed labels (no native labels on Bitbucket Server).
        // Each rejection's message starts with the camelCase wire field so `errorToStep`
        // routes it.
        let bb_base = Project {
            source_kind: SourceKind::Bitbucket,
            repo: "myrepo".to_string(),
            bitbucket_host: "https://bitbucket.example.com".to_string(),
            bitbucket_project: "GOCELL".to_string(),
            bitbucket_token: "secret-pat".to_string(),
            label_source: LabelSource::Title,
            // Bitbucket has no inbound webhook → must use a polling mode (default is
            // webhook-only, which is rejected for Bitbucket — see the updateMode case below).
            update_mode: UpdateMode::PullOnly,
            ..valid_project()
        };
        // A complete Bitbucket project validates.
        assert!(validate_project(&bb_base).is_ok());

        // Empty host → rejected, message starts with `bitbucketHost`.
        let host_err = validate_project(&Project {
            bitbucket_host: "   ".to_string(),
            ..bb_base.clone()
        })
        .unwrap_err()
        .message;
        assert!(host_err.starts_with("bitbucketHost"), "{host_err}");

        // URL-unsafe host chars (whitespace / `# ? @`) rejected with the `bitbucketHost`
        // prefix; `/` and `:` are allowed (the host is a full base URL).
        for bad in ["bitbucket example.com", "host#x", "host?x", "host@x"] {
            let err = validate_project(&Project {
                bitbucket_host: bad.to_string(),
                ..bb_base.clone()
            })
            .unwrap_err()
            .message;
            assert!(err.starts_with("bitbucketHost"), "host {bad:?}: {err}");
        }
        assert!(validate_project(&Project {
            bitbucket_host: "https://bitbucket.example.com:7990/ctx".to_string(),
            ..bb_base.clone()
        })
        .is_ok());

        // Empty project → rejected, message starts with `bitbucketProject`.
        let proj_err = validate_project(&Project {
            bitbucket_project: String::new(),
            ..bb_base.clone()
        })
        .unwrap_err()
        .message;
        assert!(proj_err.starts_with("bitbucketProject"), "{proj_err}");

        // Empty token → rejected, message starts with `bitbucketToken`.
        let token_err = validate_project(&Project {
            bitbucket_token: "   ".to_string(),
            ..bb_base.clone()
        })
        .unwrap_err()
        .message;
        assert!(token_err.starts_with("bitbucketToken"), "{token_err}");

        // Empty repo slug → rejected, message starts with `repo`.
        let repo_err = validate_project(&Project {
            repo: "   ".to_string(),
            ..bb_base.clone()
        })
        .unwrap_err()
        .message;
        assert!(repo_err.starts_with("repo"), "{repo_err}");

        // Native labels on a Bitbucket project → rejected (no native labels exist),
        // message starts with `labelSource`.
        let label_err = validate_project(&Project {
            label_source: LabelSource::Native,
            ..bb_base.clone()
        })
        .unwrap_err()
        .message;
        assert!(label_err.starts_with("labelSource"), "{label_err}");

        // A non-https host → rejected (Bearer PAT must not go over plaintext), message
        // starts with `bitbucketHost`. `http://` passes the char check but fails the scheme check.
        let http_err = validate_project(&Project {
            bitbucket_host: "http://bitbucket.example.com".to_string(),
            ..bb_base.clone()
        })
        .unwrap_err()
        .message;
        assert!(http_err.starts_with("bitbucketHost"), "{http_err}");

        // project / repo are URL path segments → a `/` (and `# ? @` / whitespace) is rejected.
        for bad in ["a/b", "a#b", "a b"] {
            let proj_bad = validate_project(&Project {
                bitbucket_project: bad.to_string(),
                ..bb_base.clone()
            })
            .unwrap_err()
            .message;
            assert!(
                proj_bad.starts_with("bitbucketProject"),
                "proj {bad:?}: {proj_bad}"
            );
            let repo_bad = validate_project(&Project {
                repo: bad.to_string(),
                ..bb_base.clone()
            })
            .unwrap_err()
            .message;
            assert!(repo_bad.starts_with("repo"), "repo {bad:?}: {repo_bad}");
        }
        // `~user` personal project key is allowed (unreserved).
        assert!(validate_project(&Project {
            bitbucket_project: "~alice".to_string(),
            ..bb_base.clone()
        })
        .is_ok());

        // Bitbucket has no inbound webhook → webhook-only / hybrid rejected (message starts
        // with `updateMode`); pull-only / manual accepted.
        for mode in [UpdateMode::WebhookOnly, UpdateMode::Hybrid] {
            let mode_err = validate_project(&Project {
                update_mode: mode,
                ..bb_base.clone()
            })
            .unwrap_err()
            .message;
            assert!(mode_err.starts_with("updateMode"), "{mode:?}: {mode_err}");
        }
        for mode in [UpdateMode::PullOnly, UpdateMode::Manual] {
            assert!(
                validate_project(&Project {
                    update_mode: mode,
                    ..bb_base.clone()
                })
                .is_ok(),
                "{mode:?} should be accepted for Bitbucket"
            );
        }
    }

    #[test]
    fn validate_webhook_fields_only_when_enabled() {
        // Disabled (default) → empty secret / any port is fine.
        assert!(validate(&AppConfig {
            webhook_enabled: false,
            webhook_secret: String::new(),
            ..valid_base()
        })
        .is_ok());

        // Enabled requires a non-empty secret (public endpoint forgery guard) — message
        // keeps the `webhookSecret` routing prefix.
        let secret_err = validate(&AppConfig {
            webhook_enabled: true,
            webhook_secret: "   ".to_string(),
            ..valid_base()
        })
        .unwrap_err()
        .message;
        assert!(secret_err.starts_with("webhookSecret"), "{secret_err}");

        // F8: a too-short (low-entropy) secret is also rejected, with the same routing
        // prefix — empty-only was insufficient for a public HMAC endpoint.
        let short_err = validate(&AppConfig {
            webhook_enabled: true,
            webhook_secret: "shh".to_string(), // 3 chars < WEBHOOK_SECRET_MIN_LEN
            webhook_port: 8787,
            ..valid_base()
        })
        .unwrap_err()
        .message;
        assert!(short_err.starts_with("webhookSecret"), "{short_err}");

        // Enabled requires a non-zero port (with a long-enough secret so the secret check
        // passes and we actually reach the port check).
        let port_err = validate(&AppConfig {
            webhook_enabled: true,
            webhook_secret: "webhook-secret-0123456789".to_string(),
            webhook_port: 0,
            ..valid_base()
        })
        .unwrap_err()
        .message;
        assert!(port_err.starts_with("webhookPort"), "{port_err}");

        // Enabled + sufficiently long secret + non-zero port → ok.
        assert!(validate(&AppConfig {
            webhook_enabled: true,
            webhook_secret: "webhook-secret-0123456789".to_string(),
            webhook_port: 8787,
            ..valid_base()
        })
        .is_ok());
    }

    // AB#1043: the local API token has no enable flag — an EMPTY token is the "disabled"
    // sentinel (always valid), but a NON-empty token must clear `LOCAL_API_TOKEN_MIN_LEN` (a
    // short token is brute-forceable by a local process). Mirrors the `webhookSecret` length
    // gate. This is the Medium carrier for that validate rule (without a test the rule would
    // be an untested Soft check).
    #[test]
    fn validate_local_api_token_min_length() {
        // Empty token = disabled → valid (the baseline already leaves it empty).
        assert!(validate(&valid_base()).is_ok());

        // A too-short non-empty token is rejected, routed by the `localApiToken` field prefix.
        let short_err = validate(&AppConfig {
            local_api_token: "shh".to_string(), // 3 chars < LOCAL_API_TOKEN_MIN_LEN
            ..valid_base()
        })
        .unwrap_err()
        .message;
        assert!(short_err.starts_with("localApiToken"), "{short_err}");

        // A sufficiently long token is accepted.
        assert!(validate(&AppConfig {
            local_api_token: "local-api-token-0123456789".to_string(),
            ..valid_base()
        })
        .is_ok());
    }

    #[test]
    fn validate_command_mode_requires_tunnel_command() {
        let base = AppConfig {
            webhook_enabled: true,
            webhook_secret: "webhook-secret-0123456789".to_string(),
            webhook_port: 8787,
            ..valid_base()
        };

        // command 模式 + 空命令（含纯空白）→ err，且消息带 `webhookTunnelCommand` 路由前缀。
        for blank in ["", "   "] {
            let err = validate(&AppConfig {
                webhook_tunnel_mode: WebhookTunnelMode::Command,
                webhook_tunnel_command: blank.to_string(),
                ..base.clone()
            })
            .unwrap_err()
            .message;
            assert!(err.starts_with("webhookTunnelCommand"), "{err}");
        }

        // command 模式 + 有命令 → ok。
        assert!(validate(&AppConfig {
            webhook_tunnel_mode: WebhookTunnelMode::Command,
            webhook_tunnel_command:
                "cloudflared tunnel run --url http://127.0.0.1:{port} my-tunnel".to_string(),
            ..base.clone()
        })
        .is_ok());

        // listener / quick 模式不要求命令（空命令仍 ok）。
        assert!(validate(&AppConfig {
            webhook_tunnel_mode: WebhookTunnelMode::Listener,
            webhook_tunnel_command: String::new(),
            ..base.clone()
        })
        .is_ok());
        assert!(validate(&AppConfig {
            webhook_tunnel_mode: WebhookTunnelMode::Quick,
            webhook_tunnel_command: String::new(),
            ..base
        })
        .is_ok());
    }

    /// Upstream lock for the `errorToStep` routing contract (PR #41 F4, Medium).
    /// `errorToStep` (src/config/fields.ts) routes a backend validation error to
    /// the onboarding step that owns the field by matching the message's leading
    /// field token — checking `skill` first (the path-escape message names both
    /// skill and repoRoot) and `repoRoot` before `repo` (since "repoRoot" has
    /// "repo" as a prefix). This pins that each per-project `validate_project`
    /// failure message starts with the token downstream relies on, so a Rust-side
    /// wording change that would silently break wizard routing fails CI here. The
    /// matching downstream cases live in fields.test.ts; the shared field tokens are
    /// the cross-end contract.
    #[test]
    fn validate_error_messages_start_with_routing_field_token() {
        let base = valid_project();
        let msg = |p: Project| validate_project(&p).unwrap_err().message;

        let repo_err = msg(Project {
            repo: "not-a-repo".to_string(),
            ..base.clone()
        });
        assert!(repo_err.starts_with("repo"), "{repo_err}");
        // Must NOT also start with "repoRoot", or errorToStep (which checks repoRoot
        // first) would route the repo error to the wrong step.
        assert!(!repo_err.starts_with("repoRoot"), "{repo_err}");

        assert!(msg(Project {
            repo_root: String::new(),
            ..base.clone()
        })
        .starts_with("repoRoot"));
        assert!(msg(Project {
            skill_rel_path: "/etc/hosts".to_string(),
            ..base.clone()
        })
        .starts_with("skill"));
        assert!(msg(Project {
            poll_interval_secs: 0,
            ..base.clone()
        })
        .starts_with("pollIntervalSecs"));
        assert!(msg(Project {
            pr_cooldown_seconds: 0,
            ..base.clone()
        })
        .starts_with("prCooldownSeconds"));
    }

    // Locks the serde-ignores-unknown-fields behavior the #11 reservation relies
    // on (the "do not add deny_unknown_fields" comment is otherwise only a Soft
    // note). An older/newer persisted config with extra keys must still load.
    #[test]
    fn unknown_fields_are_ignored() {
        let parsed: AppConfig =
            serde_json::from_value(serde_json::json!({"activeProjectId": "x", "futureField": 42}))
                .expect("unknown fields are ignored");
        assert_eq!(parsed.active_project_id, "x");
    }

    // Forward-compat lock: `#[serde(default)]` lets older/partial persisted
    // configs deserialize, filling absent fields from `Default`. This guards the
    // #11 reservation — adding a field must never break existing stored configs.
    #[test]
    fn empty_object_deserializes_to_default() {
        let parsed: AppConfig = serde_json::from_value(serde_json::json!({}))
            .expect("empty object deserializes via serde(default)");
        assert_eq!(
            serde_json::to_value(&parsed).expect("parsed serializes"),
            serde_json::to_value(AppConfig::default()).expect("default serializes")
        );
    }

    #[test]
    fn partial_object_fills_rest_from_default() {
        let parsed: AppConfig = serde_json::from_value(serde_json::json!({"activeProjectId": "x"}))
            .expect("partial object deserializes via serde(default)");
        let expected = AppConfig {
            active_project_id: "x".to_string(),
            ..AppConfig::default()
        };
        assert_eq!(
            serde_json::to_value(&parsed).expect("parsed serializes"),
            serde_json::to_value(&expected).expect("expected serializes")
        );
    }

    // Forward-compat lock for the new `Project` element: a project object missing
    // fields fills them from `Project::default()` (same `#[serde(default)]`
    // contract as `AppConfig`).
    #[test]
    fn project_partial_object_fills_rest_from_default() {
        let parsed: Project = serde_json::from_value(serde_json::json!({"id": "p1"}))
            .expect("partial project deserializes via serde(default)");
        let expected = Project {
            id: "p1".to_string(),
            ..Project::default()
        };
        assert_eq!(
            serde_json::to_value(&parsed).expect("parsed serializes"),
            serde_json::to_value(&expected).expect("expected serializes")
        );
    }

    /// [`ListenerAuthMode`] lowercase wire-value lock (AB#1064, **Medium carrier**): the
    /// literal wire strings are the cross-end contract with the TS `LISTENER_AUTH_MODES`
    /// `as const` array (same spirit as `listener_kind_wire_values_are_kebab`).
    #[test]
    fn listener_auth_mode_wire_values_are_lowercase() {
        use serde_json::json;
        assert_eq!(
            serde_json::to_value(ListenerAuthMode::None).unwrap(),
            json!("none")
        );
        assert_eq!(
            serde_json::to_value(ListenerAuthMode::Bearer).unwrap(),
            json!("bearer")
        );
    }

    /// #1553 HTTPS-only publicUrl (Medium runtime guard): a plaintext `http://` public URL on
    /// a TUNNEL is rejected; the message keeps the
    /// `publicUrl` field-token prefix.
    #[test]
    fn validate_rejects_tunnel_http_public_url() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![remote_entrypoint("entry", 9100)],
                tunnels: vec![RemoteTunnel {
                    public_url: "http://example.com".to_string(),
                    ..remote_tunnel("t1", "entry")
                }],
            },
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("publicUrl"), "{err}");
    }

    /// AB#1073 publicUrl (Medium runtime guard): an https URL with embedded userinfo
    /// (`https://user:pass@host`) is rejected — the app must not stash plaintext credentials in
    /// config (mirrors the `bitbucketHost` `@`-rejection precedent). Message keeps the
    /// `publicUrl` prefix.
    #[test]
    fn validate_rejects_userinfo_in_public_url() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![remote_entrypoint("entry", 9100)],
                tunnels: vec![RemoteTunnel {
                    public_url: "https://user:pass@example.com".to_string(),
                    ..remote_tunnel("t1", "entry")
                }],
            },
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("publicUrl"), "{err}");
    }

    /// #1553 port-conflict (Medium runtime guard): an enabled entrypoint colliding with the
    /// (enabled) webhook receiver port is rejected; the message keeps the `port` prefix. The
    /// webhook block must pass first, so the secret is long enough and the mode is the default
    /// `quick` (no tunnel command required) to reach the port-conflict check.
    #[test]
    fn validate_rejects_webhook_entrypoint_port_conflict() {
        let config = AppConfig {
            webhook_enabled: true,
            webhook_port: 9000,
            webhook_secret: "webhook-secret-0123456789".to_string(),
            remote_access: RemoteAccessConfig {
                entrypoints: vec![remote_entrypoint("entry", 9000)],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("port"), "{err}");
    }

    /// #1553 port-conflict (Medium runtime guard): local-api is an enabled route on an entrypoint,
    /// so two enabled entrypoints on the same port are rejected through the same port claim.
    #[test]
    fn validate_rejects_local_api_entrypoint_port_conflict() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![
                    remote_entrypoint("local-api", 9000),
                    remote_entrypoint("other", 9000),
                ],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("port"), "{err}");
    }

    /// #1553 port-conflict (Medium runtime guard): a disabled entrypoint is excluded from the
    /// port-conflict check because only enabled entrypoints claim a port.
    #[test]
    fn validate_disabled_entrypoint_excluded_from_port_conflict() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![
                    remote_entrypoint("enabled", 9000),
                    RemoteEntrypoint {
                        id: "disabled".to_string(),
                        name: "disabled".to_string(),
                        port: 9000,
                        enabled: false,
                        ..RemoteEntrypoint::default()
                    },
                ],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        };
        assert!(validate(&config).is_ok());
    }

    /// #1553 port-0 reject (Medium runtime guard): an enabled entrypoint with `port == 0`
    /// (the unset/won't-bind sentinel) is unbindable, so it is rejected; the message keeps the
    /// `port` prefix.
    #[test]
    fn validate_rejects_enabled_entrypoint_zero_port() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![remote_entrypoint("entry", 0)],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("port"), "{err}");
    }

    /// #1553: an enabled entrypoint may bind a non-loopback host. Remote exposure is controlled by
    /// sourcePolicy and route/capability gates rather than by a legacy save-time loopback-only rule.
    #[test]
    fn validate_accepts_enabled_non_loopback_bind_host() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![RemoteEntrypoint {
                    bind_host: "0.0.0.0".to_string(),
                    source_policy: SourcePolicy {
                        mode: SourcePolicyMode::Lan,
                        allow: Vec::new(),
                    },
                    ..remote_entrypoint("entry", 9000)
                }],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        };
        assert!(validate(&config).is_ok());
    }

    /// #1553 bindHost gate — the accept case: an enabled entrypoint bound to loopback validates.
    #[test]
    fn validate_accepts_enabled_loopback_bind_host() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![remote_entrypoint("entry", 9000)],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        };
        assert!(validate(&config).is_ok());
    }

    /// #1553 bindHost gate — the exemption: a disabled entrypoint with a blank bindHost is
    /// NOT rejected because it never binds.
    #[test]
    fn validate_exempts_disabled_non_loopback_bind_host() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![RemoteEntrypoint {
                    id: "disabled".to_string(),
                    name: "disabled".to_string(),
                    bind_host: String::new(),
                    enabled: false,
                    ..RemoteEntrypoint::default()
                }],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        };
        assert!(validate(&config).is_ok());
    }

    /// #1553 — duplicate entrypoint id is rejected because the supervisor/tunnel resolution folds
    /// entrypoints by id.
    #[test]
    fn validate_rejects_duplicate_entrypoint_id() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![
                    remote_entrypoint("dup", 9000),
                    remote_entrypoint("dup", 9001),
                ],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("entrypointId"), "{err}");
    }

    /// #1553 — an empty entrypoint id is rejected because enabled tunnels target entrypoints by id.
    #[test]
    fn validate_rejects_empty_entrypoint_id() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![remote_entrypoint("", 9000)],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("entrypointId"), "{err}");
    }

    /// #1553 follow-up — routePath is later reused as HTML/script data by the terminal shell, so
    /// config validation is the single ingress that rejects bytes unsafe outside URL path segments.
    #[test]
    fn validate_rejects_route_path_html_and_script_delimiters() {
        for path in [
            "/term\"inal",
            "/term<inal",
            "/term>inal",
            "/terminal</script><script>alert(1)</script>",
            "/terminal?x=1",
            "/terminal#hash",
            "/terminal%2fadmin",
            "/term inal",
        ] {
            let mut entrypoint = remote_entrypoint("entry", 9000);
            entrypoint.routes[0].path = path.to_string();
            let config = AppConfig {
                remote_access: RemoteAccessConfig {
                    entrypoints: vec![entrypoint],
                    tunnels: Vec::new(),
                },
                ..AppConfig::default()
            };
            let err = validate(&config).unwrap_err().message;
            assert!(err.starts_with("routePath"), "{path}: {err}");
        }
    }

    #[test]
    fn validate_accepts_route_path_url_safe_segments() {
        let mut entrypoint = remote_entrypoint("entry", 9000);
        entrypoint.routes[0].path = "/terminal-v1/api_2/~health.check".to_string();
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![entrypoint],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        };
        assert!(validate(&config).is_ok());
    }

    /// AB#1225 F3 — duplicate tunnel id is rejected (tunnel ids are also a lookup key space). One
    /// enabled entrypoint target so the tunnels are otherwise coherent and the id-uniqueness reject is
    /// what surfaces; the message keeps the `tunnelId` prefix.
    #[test]
    fn validate_rejects_duplicate_tunnel_id() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![remote_entrypoint("entry", 9000)],
                tunnels: vec![remote_tunnel("dup", "entry"), remote_tunnel("dup", "entry")],
            },
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("tunnelId"), "{err}");
    }

    /// AB#1225 F3 — an empty tunnel id is rejected. The message keeps the `tunnelId` prefix.
    #[test]
    fn validate_rejects_empty_tunnel_id() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: Vec::new(),
                tunnels: vec![RemoteTunnel {
                    id: String::new(),
                    enabled: false,
                    ..RemoteTunnel::default()
                }],
            },
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("tunnelId"), "{err}");
    }

    /// AB#1064 reference integrity (Medium runtime guard): an ENABLED tunnel with an empty
    /// `targetEntrypointId` is rejected (an enabled tunnel must name its target).
    #[test]
    fn validate_rejects_enabled_tunnel_empty_target() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: Vec::new(),
                tunnels: vec![remote_tunnel("t1", "")],
            },
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("targetEntrypointId"), "{err}");
    }

    /// AB#1064 reference integrity (Medium runtime guard): an ENABLED tunnel pointing at a
    /// `targetEntrypointId` that matches no entrypoint is rejected.
    #[test]
    fn validate_rejects_enabled_tunnel_dangling_target() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: Vec::new(),
                tunnels: vec![remote_tunnel("t1", "nope")],
            },
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("targetEntrypointId"), "{err}");
    }

    /// AB#1064 reference integrity (Medium runtime guard): an ENABLED tunnel pointing at a
    /// disabled entrypoint is rejected because the tunnel could never carry traffic.
    #[test]
    fn validate_rejects_enabled_tunnel_disabled_target() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![RemoteEntrypoint {
                    id: "entry".to_string(),
                    name: "entry".to_string(),
                    enabled: false,
                    ..RemoteEntrypoint::default()
                }],
                tunnels: vec![remote_tunnel("t1", "entry")],
            },
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("targetEntrypointId"), "{err}");
    }

    /// AB#1064 reference integrity (Medium runtime guard): an ENABLED tunnel pointing at an
    /// existing enabled entrypoint validates.
    #[test]
    fn validate_accepts_enabled_tunnel_valid_target() {
        let config = AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![remote_entrypoint("entry", 9000)],
                tunnels: vec![remote_tunnel("t1", "entry")],
            },
            ..AppConfig::default()
        };
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn validate_terminal_route_requires_bearer_token_and_read_permission() {
        let terminal_route = RemoteRoute::terminal();
        let terminal_entrypoint = |route: RemoteRoute| RemoteEntrypoint {
            id: "term".to_string(),
            name: "Terminal".to_string(),
            port: 9100,
            routes: vec![route],
            ..remote_entrypoint("term", 9100)
        };

        let auth_err = validate(&AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![terminal_entrypoint(terminal_route.clone())],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        })
        .unwrap_err()
        .message;
        assert!(auth_err.starts_with("authToken"), "{auth_err}");

        let token_err = validate(&AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![terminal_entrypoint(RemoteRoute {
                    auth_token: "x".repeat(31),
                    ..terminal_route.clone()
                })],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        })
        .unwrap_err()
        .message;
        assert!(token_err.starts_with("authToken"), "{token_err}");
        assert!(token_err.contains("至少 32 个字符"), "{token_err}");

        let read_err = validate(&AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![terminal_entrypoint(RemoteRoute {
                    auth_token: "x".repeat(32),
                    terminal_read: false,
                    ..terminal_route.clone()
                })],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        })
        .unwrap_err()
        .message;
        assert!(read_err.starts_with("terminalRead"), "{read_err}");

        assert!(validate(&AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![terminal_entrypoint(RemoteRoute {
                    auth_token: "x".repeat(32),
                    terminal_read: true,
                    ..terminal_route
                })],
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        })
        .is_ok());
    }

    #[test]
    fn validate_enabled_command_tunnel_requires_per_tunnel_command() {
        let tunnel = RemoteTunnel {
            id: "tun".to_string(),
            name: "Terminal Tunnel".to_string(),
            enabled: true,
            mode: RemoteTunnelMode::Command,
            target_entrypoint_id: "term".to_string(),
            command: String::new(),
            ..RemoteTunnel::default()
        };

        let err = validate(&AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![remote_entrypoint("term", 9100)],
                tunnels: vec![tunnel.clone()],
            },
            ..AppConfig::default()
        })
        .unwrap_err()
        .message;
        assert!(err.starts_with("command"), "{err}");

        assert!(validate(&AppConfig {
            remote_access: RemoteAccessConfig {
                entrypoints: vec![remote_entrypoint("term", 9100)],
                tunnels: vec![RemoteTunnel {
                    command: "cloudflared tunnel --url http://127.0.0.1:{port}".to_string(),
                    ..tunnel
                }],
            },
            ..AppConfig::default()
        })
        .is_ok());
    }

    /// Forward-compat lock for [`Listener`] (AB#1064): a listener object missing fields fills
    /// them from `Listener::default()` (same `#[serde(default)]` contract as `Project`).
    #[test]
    fn listener_partial_object_fills_rest_from_default() {
        let parsed: Listener = serde_json::from_value(serde_json::json!({"id": "l1"}))
            .expect("partial listener deserializes via serde(default)");
        let expected = Listener {
            id: "l1".to_string(),
            ..Listener::default()
        };
        assert_eq!(
            serde_json::to_value(&parsed).expect("parsed serializes"),
            serde_json::to_value(&expected).expect("expected serializes")
        );
    }

    /// Forward-compat lock for [`Tunnel`] (AB#1064): a tunnel object missing fields fills them
    /// from `Tunnel::default()` (same `#[serde(default)]` contract as `Project`).
    #[test]
    fn tunnel_partial_object_fills_rest_from_default() {
        let parsed: Tunnel = serde_json::from_value(serde_json::json!({"id": "t1"}))
            .expect("partial tunnel deserializes via serde(default)");
        let expected = Tunnel {
            id: "t1".to_string(),
            ..Tunnel::default()
        };
        assert_eq!(
            serde_json::to_value(&parsed).expect("parsed serializes"),
            serde_json::to_value(&expected).expect("expected serializes")
        );
    }
}
