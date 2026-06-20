//! Config slice domain model.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::model::{EngineKind, SourceKind, UpdateMode, WebhookTunnelMode};

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
    /// Monitored repo, `owner/name`.
    pub repo: String,
    /// Absolute path to the local clone codex runs the pr-review skill against.
    pub repo_root: String,
    /// Scheduled-pull period in seconds.
    pub poll_interval_secs: u64,
    /// PR author allowlist (mirrors the dispatcher's author gate).
    pub authors: Vec<String>,
    /// Label that triggers a `review` turn.
    pub review_label: String,
    /// Label that triggers a `check` turn.
    pub check_label: String,
    /// Path (relative to `repo_root`) of the codex pr-review skill to invoke.
    pub skill_rel_path: String,
    /// Per-PR cooldown between dispatches of the same `(pr, kind)`.
    pub pr_cooldown_seconds: u64,
    /// Which PR source backs the monitor. #11 reservation: today only
    /// [`SourceKind::Github`]; future variants gate GitLab/Bitbucket.
    pub source_kind: SourceKind,
    /// Which review engine runs against a PR. #11 reservation: today only
    /// [`EngineKind::Codex`]; future variant gates Claude.
    pub engine_kind: EngineKind,
    /// 是否在发现 dispatchable PR 时自动派发 review（false=仅手动「开始 review」触发）。
    pub auto_review: bool,
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
            review_label: "pr-status/needs-review-again".to_string(),
            check_label: "pr-status/needs-check-fix".to_string(),
            skill_rel_path: ".codex/skills/pr-review/SKILL.md".to_string(),
            pr_cooldown_seconds: 1800,
            source_kind: SourceKind::default(),
            engine_kind: EngineKind::default(),
            // Boot defaults to manual review: scheduler polls/emits but does NOT
            // auto-dispatch codex at startup (avoids clashing with other review
            // processes). Flip-back guarded by `default_auto_review_is_off`.
            auto_review: false,
            // #818: boot defaults to webhook-only — NO automatic CLI polling at startup
            // (the scheduler does not start a loop for this mode). Flip-back guarded by
            // `default_update_mode_is_webhook_only`.
            update_mode: UpdateMode::WebhookOnly,
            azure_org: String::new(),
            azure_project: String::new(),
        }
    }
}

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
        }
    }
}

