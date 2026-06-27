//! Config slice domain model.

use std::path::Path;

use lettre::message::Mailbox;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{AppError, AppResult};
use crate::model::{
    EngineKind, EventType, LabelSource, NotificationKind, SourceKind, UpdateMode, WebhookTunnelMode,
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
    pub actions: Vec<RuleActionKind>,
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
    /// 本地 REST API 的 Bearer token（AB#1043）。**空 = fail-closed 禁用**：listener 仍绑定，但
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
    /// Declarative Remote Access listeners. The remote supervisor consumes bindable loopback
    /// listeners at startup and after config saves.
    pub listeners: Vec<Listener>,
    /// Declarative Remote Access tunnels. `mode` reuses WebhookTunnelMode and is reconciled
    /// against currently bound target listeners.
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
            // AB#1225: 全新安装默认开启本地触发 API（端口 8788 = webhook 默认 8787 + 1，避免
            // 两端口同默认时冲突），现以 `listeners[]` 的 local-api 条目表达——supervisor 绑定的
            // 单一真值源（取代旧的 `local_api_port` 字段）。
            listeners: vec![default_local_api_listener()],
            tunnels: Vec::new(),
        }
    }
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

/// Stable `id` of the seeded local-api listener (AB#1225). Single source for the literal so the
/// model's [`default_local_api_listener`] and the service's `seed_local_api_listener` (detect-by-id
/// / seeded `id`) and its seeded `kind` string agree. NOTE: this wire string equals
/// [`ListenerKind::LocalApi`]'s serde value (`"local-api"`), golden-locked by
/// `listener_kind_wire_values_are_kebab` — keep the two in sync if either changes.
pub(crate) const LOCAL_API_LISTENER_ID: &str = "local-api";

/// Loopback bindHost whitelist (AB#1225). Single source for both save-time validation here and
/// the runtime supervisor's fail-closed gate. Whitelist, never blacklist.
///
/// Narrowed (F2) to ONLY the literal `127.0.0.1` the supervisor actually binds (`bind_std_with_retry`
/// always binds `("127.0.0.1", port)` and the status text is fixed `127.0.0.1:{p}`): accepting
/// `localhost` / `::1` / `[::1]` here would let a config value pass save-time validation while the
/// runtime silently bound a DIFFERENT address than the one configured — a config/runtime mismatch.
/// `localhost` / `::1` are simply NOT YET supported as a `bindHost` value (no compat shim — they were
/// never wired to a distinct bind). NOTE: this is a SEPARATE concern from
/// `review::local_api`'s `security::host_allowed`, which validates the incoming request `Host` HEADER
/// (and correctly accepts `localhost` / `::1` for loopback clients) — do not conflate the two.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    host.trim() == "127.0.0.1"
}

/// The default local-api listener (AB#1225). Fresh installs get the local trigger API ON at
/// 8788 (was the old `local_api_port` default), now expressed as a `listeners[]` entry — the
/// single source of truth the supervisor binds. `auth: Bearer` is descriptive; the token is the
/// global `local_api_token`, read live by the handler (a per-listener token is an AB#1073 follow-up).
pub(crate) fn default_local_api_listener() -> Listener {
    Listener {
        id: LOCAL_API_LISTENER_ID.to_string(),
        name: "Local API".to_string(),
        kind: ListenerKind::LocalApi,
        bind_host: "127.0.0.1".to_string(),
        port: 8788,
        enabled: true,
        auth: ListenerAuthMode::Bearer,
        auth_token: String::new(),
        terminal_read: false,
        terminal_write: false,
        terminal_create: false,
        terminal_admin: false,
        allowed_origins: Vec::new(),
        public_url: String::new(),
    }
}

/// A declarative tunnel descriptor. Reuses WebhookTunnelMode (quick/command/listener); enabled
/// tunnels are reconciled against currently bound target listeners.
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
const REMOTE_WEB_TOKEN_MIN_LEN: usize = 32;

pub(crate) fn terminal_auth_token_is_strong(token: &str) -> bool {
    token.trim().chars().count() >= LOCAL_API_TOKEN_MIN_LEN
}