/// Minimum `webhook_secret` length (trimmed chars) when the receiver is enabled. The
/// secret is the SOLE gate on a public HMAC-SHA256 endpoint, so a 1–2 char value is
/// brute-forceable; require a floor (GitHub recommends a long random secret).
const WEBHOOK_SECRET_MIN_LEN: usize = 16;

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
/// offending field's wire name (`repo` / `azureOrg` / `azureProject` / `repoRoot` /
/// `skillRelPath` / `skill` / `pollIntervalSecs` / `prCooldownSeconds` /
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
    }

    let repo_root = project.repo_root.trim();
    let root = Path::new(repo_root);
    if repo_root.is_empty() || !root.is_absolute() || !root.is_dir() {
        return Err(AppError::new(format!(
            "repoRoot 必须是存在的绝对目录路径: {}",
            project.repo_root
        )));
    }

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

    if project.poll_interval_secs == 0 {
        return Err(AppError::new("pollIntervalSecs 必须大于 0"));
    }
    if project.pr_cooldown_seconds == 0 {
        return Err(AppError::new("prCooldownSeconds 必须大于 0"));
    }

    if project.review_label.trim().is_empty() {
        return Err(AppError::new("reviewLabel 不能为空"));
    }
    if project.check_label.trim().is_empty() {
        return Err(AppError::new("checkLabel 不能为空"));
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
        };

        let v = serde_json::to_value(&config).expect("AppConfig serializes");

        // Multi-project keys present (camelCase).
        assert!(v.get("projects").is_some());
        assert!(v.get("activeProjectId").is_some());
        // Global webhook keys stay at the top level.
        assert!(v.get("webhookEnabled").is_some());
        assert!(v.get("webhookPort").is_some());
        assert!(v.get("webhookSecret").is_some());
        assert!(v.get("cloudflaredBin").is_some());
        assert!(v.get("webhookTunnelMode").is_some());
        assert_eq!(v["webhookTunnelMode"], "quick");
        assert!(v.get("webhookTunnelCommand").is_some());
        assert!(v.get("webhookPublicUrl").is_some());

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("active_project_id").is_none());
        assert!(v.get("webhook_enabled").is_none());
        assert!(v.get("webhook_port").is_none());
        assert!(v.get("webhook_secret").is_none());
        assert!(v.get("cloudflared_bin").is_none());
        assert!(v.get("webhook_tunnel_mode").is_none());
        assert!(v.get("webhook_tunnel_command").is_none());
        assert!(v.get("webhook_public_url").is_none());

        // The per-project fields must NOT have leaked back to the top level (they
        // moved into `Project` — a regression that re-flattened them surfaces here).
        assert!(v.get("repo").is_none());
        assert!(v.get("repoRoot").is_none());
        assert!(v.get("autoReview").is_none());
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
            review_label: "needs-review".to_string(),
            check_label: "needs-check".to_string(),
            skill_rel_path: ".codex/skills/pr-review/SKILL.md".to_string(),
            pr_cooldown_seconds: 1800,
            source_kind: SourceKind::default(),
            engine_kind: EngineKind::default(),
            auto_review: false,
            update_mode: UpdateMode::WebhookOnly,
            azure_org: "myorg".to_string(),
            azure_project: "myproject".to_string(),
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
        assert!(v.get("reviewLabel").is_some());
        assert!(v.get("checkLabel").is_some());
        assert!(v.get("skillRelPath").is_some());
        assert!(v.get("prCooldownSeconds").is_some());
        assert!(v.get("sourceKind").is_some());
        assert_eq!(v["sourceKind"], "github");
        assert!(v.get("engineKind").is_some());
        assert_eq!(v["engineKind"], "codex");
        assert!(v.get("autoReview").is_some());
        // #818: the new data-source-mode fields.
        assert!(v.get("updateMode").is_some());
        assert_eq!(v["updateMode"], "webhook-only");
        assert!(v.get("azureOrg").is_some());
        assert!(v.get("azureProject").is_some());

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("repo_root").is_none());
        assert!(v.get("poll_interval_secs").is_none());
        assert!(v.get("review_label").is_none());
        assert!(v.get("check_label").is_none());
        assert!(v.get("skill_rel_path").is_none());
        assert!(v.get("pr_cooldown_seconds").is_none());
        assert!(v.get("source_kind").is_none());
        assert!(v.get("engine_kind").is_none());
        assert!(v.get("auto_review").is_none());
        // #818: snake_case forms of the new fields absent.
        assert!(v.get("update_mode").is_none());
        assert!(v.get("azure_org").is_none());
        assert!(v.get("azure_project").is_none());
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

    /// Default-manual-review lock (Medium). A fresh project must NOT auto-dispatch
    /// codex review at boot: `Project::auto_review` defaults off, so the scheduler
    /// only polls/emits PRs and codex is left to the explicit triggers
    /// (`start_review` / `start_codex`). Locking the default here makes that intent
    /// machine-checked — a silent flip back to `true` would reintroduce the
    /// boot-time review-process clash this guards against, and fails CI first.
    #[test]
    fn default_auto_review_is_off() {
        assert!(!Project::default().auto_review);
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
    fn validate_rejects_empty_labels() {
        // Each label feeds `gh pr list --label`; a blank one makes every poll
        // match nothing / fail (PR #41 F2).
        for blank in ["", "   "] {
            assert!(validate(&with_project(Project {
                review_label: blank.to_string(),
                ..valid_project()
            }))
            .is_err());
            assert!(validate(&with_project(Project {
                check_label: blank.to_string(),
                ..valid_project()
            }))
            .is_err());
        }
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
        assert!(msg(Project {
            review_label: "  ".to_string(),
            ..base.clone()
        })
        .starts_with("reviewLabel"));
        assert!(msg(Project {
            check_label: String::new(),
            ..base
        })
        .starts_with("checkLabel"));
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
}