pub(crate) fn remote_web_auth_token_is_strong(token: &str) -> bool {
    token.trim().chars().count() >= REMOTE_WEB_TOKEN_MIN_LEN
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

fn validate_rule(rule: &RuleConfig) -> AppResult<()> {
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
    for (idx, action) in rule.actions.iter().enumerate() {
        if rule.actions[..idx].contains(action) {
            return Err(AppError::new(format!("actions 不能重复: {:?}", action)));
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
        validate_rule(rule).map_err(|e| AppError::new(format!("rules[{idx}].{}", e.message)))?;
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

    // Remote Access (AB#1073) — config-save fail-fast runtime guard (Medium carrier). Scope is
    // intentionally the two SIMPLEST checks only (HTTPS-only public URLs + no enabled-port
    // collisions); per-listener auth/origin semantics are deferred with the runtime (AB#1064).
    //
    // 1. HTTPS-only publicUrl: every listener/tunnel `public_url`, when set, must be an
    //    `https://` URL with NO embedded credentials — a remote ingress over plaintext HTTP
    //    would expose tokens/traffic, and a `https://user:pass@host` URL would stash a
    //    plaintext credential in the config (same precedent as the `bitbucketHost` `@`-rejection
    //    above; and thin-orchestrator: the app must never hold credentials — see
    //    `[[app-thin-orchestrator-decouple]]`). An empty value is "unset" → skipped (mirrors
    //    webhook fields being unconstrained when unset). The label names the offending resource
    //    (监听器/隧道 + name) so the message points at WHICH entry failed; the message still
    //    starts with the `publicUrl` field-token prefix (golden/routing contract).
    for (src, url) in config
        .listeners
        .iter()
        .map(|l| (format!("监听器「{}」", l.name), &l.public_url))
        .chain(
            config
                .tunnels
                .iter()
                .map(|t| (format!("隧道「{}」", t.name), &t.public_url)),
        )
    {
        let url = url.trim();
        if url.is_empty() {
            continue;
        }
        // Reject non-https schemes AND embedded credentials. `url` crate v2: `.username()`
        // returns `&str` (empty when absent), `.password()` returns `Option<&str>`.
        let is_https_no_creds = Url::parse(url)
            .map(|u| u.scheme() == "https" && u.username().is_empty() && u.password().is_none())
            .unwrap_or(false);
        if !is_https_no_creds {
            return Err(AppError::new(format!(
                "publicUrl 必须是 https:// 开头的 URL（{src}，远程入口不能走明文 HTTP/不能内嵌凭据）: {url}"
            )));
        }
    }

    // 1b. Listener / tunnel `id` uniqueness (AB#1225 F3): every listener `id` and every tunnel `id`
    //     must be non-empty and unique. These ids are HashMap-by-id lookup keys — the supervisor's
    //     `desired_ports` / runtime map and `listener_enabled_by_id`, plus tunnel `target_listener_id`
    //     resolution — so a duplicate id would silently fold two entries into one (last writer wins),
    //     and an empty id can't be addressed by a tunnel target. Checked for EVERY listener/tunnel
    //     (enabled or not): a disabled entry's id still occupies the id space the moment it is enabled,
    //     and a tunnel may target a (currently) disabled listener by id. Messages keep the
    //     `listenerId` / `tunnelId` field-token prefix (cross-end routing contract). The Hard path
    //     (future) is a typed-id newtype whose constructor rejects empties at the type level.
    let mut seen_listener_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for listener in &config.listeners {
        let id = listener.id.trim();
        if id.is_empty() {
            return Err(AppError::new(format!(
                "listenerId 不能为空（监听器「{}」需要一个唯一 id）",
                listener.name
            )));
        }
        if !seen_listener_ids.insert(id) {
            return Err(AppError::new(format!(
                "listenerId 重复: {id}（每个监听器的 id 必须唯一）"
            )));
        }
    }
    let mut seen_tunnel_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for tunnel in &config.tunnels {
        let id = tunnel.id.trim();
        if id.is_empty() {
            return Err(AppError::new(format!(
                "tunnelId 不能为空（隧道「{}」需要一个唯一 id）",
                tunnel.name
            )));
        }
        if !seen_tunnel_ids.insert(id) {
            return Err(AppError::new(format!(
                "tunnelId 重复: {id}（每个隧道的 id 必须唯一）"
            )));
        }
    }

    let remote_web_has_public_entry = |listener_id: &str| -> bool {
        config
            .listeners
            .iter()
            .any(|l| l.id == listener_id && !l.public_url.trim().is_empty())
            || config.tunnels.iter().any(|t| {
                t.enabled
                    && t.target_listener_id.trim() == listener_id
                    && (t.mode == WebhookTunnelMode::Quick || !t.public_url.trim().is_empty())
            })
    };

    // 2. Port conflict: every ENABLED listening port must be unique. Sources are the webhook
    //    receiver (when enabled) and each enabled listener with a non-zero port (the local REST
    //    API is now one such listener — kind `local-api` — not a standalone field, AB#1225). The
    //    label (port → human name) makes the first collision's message name both occupants.
    // Claim order is fixed (webhook → listeners in declared order), so the first collision — and
    // thus the conflict error message — is deterministic despite HashMap being an unordered
    // container (we only ever read `insert`'s returned prior value, never iterate).
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
    for listener in &config.listeners {
        if !listener.enabled {
            continue;
        }
        // An enabled listener with `port == 0` is unbindable (`0` = unset/won't-bind
        // sentinel), so reject it before the conflict claim (which intentionally skips
        // `port == 0`). The message keeps the `port` field-token prefix.
        if listener.port == 0 {
            return Err(AppError::new(format!(
                "port 必须大于 0（监听器「{}」已启用但端口为 0/未设置）",
                listener.name
            )));
        }
        // AB#1225 security gate: an ENABLED listener MUST bind a loopback host. Remote exposure
        // (`0.0.0.0` / LAN IP / hostname) is deferred to AB#1073 (per-listener auth/origin), so a
        // non-loopback enabled listener is rejected here — fail-closed, and via the SAME whitelist
        // ([`is_loopback_host`]) the runtime supervisor enforces at bind time, so save-time and
        // bind-time agree. Ordered AFTER the port-0 reject (a 0-port listener is unbindable
        // regardless of host, so that diagnostic wins) and BEFORE the conflict claim (a
        // remote-exposed listener never reaches port-arbitration). Disabled listeners are exempt
        // (they never bind), mirroring the disabled port-0 / port-conflict exemptions. The message
        // keeps the `bindHost` field-token prefix so SettingsView's `errorToStep` routes it to the
        // listener's bindHost field.
        if !is_loopback_host(&listener.bind_host) {
            return Err(AppError::new(format!(
                "bindHost 仅支持 127.0.0.1（监听器「{}」远程暴露需 AB#1073；localhost/::1 暂不支持）: {}",
                listener.name, listener.bind_host
            )));
        }
        if listener.kind == ListenerKind::Terminal {
            if listener.auth != ListenerAuthMode::Bearer {
                return Err(AppError::new(format!(
                    "auth 终端监听器「{}」必须使用 bearer 鉴权",
                    listener.name
                )));
            }
            if !terminal_auth_token_is_strong(&listener.auth_token) {
                return Err(AppError::new(format!(
                    "authToken 终端监听器「{}」必须配置至少 {LOCAL_API_TOKEN_MIN_LEN} 个字符的 Bearer token",
                    listener.name
                )));
            }
            if !listener.terminal_read {
                return Err(AppError::new(format!(
                    "terminalRead 终端监听器「{}」必须至少开启读取权限",
                    listener.name
                )));
            }
        }
        if listener.kind == ListenerKind::RemoteWeb {
            if listener.auth != ListenerAuthMode::Bearer {
                return Err(AppError::new(format!(
                    "auth 远程面板监听器「{}」必须使用 bearer 鉴权",
                    listener.name
                )));
            }
            if !remote_web_auth_token_is_strong(&listener.auth_token) {
                return Err(AppError::new(format!(
                    "authToken 远程面板监听器「{}」必须配置至少 {REMOTE_WEB_TOKEN_MIN_LEN} 个字符的 Bearer token",
                    listener.name
                )));
            }
            if !listener.allowed_origins.is_empty() {
                return Err(AppError::new(format!(
                    "allowedOrigins 远程面板监听器「{}」不支持手填 Origin；Host/Origin 仅从 publicUrl 或目标隧道派生",
                    listener.name
                )));
            }
            if !remote_web_has_public_entry(&listener.id) {
                return Err(AppError::new(format!(
                    "publicUrl 远程面板监听器「{}」必须配置 HTTPS publicUrl，或启用指向它的 HTTPS/Quick 隧道",
                    listener.name
                )));
            }
        }
        // Label the occupant by its user-facing name, falling back to a short id when the
        // name is empty (so the conflict message points at WHICH listener a human recognizes,
        // not the internal id). webhook/localApi labels stay as their field tokens above.
        let label = if listener.name.trim().is_empty() {
            format!("监听器 {}", listener.id.chars().take(8).collect::<String>())
        } else {
            format!("监听器「{}」", listener.name.trim())
        };
        claim(listener.port, label)?;
    }

    // 3. Reference integrity for ENABLED tunnels (AB#1064 declarative wiring): an enabled
    //    tunnel's `target_listener_id` must (a) be non-empty, (b) match some listener's `id`,
    //    and (c) point at an ENABLED listener — a tunnel that exposes a missing/disabled
    //    listener can never carry traffic, so the config is incoherent. Disabled tunnels are
    //    unconstrained (skipped). The message keeps the `targetListenerId` field-token prefix.
    let listener_enabled_by_id: std::collections::HashMap<&str, (bool, ListenerKind)> = config
        .listeners
        .iter()
        .map(|l| (l.id.as_str(), (l.enabled, l.kind)))
        .collect();
    for tunnel in &config.tunnels {
        if !tunnel.enabled {
            continue;
        }
        if tunnel.mode == WebhookTunnelMode::Command && tunnel.command.trim().is_empty() {
            return Err(AppError::new(format!(
                "command 不能为空（隧道「{}」为 command 模式时需填写隧道命令，可用 {{port}} 占位）",
                tunnel.name
            )));
        }
        let target = tunnel.target_listener_id.trim();
        if target.is_empty() {
            return Err(AppError::new(format!(
                "targetListenerId 不能为空（隧道「{}」已启用，需指定目标监听器）",
                tunnel.name
            )));
        }
        match listener_enabled_by_id.get(target) {
            None => {
                return Err(AppError::new(format!(
                    "targetListenerId 不指向任何监听器（隧道「{}」: {target}）",
                    tunnel.name
                )));
            }
            Some((false, _)) => {
                return Err(AppError::new(format!(
                    "targetListenerId 指向未启用的监听器（隧道「{}」→ 目标未启用）",
                    tunnel.name
                )));
            }
            Some((true, ListenerKind::LocalApi)) => {
                return Err(AppError::new(format!(
                    "targetListenerId 不能指向 local-api（隧道「{}」会暴露本机 CLI API）",
                    tunnel.name
                )));
            }
            Some((true, _)) => {}
        }
    }

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
        // AB#1043: local REST API token present (camelCase) at the top level. (The port is no
        // longer a top-level field — AB#1225 moved it into a `listeners[]` local-api entry.)
        assert!(v.get("localApiToken").is_some());
        // AB#1064: Remote Access collections present at the top level.
        assert!(v.get("listeners").is_some());
        assert!(v.get("tunnels").is_some());
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
                RuleActionKind::Review,
                RuleActionKind::Check,
                RuleActionKind::Notify,
            ],
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
                "actions": ["review", "check", "notify"]
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

    /// Default local-api listener lock (AB#1225, Medium): a fresh install ships exactly ONE
    /// listener — the local trigger API, enabled at `127.0.0.1:8788` — which is the single source
    /// of truth the supervisor binds (replacing the old `local_api_port` field). A silent change
    /// to the seeded default (off / different port / wrong kind) would break "local API on by
    /// default" for new installs, so it is machine-checked here. No tunnels are seeded — nothing
    /// public is exposed until the user adds one.
    #[test]
    fn default_seeds_local_api_listener() {
        let listeners = AppConfig::default().listeners;
        assert_eq!(listeners.len(), 1);
        let l = &listeners[0];
        assert_eq!(l.kind, ListenerKind::LocalApi);
        assert!(l.enabled);
        assert_eq!(l.port, 8788);
        assert_eq!(l.bind_host, "127.0.0.1");
        assert!(AppConfig::default().tunnels.is_empty());
    }

    /// AB#1073 HTTPS-only publicUrl (Medium runtime guard): a plaintext `http://` public URL
    /// is rejected; the message keeps the `publicUrl` field-token prefix.
    #[test]
    fn validate_rejects_http_public_url() {
        let config = AppConfig {
            listeners: vec![Listener {
                public_url: "http://example.com".to_string(),
                ..sample_listener()
            }],
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("publicUrl"), "{err}");
    }

    /// AB#1073 HTTPS-only publicUrl (Medium runtime guard): an `https://` public URL with
    /// distinct ports validates.
    #[test]
    fn validate_accepts_https_public_url() {
        let config = AppConfig {
            // Override `listeners` so only this one listener exists (no seeded default local-api
            // listener) — only the HTTPS path is exercised here.
            listeners: vec![Listener {
                port: 9100,
                public_url: "https://example.com".to_string(),
                ..sample_listener()
            }],
            ..AppConfig::default()
        };
        assert!(validate(&config).is_ok());
    }

    /// AB#1073 port-conflict (Medium runtime guard): two enabled listeners on the same
    /// non-zero port are rejected; the message keeps the `port` prefix.
    #[test]
    fn validate_rejects_port_conflict() {
        let config = AppConfig {
            // Override `listeners` so ONLY these two compete (no seeded default local-api listener).
            // Both bind loopback so the AB#1225 bindHost gate passes and the PORT conflict is what
            // surfaces.
            listeners: vec![
                Listener {
                    id: "a".to_string(),
                    port: 9000,
                    enabled: true,
                    bind_host: "127.0.0.1".to_string(),
                    ..Listener::default()
                },
                Listener {
                    id: "b".to_string(),
                    port: 9000,
                    enabled: true,
                    bind_host: "127.0.0.1".to_string(),
                    ..Listener::default()
                },
            ],
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
            actions: vec![RuleActionKind::Review, RuleActionKind::Review],
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
            actions: vec![RuleActionKind::Notify],
            ..RuleConfig::default()
        });
        assert!(validate(&config)
            .unwrap_err()
            .message
            .starts_with("rules[0].projectId"));
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

    /// AB#1073 HTTPS-only publicUrl (Medium runtime guard): a plaintext `http://` public URL on
    /// a TUNNEL is rejected too (the guard chains listeners AND tunnels); the message keeps the
    /// `publicUrl` field-token prefix.
    #[test]
    fn validate_rejects_tunnel_http_public_url() {
        let config = AppConfig {
            // No listeners (drop the seeded default) so ONLY the tunnel HTTPS path can fire.
            listeners: Vec::new(),
            tunnels: vec![Tunnel {
                id: "t1".to_string(),
                public_url: "http://example.com".to_string(),
                ..Tunnel::default()
            }],
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
            // Override `listeners` so ONLY this one (with the userinfo URL) is checked.
            listeners: vec![Listener {
                port: 9100,
                public_url: "https://user:pass@example.com".to_string(),
                ..sample_listener()
            }],
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("publicUrl"), "{err}");
    }

    /// AB#1073 port-conflict (Medium runtime guard): an enabled listener colliding with the
    /// (enabled) webhook receiver port is rejected; the message keeps the `port` prefix. The
    /// webhook block must pass first, so the secret is long enough and the mode is the default
    /// `quick` (no tunnel command required) to reach the port-conflict check.
    #[test]
    fn validate_rejects_webhook_listener_port_conflict() {
        let config = AppConfig {
            webhook_enabled: true,
            webhook_port: 9000,
            webhook_secret: "webhook-secret-0123456789".to_string(),
            // Override `listeners` so the only collision is webhook ↔ this listener (no seeded
            // default). Loopback bind_host so the AB#1225 bindHost gate passes and the webhook↔
            // listener PORT conflict is what surfaces.
            listeners: vec![Listener {
                id: "l1".to_string(),
                port: 9000,
                enabled: true,
                bind_host: "127.0.0.1".to_string(),
                ..Listener::default()
            }],
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("port"), "{err}");
    }

    /// AB#1225 port-conflict (Medium runtime guard): the local REST API is now itself a
    /// `kind = local-api` listener (not a standalone field), so an enabled local-api listener
    /// colliding with another enabled listener on the same port is rejected through the SAME
    /// enabled-listeners loop; the message keeps the `port` prefix.
    #[test]
    fn validate_rejects_local_api_listener_port_conflict() {
        let config = AppConfig {
            listeners: vec![
                Listener {
                    port: 9000,
                    enabled: true,
                    ..default_local_api_listener()
                },
                Listener {
                    id: "other".to_string(),
                    port: 9000,
                    enabled: true,
                    // Loopback bind_host so the AB#1225 bindHost gate passes and the PORT conflict
                    // with the local-api listener is what surfaces.
                    bind_host: "127.0.0.1".to_string(),
                    ..Listener::default()
                },
            ],
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("port"), "{err}");
    }

    /// AB#1073 port-conflict (Medium runtime guard): a DISABLED listener is excluded from the
    /// port-conflict check — only enabled listeners claim a port, so a disabled one sharing the
    /// enabled local-api listener's port is fine.
    #[test]
    fn validate_disabled_listener_excluded_from_port_conflict() {
        let config = AppConfig {
            listeners: vec![
                Listener {
                    port: 9000,
                    enabled: true,
                    ..default_local_api_listener()
                },
                Listener {
                    id: "l1".to_string(),
                    port: 9000,
                    enabled: false,
                    ..Listener::default()
                },
            ],
            ..AppConfig::default()
        };
        assert!(validate(&config).is_ok());
    }

    /// AB#1064 port-0 reject (Medium runtime guard): an ENABLED listener with `port == 0`
    /// (the unset/won't-bind sentinel) is unbindable, so it is rejected; the message keeps the
    /// `port` prefix.
    #[test]
    fn validate_rejects_enabled_listener_zero_port() {
        let config = AppConfig {
            // Override `listeners` (drop the seeded default) so ONLY the port-0 check can fire.
            listeners: vec![Listener {
                id: "l1".to_string(),
                port: 0,
                enabled: true,
                ..Listener::default()
            }],
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("port"), "{err}");
    }

    /// `is_loopback_host` whitelist (AB#1225, narrowed F2). The save-time bindHost gate and the
    /// runtime supervisor's fail-closed bind gate share THIS predicate, so its behavior is the
    /// contract: accept ONLY the literal `127.0.0.1` the supervisor actually binds (incl. surrounding
    /// whitespace, which is trimmed). `localhost` / `::1` / `[::1]` are REJECTED — the runtime always
    /// binds `127.0.0.1`, so accepting them would let a config value pass while the runtime bound a
    /// different address (config/runtime mismatch); they are not yet a supported bindHost value. Also
    /// reject `0.0.0.0` / a LAN IP / a hostname / empty / a look-alike (`127.0.0.1.evil.com`).
    #[test]
    fn is_loopback_host_whitelists_only_loopback() {
        for ok in ["127.0.0.1", "  127.0.0.1  ", "\t127.0.0.1\n"] {
            assert!(is_loopback_host(ok), "expected {ok:?} to be loopback");
        }
        for bad in [
            // F2: localhost / ::1 / [::1] are no longer accepted — the runtime binds 127.0.0.1 only.
            "localhost",
            "::1",
            "[::1]",
            "0.0.0.0",
            "192.168.1.10",
            "10.0.0.1",
            "example.com",
            "",
            "   ",
            "127.0.0.1.evil.com",
            "::",
        ] {
            assert!(!is_loopback_host(bad), "expected {bad:?} to be rejected");
        }
    }

    /// AB#1225 bindHost gate (Medium/P2 security): an ENABLED listener that binds a non-loopback
    /// host is rejected at save time — remote exposure is deferred to AB#1073 — with the message
    /// starting at the `bindHost` field token so SettingsView's `errorToStep` routes it. A DISABLED
    /// non-loopback listener is exempt (it never binds), and an enabled loopback listener validates.
    #[test]
    fn validate_rejects_enabled_non_loopback_bind_host() {
        // (a) Enabled listener bound to 0.0.0.0 → rejected, message starts with `bindHost`.
        let config = AppConfig {
            // Drop the seeded default local-api listener so ONLY this one is checked.
            listeners: vec![Listener {
                id: "l1".to_string(),
                port: 9000,
                enabled: true,
                bind_host: "0.0.0.0".to_string(),
                ..Listener::default()
            }],
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("bindHost"), "{err}");
    }

    /// AB#1225 bindHost gate — the accept case: an enabled listener bound to loopback validates.
    #[test]
    fn validate_accepts_enabled_loopback_bind_host() {
        let config = AppConfig {
            listeners: vec![Listener {
                id: "l1".to_string(),
                port: 9000,
                enabled: true,
                bind_host: "127.0.0.1".to_string(),
                ..Listener::default()
            }],
            ..AppConfig::default()
        };
        assert!(validate(&config).is_ok());
    }

    /// AB#1225 bindHost gate — the exemption: a DISABLED listener with a non-loopback bind_host is
    /// NOT rejected (it never binds), mirroring the disabled port-0 / port-conflict exemptions.
    #[test]
    fn validate_exempts_disabled_non_loopback_bind_host() {
        let config = AppConfig {
            listeners: vec![Listener {
                id: "l1".to_string(),
                port: 9000,
                enabled: false,
                bind_host: "0.0.0.0".to_string(),
                ..Listener::default()
            }],
            ..AppConfig::default()
        };
        assert!(validate(&config).is_ok());
    }

    /// AB#1225 F3 — duplicate listener id is rejected (the id is a HashMap-by-id lookup key the
    /// supervisor/tunnel resolution fold by, so a duplicate would silently collapse two entries).
    /// Two distinct loopback ports so the PORT check passes and the id-uniqueness reject is what
    /// surfaces; the message keeps the `listenerId` prefix.
    #[test]
    fn validate_rejects_duplicate_listener_id() {
        let config = AppConfig {
            listeners: vec![
                Listener {
                    id: "dup".to_string(),
                    port: 9000,
                    enabled: true,
                    bind_host: "127.0.0.1".to_string(),
                    ..Listener::default()
                },
                Listener {
                    id: "dup".to_string(),
                    port: 9001,
                    enabled: true,
                    bind_host: "127.0.0.1".to_string(),
                    ..Listener::default()
                },
            ],
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("listenerId"), "{err}");
    }

    /// AB#1225 F3 — an empty listener id is rejected (it can't be addressed by a tunnel target, and
    /// the seeded local-api id is non-empty). The message keeps the `listenerId` prefix.
    #[test]
    fn validate_rejects_empty_listener_id() {
        let config = AppConfig {
            listeners: vec![Listener {
                id: String::new(),
                port: 9000,
                enabled: true,
                bind_host: "127.0.0.1".to_string(),
                ..Listener::default()
            }],
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("listenerId"), "{err}");
    }

    /// AB#1225 F3 — duplicate tunnel id is rejected (tunnel ids are also a lookup key space). One
    /// enabled listener target so the tunnels are otherwise coherent and the id-uniqueness reject is
    /// what surfaces; the message keeps the `tunnelId` prefix.
    #[test]
    fn validate_rejects_duplicate_tunnel_id() {
        let config = AppConfig {
            listeners: vec![Listener {
                id: "l1".to_string(),
                port: 9000,
                enabled: true,
                bind_host: "127.0.0.1".to_string(),
                ..Listener::default()
            }],
            tunnels: vec![
                Tunnel {
                    id: "dup".to_string(),
                    enabled: true,
                    target_listener_id: "l1".to_string(),
                    ..Tunnel::default()
                },
                Tunnel {
                    id: "dup".to_string(),
                    enabled: true,
                    target_listener_id: "l1".to_string(),
                    ..Tunnel::default()
                },
            ],
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("tunnelId"), "{err}");
    }

    /// AB#1225 F3 — an empty tunnel id is rejected. The message keeps the `tunnelId` prefix.
    #[test]
    fn validate_rejects_empty_tunnel_id() {
        let config = AppConfig {
            // No listeners (drop the seeded default) so the empty-tunnel-id check is what fires.
            listeners: Vec::new(),
            tunnels: vec![Tunnel {
                id: String::new(),
                enabled: false,
                ..Tunnel::default()
            }],
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("tunnelId"), "{err}");
    }

    /// AB#1064 reference integrity (Medium runtime guard): an ENABLED tunnel with an empty
    /// `target_listener_id` is rejected (an enabled tunnel must name its target); the message
    /// keeps the `targetListenerId` prefix.
    #[test]
    fn validate_rejects_enabled_tunnel_empty_target() {
        let config = AppConfig {
            // No listeners (drop the seeded default) so ONLY the tunnel reference check fires.
            listeners: Vec::new(),
            tunnels: vec![Tunnel {
                id: "t1".to_string(),
                enabled: true,
                target_listener_id: String::new(),
                ..Tunnel::default()
            }],
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("targetListenerId"), "{err}");
    }

    /// AB#1064 reference integrity (Medium runtime guard): an ENABLED tunnel pointing at a
    /// `target_listener_id` that matches no listener is rejected; the message keeps the
    /// `targetListenerId` prefix.
    #[test]
    fn validate_rejects_enabled_tunnel_dangling_target() {
        let config = AppConfig {
            // No listeners (drop the seeded default) so the target "nope" matches nothing.
            listeners: Vec::new(),
            tunnels: vec![Tunnel {
                id: "t1".to_string(),
                enabled: true,
                target_listener_id: "nope".to_string(),
                ..Tunnel::default()
            }],
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("targetListenerId"), "{err}");
    }

    /// AB#1064 reference integrity (Medium runtime guard): an ENABLED tunnel pointing at a
    /// DISABLED listener is rejected (the tunnel could never carry traffic); the message keeps
    /// the `targetListenerId` prefix.
    #[test]
    fn validate_rejects_enabled_tunnel_disabled_target() {
        let config = AppConfig {
            listeners: vec![Listener {
                id: "l1".to_string(),
                port: 9000,
                enabled: false,
                ..Listener::default()
            }],
            tunnels: vec![Tunnel {
                id: "t1".to_string(),
                enabled: true,
                target_listener_id: "l1".to_string(),
                ..Tunnel::default()
            }],
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("targetListenerId"), "{err}");
    }

    /// AB#1064 reference integrity (Medium runtime guard): an ENABLED tunnel pointing at an
    /// existing ENABLED listener validates. The listener gets a non-zero port (so the port-0
    /// reject does not fire) distinct from any other claim; overriding `listeners` drops the
    /// seeded default so this lone listener is the only claim.
    #[test]
    fn validate_accepts_enabled_tunnel_valid_target() {
        let config = AppConfig {
            // Loopback bind_host so the AB#1225 bindHost gate passes (an enabled listener must bind
            // loopback) and the whole config validates.
            listeners: vec![Listener {
                id: "l1".to_string(),
                kind: ListenerKind::Terminal,
                port: 9000,
                enabled: true,
                bind_host: "127.0.0.1".to_string(),
                auth: ListenerAuthMode::Bearer,
                auth_token: "terminal-token-0123456789".to_string(),
                terminal_read: true,
                ..Listener::default()
            }],
            tunnels: vec![Tunnel {
                id: "t1".to_string(),
                enabled: true,
                target_listener_id: "l1".to_string(),
                ..Tunnel::default()
            }],
            ..AppConfig::default()
        };
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn validate_rejects_enabled_tunnel_targeting_local_api() {
        let config = AppConfig {
            listeners: vec![Listener {
                id: "local-api".to_string(),
                kind: ListenerKind::LocalApi,
                port: 8788,
                enabled: true,
                bind_host: "127.0.0.1".to_string(),
                ..Listener::default()
            }],
            tunnels: vec![Tunnel {
                id: "t1".to_string(),
                enabled: true,
                target_listener_id: "local-api".to_string(),
                ..Tunnel::default()
            }],
            ..AppConfig::default()
        };
        let err = validate(&config).unwrap_err().message;
        assert!(err.starts_with("targetListenerId"), "{err}");
    }

    #[test]
    fn validate_terminal_listener_requires_bearer_token_and_read_permission() {
        let terminal = Listener {
            id: "term".to_string(),
            name: "Terminal".to_string(),
            kind: ListenerKind::Terminal,
            bind_host: "127.0.0.1".to_string(),
            port: 9100,
            enabled: true,
            auth: ListenerAuthMode::None,
            ..Listener::default()
        };

        let auth_err = validate(&AppConfig {
            listeners: vec![terminal.clone()],
            ..AppConfig::default()
        })
        .unwrap_err()
        .message;
        assert!(auth_err.starts_with("auth "), "{auth_err}");

        let token_err = validate(&AppConfig {
            listeners: vec![Listener {
                auth: ListenerAuthMode::Bearer,
                auth_token: "short".to_string(),
                ..terminal.clone()
            }],
            ..AppConfig::default()
        })
        .unwrap_err()
        .message;
        assert!(token_err.starts_with("authToken"), "{token_err}");

        let read_err = validate(&AppConfig {
            listeners: vec![Listener {
                auth: ListenerAuthMode::Bearer,
                auth_token: "terminal-token-0123456789".to_string(),
                terminal_read: false,
                ..terminal.clone()
            }],
            ..AppConfig::default()
        })
        .unwrap_err()
        .message;
        assert!(read_err.starts_with("terminalRead"), "{read_err}");

        assert!(validate(&AppConfig {
            listeners: vec![Listener {
                auth: ListenerAuthMode::Bearer,
                auth_token: "terminal-token-0123456789".to_string(),
                terminal_read: true,
                ..terminal
            }],
            ..AppConfig::default()
        })
        .is_ok());
    }

    #[test]
    fn validate_remote_web_listener_requires_bearer_token_and_public_entry() {
        let web = Listener {
            id: "web".to_string(),
            name: "Remote Web".to_string(),
            kind: ListenerKind::RemoteWeb,
            bind_host: "127.0.0.1".to_string(),
            port: 9200,
            enabled: true,
            auth: ListenerAuthMode::None,
            public_url: "https://console.example.com".to_string(),
            ..Listener::default()
        };

        let auth_err = validate(&AppConfig {
            listeners: vec![web.clone()],
            ..AppConfig::default()
        })
        .unwrap_err()
        .message;
        assert!(auth_err.starts_with("auth "), "{auth_err}");

        let token_err = validate(&AppConfig {
            listeners: vec![Listener {
                auth: ListenerAuthMode::Bearer,
                auth_token: "short".to_string(),
                ..web.clone()
            }],
            ..AppConfig::default()
        })
        .unwrap_err()
        .message;
        assert!(token_err.starts_with("authToken"), "{token_err}");

        let public_err = validate(&AppConfig {
            listeners: vec![Listener {
                auth: ListenerAuthMode::Bearer,
                auth_token: "remote-web-token-0123456789abcdef".to_string(),
                public_url: String::new(),
                ..web.clone()
            }],
            ..AppConfig::default()
        })
        .unwrap_err()
        .message;
        assert!(public_err.starts_with("publicUrl"), "{public_err}");

        let origins_err = validate(&AppConfig {
            listeners: vec![Listener {
                auth: ListenerAuthMode::Bearer,
                auth_token: "remote-web-token-0123456789abcdef".to_string(),
                allowed_origins: vec!["https://evil.example.com".to_string()],
                ..web.clone()
            }],
            ..AppConfig::default()
        })
        .unwrap_err()
        .message;
        assert!(origins_err.starts_with("allowedOrigins"), "{origins_err}");

        assert!(validate(&AppConfig {
            listeners: vec![Listener {
                auth: ListenerAuthMode::Bearer,
                auth_token: "remote-web-token-0123456789abcdef".to_string(),
                ..web
            }],
            ..AppConfig::default()
        })
        .is_ok());
    }

    #[test]
    fn validate_remote_web_listener_accepts_quick_tunnel_public_entry() {
        let web = Listener {
            id: "web".to_string(),
            name: "Remote Web".to_string(),
            kind: ListenerKind::RemoteWeb,
            bind_host: "127.0.0.1".to_string(),
            port: 9200,
            enabled: true,
            auth: ListenerAuthMode::Bearer,
            auth_token: "remote-web-token-0123456789abcdef".to_string(),
            public_url: String::new(),
            ..Listener::default()
        };
        assert!(validate(&AppConfig {
            listeners: vec![web],
            tunnels: vec![Tunnel {
                id: "web-tunnel".to_string(),
                enabled: true,
                mode: WebhookTunnelMode::Quick,
                target_listener_id: "web".to_string(),
                ..Tunnel::default()
            }],
            ..AppConfig::default()
        })
        .is_ok());
    }

    #[test]
    fn validate_enabled_command_tunnel_requires_per_tunnel_command() {
        let listener = Listener {
            id: "term".to_string(),
            kind: ListenerKind::Terminal,
            bind_host: "127.0.0.1".to_string(),
            port: 9100,
            enabled: true,
            auth: ListenerAuthMode::Bearer,
            auth_token: "terminal-token-0123456789".to_string(),
            terminal_read: true,
            ..Listener::default()
        };
        let tunnel = Tunnel {
            id: "tun".to_string(),
            name: "Terminal Tunnel".to_string(),
            enabled: true,
            mode: WebhookTunnelMode::Command,
            target_listener_id: "term".to_string(),
            command: String::new(),
            ..Tunnel::default()
        };

        let err = validate(&AppConfig {
            listeners: vec![listener.clone()],
            tunnels: vec![tunnel.clone()],
            ..AppConfig::default()
        })
        .unwrap_err()
        .message;
        assert!(err.starts_with("command"), "{err}");

        assert!(validate(&AppConfig {
            listeners: vec![listener],
            tunnels: vec![Tunnel {
                command: "cloudflared tunnel --url http://127.0.0.1:{port}".to_string(),
                ..tunnel
            }],
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
