//! Config slice logic.
//!
//! Backend-owned persistence in the unified SQLite store (#70): the whole [`AppConfig`]
//! lives as one camelCase-JSON blob in the single-row `config_blob` table (swapping only
//! the storage backend — the [`migrate_value`] / `validate` shape logic is unchanged).
//! The frontend calls the `get_config` / `set_config` commands (not SQLite directly), so
//! all reads/writes funnel through here.

use rusqlite::OptionalExtension;
use serde_json::{json, Map, Value};
use tauri::Manager;

use super::model::AppConfig;
use crate::db::{map_err, Database};
use crate::error::{AppError, AppResult};
use crate::model::NotificationDeliveryChannel;

/// Re-export the project domain type THROUGH the config public service surface (#35,
/// F9). The `pr` slice (scheduler / commands) depends on `Project` via
/// `config::service::Project`, not `config::model::Project` — so its cross-slice
/// coupling is to the service (the slice's public API), keeping the model an internal
/// detail the service mediates. The functions below (`project` / `project_validated`)
/// use `Project` through this same re-export.
pub use super::model::{
    NotificationChannel, NotificationSettings, Project, RuleActionKind, RuleConfig,
};

/// The DEFAULT outbox worker policy, exposed THROUGH the config public service surface (AB#1182
/// F1) — the seam the `outbox` worker uses as its config-read-failure FALLBACK. The worker reads
/// the LIVE policy via [`load`] each cycle; when THAT read fails it degrades to this default (and
/// logs), so the outbox slice depends only on `config::service`, never reaching into
/// `config::model` for the constant (the boundary the `slice_boundary_test` now machine-enforces
/// to `config::service` only). Returns [`OutboxConfig::default`] — single-sourced with
/// `DEFAULT_NOTIFICATION_TTL_SECS`, so a future outbox-policy field is picked up here for free.
pub fn default_outbox_config() -> super::model::OutboxConfig {
    super::model::OutboxConfig::default()
}

/// Labels a project's PR source should watch because a rule mentions them.
///
/// Disabled rules are included on purpose: legacy `autoReview=false` migrates to disabled
/// review/check rules, and those labels must still keep the old "list matching PRs, enqueue no
/// action" behavior. Rule execution itself still checks `enabled`, so disabled rules never create
/// outbox actions.
pub fn rule_interest_labels(rules: &[RuleConfig], project_id: &str) -> Vec<String> {
    let mut labels = Vec::new();
    for rule in rules {
        if !rule.project_id.is_empty() && rule.project_id != project_id {
            continue;
        }
        for label in rule.labels_any.iter().chain(rule.labels_all.iter()) {
            let label = label.trim();
            if !label.is_empty() && !labels.iter().any(|existing| existing == label) {
                labels.push(label.to_string());
            }
        }
    }
    labels
}

pub fn default_notification_settings() -> NotificationSettings {
    NotificationSettings::default()
}

pub fn notification_delivery_channel(channel: &NotificationChannel) -> NotificationDeliveryChannel {
    NotificationDeliveryChannel {
        id: channel.id.clone(),
        name: channel.name.clone(),
        kind: channel.kind,
        webhook_url: channel.webhook_url.clone(),
        webhook_secret: channel.webhook_secret.clone(),
        telegram_bot_token: channel.telegram_bot_token.clone(),
        telegram_chat_id: channel.telegram_chat_id.clone(),
        smtp_host: channel.smtp_host.clone(),
        smtp_port: channel.smtp_port,
        smtp_username: channel.smtp_username.clone(),
        smtp_password: channel.smtp_password.clone(),
        smtp_from: channel.smtp_from.clone(),
        smtp_to: channel.smtp_to.clone(),
        timeout_secs: channel.timeout_secs,
    }
}

pub fn validate_notification_channel_for_test(channel: &NotificationChannel) -> AppResult<()> {
    let mut test_channel = channel.clone();
    test_channel.enabled = true;
    super::model::validate_notification_channel(&test_channel)
}

/// `id`/`name` assigned to the single project lifted out of a legacy flat config by
/// [`migrate_value`] (#35). One source so the migration and its tests agree on the
/// id the active-project pointer (`activeProjectId`) is also set to.
const MIGRATED_PROJECT_ID: &str = "default";

/// The per-project keys lifted out of the legacy flat single-project config
/// into the migrated [`Project`] object (#35). camelCase wire names (the persisted
/// shape — `save` writes `serde_json::to_value(&AppConfig)`, which is camelCase).
const PROJECT_KEYS: &[&str] = &[
    "repo",
    "repoRoot",
    "pollIntervalSecs",
    "authors",
    // Legacy trigger fields are lifted only so `seed_rule_configs` can convert them
    // into rules. They are stripped from the migrated project JSON before deserialization.
    "reviewLabel",
    "checkLabel",
    "skillRelPath",
    "prCooldownSeconds",
    "sourceKind",
    "engineKind",
    "autoReview",
];

/// The 7 GLOBAL webhook/shell keys that stay at the top level of the migrated
/// [`AppConfig`] (#35: one webhook receiver serves every project).
const WEBHOOK_KEYS: &[&str] = &[
    "webhookEnabled",
    "webhookPort",
    "webhookSecret",
    "cloudflaredBin",
    "webhookTunnelMode",
    "webhookTunnelCommand",
    "webhookPublicUrl",
];

/// Migrates a raw persisted config value to the #35 multi-project shape.
///
/// Pure (no IO) so it is table-testable; the only caller is [`load`], which runs it
/// on the raw stored value before `serde_json::from_value::<AppConfig>`. The result
/// is always fed through `from_value` (which is lenient via `#[serde(default)]`), so
/// this only needs to produce the right *shape* — missing keys are filled by
/// `Default` afterward.
///
/// Detect-by-key (idempotent): a value that already has a `projects` key is the new
/// shape → returned unchanged, so a second pass (or a `save`d config reloaded) is a
/// no-op. Otherwise the legacy flat single-project shape is upgraded:
/// - the 11 [`PROJECT_KEYS`] (whichever exist) are lifted into one project object
///   tagged `id`/`name` = `"default"`, `enabled` = `true`;
/// - the 7 [`WEBHOOK_KEYS`] (whichever exist) stay at the top level;
/// - `projects` = `[thatProject]`, `activeProjectId` = `"default"`.
///
/// An empty object `{}` (and any non-object) is treated as FIRST LAUNCH — it produces
/// `{ "projects": [], "activeProjectId": "" }` so onboarding triggers, rather than a
/// migrated default project. (A `{}` has no flat keys to lift; materializing a
/// gocell-default project would skip onboarding.)
///
/// Two passes: [`normalize_multiproject`] first (legacy-flat → multi-project shape), then
/// [`migrate_remote_access`] (#1553) so legacy `listeners[]` / `tunnels[]` / `localApiPort`
/// are lifted into `remoteAccess.entrypoints[]` / `remoteAccess.tunnels[]`.
fn migrate_value(raw: Value) -> Value {
    seed_rule_configs(migrate_remote_access(normalize_multiproject(raw)))
}

/// First migration pass: normalize the raw persisted value to the #35 multi-project shape.
/// (Unchanged from the historical `migrate_value` body — see the module doc above.)
fn normalize_multiproject(raw: Value) -> Value {
    let Value::Object(old) = raw else {
        // Non-object (null / array / scalar): treat as first launch.
        return json!({ "projects": [], "activeProjectId": "" });
    };

    // Already new shape → identity (idempotent).
    if old.contains_key("projects") {
        return Value::Object(old);
    }

    // Empty object → first launch (no project, so onboarding triggers).
    if old.is_empty() {
        return json!({ "projects": [], "activeProjectId": "" });
    }

    // Legacy flat single-project shape: lift the per-project keys into one project.
    let mut project = Map::new();
    project.insert("id".to_string(), json!(MIGRATED_PROJECT_ID));
    project.insert("name".to_string(), json!(MIGRATED_PROJECT_ID));
    project.insert("enabled".to_string(), json!(true));
    for key in PROJECT_KEYS {
        if let Some(v) = old.get(*key) {
            project.insert((*key).to_string(), v.clone());
        }
    }

    let mut new = Map::new();
    new.insert("projects".to_string(), json!([Value::Object(project)]));
    new.insert("activeProjectId".to_string(), json!(MIGRATED_PROJECT_ID));
    for key in WEBHOOK_KEYS {
        if let Some(v) = old.get(*key) {
            new.insert((*key).to_string(), v.clone());
        }
    }

    Value::Object(new)
}

/// #1553 remote-access migration: old `listeners[]` become entrypoints with a single capability
/// route; old `tunnels[]` retarget those entrypoints. This is idempotent: a config that already
/// carries `remoteAccess` keeps it, with only a missing legacy `localApiPort` local-api entrypoint
/// seeded for users upgrading from the transitional top-level field.
fn migrate_remote_access(value: Value) -> Value {
    let Value::Object(mut obj) = value else {
        return value;
    };

    let mut remote = obj
        .get("remoteAccess")
        .cloned()
        .unwrap_or_else(|| json!({ "entrypoints": [], "tunnels": [] }));
    if !remote.is_object() {
        remote = json!({ "entrypoints": [], "tunnels": [] });
    }

    let had_remote_access = obj.contains_key("remoteAccess");
    let has_legacy_remote_access = obj.contains_key("listeners")
        || obj.contains_key("tunnels")
        || obj.contains_key("localApiPort");
    if !had_remote_access && !has_legacy_remote_access {
        return Value::Object(obj);
    }
    if !had_remote_access {
        let entrypoints = obj
            .get("listeners")
            .and_then(Value::as_array)
            .map(|listeners| {
                listeners
                    .iter()
                    .filter_map(legacy_listener_to_entrypoint)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some(remote_obj) = remote.as_object_mut() {
            remote_obj.insert("entrypoints".to_string(), Value::Array(entrypoints));
            let tunnels = obj
                .get("tunnels")
                .and_then(Value::as_array)
                .map(|tunnels| {
                    tunnels
                        .iter()
                        .filter_map(legacy_tunnel_to_remote_tunnel)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            remote_obj.insert("tunnels".to_string(), Value::Array(tunnels));
        }
    }

    if let Some(port) = obj.get("localApiPort").and_then(Value::as_u64) {
        if port <= u16::MAX as u64 && !remote_access_has_local_api(&remote) {
            push_remote_entrypoint(
                &mut remote,
                local_api_entrypoint_json(port as u16, port != 0),
            );
        } else if port > u16::MAX as u64 {
            eprintln!(
                "[config] 忽略越界的 legacy localApiPort={port}（> {}）：未播种 local-api 入口，请在设置中重设端口",
                u16::MAX
            );
        }
    }

    obj.insert("remoteAccess".to_string(), remote);
    obj.remove("listeners");
    obj.remove("tunnels");
    Value::Object(obj)
}

fn remote_access_has_local_api(remote: &Value) -> bool {
    remote
        .get("entrypoints")
        .and_then(Value::as_array)
        .is_some_and(|entrypoints| {
            entrypoints.iter().any(|entrypoint| {
                entrypoint
                    .get("routes")
                    .and_then(Value::as_array)
                    .is_some_and(|routes| {
                        routes
                            .iter()
                            .any(|route| route.get("capability") == Some(&json!("local-api")))
                    })
            })
        })
}

fn push_remote_entrypoint(remote: &mut Value, entrypoint: Value) {
    if let Some(arr) = remote.get_mut("entrypoints").and_then(Value::as_array_mut) {
        arr.push(entrypoint);
    } else if let Some(obj) = remote.as_object_mut() {
        obj.insert("entrypoints".to_string(), json!([entrypoint]));
    }
}

fn local_api_entrypoint_json(port: u16, enabled: bool) -> Value {
    json!({
        "id": "local-api",
        "name": "Local API",
        "bindHost": "127.0.0.1",
        "port": port,
        "enabled": enabled,
        "sourcePolicy": { "mode": "loopback", "allow": [] },
        "allowedOrigins": [],
        "trustedProxies": [],
        "routes": [{
            "id": "local-api",
            "name": "Local API",
            "path": "/api",
            "capability": "local-api",
            "enabled": true,
            "authToken": "",
            "terminalRead": false,
            "terminalWrite": false,
            "terminalCreate": false,
            "terminalAdmin": false
        }]
    })
}

fn legacy_listener_to_entrypoint(listener: &Value) -> Option<Value> {
    let kind = listener.get("kind").and_then(Value::as_str)?;
    match kind {
        "local-api" => Some(local_api_entrypoint_json(
            listener
                .get("port")
                .and_then(Value::as_u64)
                .filter(|p| *p <= u16::MAX as u64)
                .unwrap_or(8788) as u16,
            listener
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        )),
        "terminal" => {
            let id = listener
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("terminal");
            Some(json!({
                "id": id,
                "name": listener.get("name").and_then(Value::as_str).unwrap_or("Terminal"),
                "bindHost": listener.get("bindHost").and_then(Value::as_str).unwrap_or("127.0.0.1"),
                "port": listener.get("port").and_then(Value::as_u64).unwrap_or(0),
                "enabled": listener.get("enabled").and_then(Value::as_bool).unwrap_or(false),
                "sourcePolicy": { "mode": "loopback", "allow": [] },
                "allowedOrigins": listener.get("allowedOrigins").cloned().unwrap_or_else(|| json!([])),
                "trustedProxies": [],
                "routes": [{
                    "id": "terminal",
                    "name": "Terminal",
                    "path": "/terminal",
                    "capability": "terminal",
                    "enabled": true,
                    "authToken": listener.get("authToken").and_then(Value::as_str).unwrap_or(""),
                    "terminalRead": listener.get("terminalRead").and_then(Value::as_bool).unwrap_or(true),
                    "terminalWrite": listener.get("terminalWrite").and_then(Value::as_bool).unwrap_or(false),
                    "terminalCreate": listener.get("terminalCreate").and_then(Value::as_bool).unwrap_or(false),
                    "terminalAdmin": listener.get("terminalAdmin").and_then(Value::as_bool).unwrap_or(false)
                }]
            }))
        }
        _ => None,
    }
}

fn legacy_tunnel_to_remote_tunnel(tunnel: &Value) -> Option<Value> {
    Some(json!({
        "id": tunnel.get("id").and_then(Value::as_str)?,
        "name": tunnel.get("name").and_then(Value::as_str).unwrap_or("Tunnel"),
        "mode": tunnel.get("mode").and_then(Value::as_str).unwrap_or("quick"),
        "targetEntrypointId": tunnel.get("targetListenerId").and_then(Value::as_str).unwrap_or(""),
        "bindHost": tunnel.get("bindHost").and_then(Value::as_str).unwrap_or("0.0.0.0"),
        "port": tunnel.get("port").and_then(Value::as_u64).unwrap_or(0),
        "command": tunnel.get("command").and_then(Value::as_str).unwrap_or(""),
        "publicUrl": tunnel.get("publicUrl").and_then(Value::as_str).unwrap_or(""),
        "enabled": tunnel.get("enabled").and_then(Value::as_bool).unwrap_or(false),
    }))
}

/// Seed the v1 rule-engine config (#1371) from legacy per-project trigger fields.
///
/// Detect-by-key: if `rules` already exists, leave it untouched. Otherwise, each project that still
/// carries old `reviewLabel` / `checkLabel` JSON fields becomes two rules. `autoReview` maps to the
/// rule `enabled` bit, preserving the old "listed but not dispatched" default as disabled rules.
/// The old fields are intentionally not copied into Rust `Project`, so after the next save the blob
/// is rule-only with no runtime compatibility path.
fn seed_rule_configs(value: Value) -> Value {
    let Value::Object(mut obj) = value else {
        return value;
    };
    if obj.contains_key("rules") {
        return Value::Object(obj);
    }

    let mut rules = Vec::new();
    if let Some(projects) = obj.get("projects").and_then(Value::as_array) {
        for project in projects {
            let Some(pid) = project.get("id").and_then(Value::as_str) else {
                continue;
            };
            let auto = project
                .get("autoReview")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let repo = project
                .get("repo")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(label) = project.get("reviewLabel").and_then(Value::as_str) {
                if !label.trim().is_empty() {
                    rules.push(json!({
                        "id": format!("{pid}-review"),
                        "name": format!("{pid} review"),
                        "enabled": auto,
                        "source": null,
                        "eventType": "pullRequest",
                        "projectId": pid,
                        "repo": repo,
                        "labelsAny": [label],
                        "labelsAll": [],
                        "titleContains": "",
                        "bodyContains": "",
                        "actions": ["review"]
                    }));
                }
            }
            if let Some(label) = project.get("checkLabel").and_then(Value::as_str) {
                if !label.trim().is_empty() {
                    rules.push(json!({
                        "id": format!("{pid}-check"),
                        "name": format!("{pid} check"),
                        "enabled": auto,
                        "source": null,
                        "eventType": "pullRequest",
                        "projectId": pid,
                        "repo": repo,
                        "labelsAny": [label],
                        "labelsAll": [],
                        "titleContains": "",
                        "bodyContains": "",
                        "actions": ["check"]
                    }));
                }
            }
        }
    }
    if let Some(projects) = obj.get_mut("projects").and_then(Value::as_array_mut) {
        for project in projects {
            if let Some(project) = project.as_object_mut() {
                project.remove("reviewLabel");
                project.remove("checkLabel");
                project.remove("autoReview");
            }
        }
    }
    obj.insert("rules".to_string(), Value::Array(rules));
    Value::Object(obj)
}

/// The port the local-api listener is bound on (AB#1225 single source of truth): the first
/// ENABLED `kind = local-api` listener's port, else 0 (off). Consumed by the CLI client
/// (`cli.rs`) which connects to `127.0.0.1:<port>`. (A non-loopback local-api is refused at
/// runtime by the supervisor; this helper still returns its port — the CLI is loopback-only.)
pub fn local_api_port(cfg: &AppConfig) -> u16 {
    cfg.remote_access
        .entrypoints
        .iter()
        .find(|entrypoint| {
            entrypoint.enabled
                && entrypoint.routes.iter().any(|route| {
                    route.enabled
                        && route.capability == crate::config::model::RemoteCapability::LocalApi
                })
        })
        .map(|entrypoint| entrypoint.port)
        .unwrap_or(0)
}

pub fn notification_channel<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    channel_id: &str,
) -> AppResult<NotificationChannel> {
    let cfg = load(app)?;
    cfg.notifications
        .channels
        .into_iter()
        .find(|c| c.id == channel_id)
        .ok_or_else(|| AppError::new(format!("notificationChannelId 不存在: {channel_id}")))
}

pub fn enabled_notification_channels<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> AppResult<Vec<NotificationChannel>> {
    Ok(load(app)?
        .notifications
        .channels
        .into_iter()
        .filter(|c| c.enabled)
        .collect())
}

/// Loads the persisted configuration, falling back to [`AppConfig::default`]
/// when nothing has been stored yet. The raw stored value is run through
/// [`migrate_value`] first so a legacy flat single-project config (#35) upgrades to
/// the multi-project shape before deserialization.
pub fn load<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> AppResult<AppConfig> {
    load_db(app.state::<Database>().inner())
}

/// SQLite-level load (no Tauri app) — reads the `config_blob` row, runs [`migrate_value`]
/// on it, then deserializes. Split from [`load`] so the blob path + legacy migration are
/// testable against an in-memory [`Database`].
pub(crate) fn load_db(db: &Database) -> AppResult<AppConfig> {
    let raw: Option<String> = db.with_conn(|conn| {
        conn.query_row("SELECT json FROM config_blob WHERE id = 1", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()
    })?;

    match raw {
        None => Ok(AppConfig::default()),
        // Surface (don't silently discard) a corrupt/incompatible persisted config so the
        // user can fix it rather than lose their settings.
        Some(json) => {
            let value: Value = serde_json::from_str(&json)
                .map_err(|e| AppError::new(format!("解析持久化配置失败: {e}")))?;
            serde_json::from_value(migrate_value(value))
                .map_err(|e| AppError::new(format!("解析持久化配置失败: {e}")))
        }
    }
}

/// Looks up a [`Project`] by `id` in the persisted config. Callers in other slices
/// (scheduler / review dispatch / webhook routing) resolve the project they act on
/// through here, so the config slice stays the single owner of project lookup.
pub fn project<R: tauri::Runtime>(app: &tauri::AppHandle<R>, id: &str) -> AppResult<Project> {
    let config = load(app)?;
    config
        .projects
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| AppError::new(format!("找不到项目: {id}")))
}

/// Looks up a [`Project`] by `id` and validates its filesystem-dependent fields
/// before returning it. The per-project analogue of [`load_validated`]: the review
/// slice calls this right before attaching the project's skill path to a codex turn
/// so a broken/escaped skill path is rejected at dispatch time (the same guarantee
/// the old single-project `load_validated` gave). Validation stays inside the config
/// slice (via [`crate::config::model::validate_project`]) so review depends only on
/// `config::service`, never `config::model`.
pub fn project_validated<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    id: &str,
) -> AppResult<Project> {
    let p = project(app, id)?;
    crate::config::model::validate_project(&p)?;
    Ok(p)
}

/// Resolves a project from a free-form `reference` (a project `id` OR a `repo`), then
/// validates its filesystem-dependent fields — the [`project_validated`] analogue for the
/// trigger funnel (AB#1042), where a third-party caller (CLI/deeplink, future) names a
/// project by id or repo rather than its internal id. Matching is delegated to the pure
/// [`match_project_ref`] (unit-tested without an app); validation stays inside the config
/// slice so the review slice depends only on `config::service`, never `config::model`.
pub fn project_by_ref_validated<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    reference: &str,
) -> AppResult<Project> {
    let config = load(app)?;
    let p = match_project_ref(&config.projects, reference)?.clone();
    crate::config::model::validate_project(&p)?;
    Ok(p)
}

/// Pure project resolver for a `reference` that is either a project `id` or a `repo`
/// (AB#1042). Returns a borrow of the matched project, or an [`AppError`] when the
/// reference is ambiguous / unknown:
/// - **id first** (exact, unique by [`crate::config::model::validate`]): an `id` hit
///   returns immediately, so a repo that happens to equal some project's id can't shadow it.
/// - else **repo** (case-insensitive, mirroring the webhook router's `eq_ignore_ascii_case`
///   and config's case-insensitive duplicate-repo rejection): 0 matches → "找不到"; **>1
///   matches → reject** "repo 不唯一，请用 projectId" (fail-closed — never silently trigger
///   the wrong project); exactly 1 → that project.
///
/// `config::validate` already forbids duplicate repos (case-insensitive), so >1 is
/// theoretically unreachable; rejecting it explicitly is a fail-closed backstop rather than
/// resting on that "config can't duplicate" soft assumption. Pure (no IO) so the
/// id-hit / repo-ci-hit / miss / ambiguity cases are unit-tested directly.
fn match_project_ref<'a>(projects: &'a [Project], reference: &str) -> AppResult<&'a Project> {
    if let Some(p) = projects.iter().find(|p| p.id == reference) {
        return Ok(p);
    }
    let mut by_repo = projects
        .iter()
        .filter(|p| p.repo.eq_ignore_ascii_case(reference));
    match (by_repo.next(), by_repo.next()) {
        (None, _) => Err(AppError::new(format!(
            "找不到项目（reference 既非 id 也非已知 repo）: {reference}"
        ))),
        (Some(p), None) => Ok(p),
        (Some(_), Some(_)) => Err(AppError::new(format!(
            "repo 不唯一，请用 projectId: {reference}"
        ))),
    }
}

/// Returns the `repo_root` of the active project, or an empty string when there is
/// no active project (first launch, or `active_project_id` matches nothing).
///
/// Degrades gracefully (returns `Ok("")` rather than an error) so the GLOBAL codex
/// status probe — which only needs *a* repo root to spawn its app-server check — can
/// treat "no active project yet" as "unavailable" instead of surfacing an error. It
/// also does NOT validate the path (mirroring `load`'s leniency for read-only
/// consumers); callers that will actually *use* the path go through
/// [`project_validated`] instead.
pub fn active_repo_root<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> AppResult<String> {
    let config = load(app)?;
    Ok(config
        .projects
        .iter()
        .find(|p| p.id == config.active_project_id)
        .map(|p| p.repo_root.clone())
        .unwrap_or_default())
}

/// Loads the persisted config and validates its filesystem-dependent fields, for
/// callers that will *use* those paths (e.g. the review slice attaching the skill
/// path to a codex turn). Keeps validation inside the config slice so callers
/// depend only on `config::service`, never `config::model` — `load` stays lenient
/// (no validation) for read-only consumers like `get_codex_status`.
pub fn load_validated<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> AppResult<AppConfig> {
    let config = load(app)?;
    super::model::validate(&config)?;
    Ok(config)
}

/// Persists the configuration after validating filesystem-dependent fields.
pub fn save<R: tauri::Runtime>(app: &tauri::AppHandle<R>, config: AppConfig) -> AppResult<()> {
    super::model::validate(&config)?;
    persist(app, &config)
}

/// Writes `config` to the store (no validation). Private — the validating [`save`]
/// and the lenient [`set_active_project`] both funnel through here so the
/// store-write plumbing lives in one place.
fn persist<R: tauri::Runtime>(app: &tauri::AppHandle<R>, config: &AppConfig) -> AppResult<()> {
    persist_db(app.state::<Database>().inner(), config)
}

/// SQLite-level write of the whole [`AppConfig`] as the single `config_blob` row (#70).
/// Split from [`persist`] so the blob round-trip is testable against an in-memory
/// [`Database`]. Stores the camelCase JSON; [`load_db`] runs `migrate_value` on read
/// (identity for an already-new shape).
pub(crate) fn persist_db(db: &Database, config: &AppConfig) -> AppResult<()> {
    let json = serde_json::to_string(config).map_err(|e| AppError::new(e.to_string()))?;
    db.with_conn(|conn| {
        conn.execute(
            "INSERT OR REPLACE INTO config_blob (id, json) VALUES (1, ?1)",
            [&json],
        )
        .map(|_| ())
    })
}

/// One-time legacy import (#70) of the old `config.json` `appConfig` value into the
/// `config_blob` row. Stores the RAW legacy value as-is (which may be the pre-#35 flat
/// shape) — [`load_db`]'s [`migrate_value`] lifts it on the next read, so this needs no
/// shape knowledge. Runs inside the composition root's import transaction.
pub fn import_legacy_config(tx: &rusqlite::Transaction, value: &Value) -> AppResult<()> {
    let json = serde_json::to_string(value).map_err(|e| AppError::new(e.to_string()))?;
    tx.execute(
        "INSERT OR REPLACE INTO config_blob (id, json) VALUES (1, ?1)",
        [&json],
    )
    .map_err(map_err)?;
    Ok(())
}

/// Persists `active_project_id` (#35) WITHOUT re-validating the whole config —
/// switching the viewed project is a UI navigation action, not a config edit, and
/// must not be blocked because some OTHER enabled project's filesystem field went
/// stale. Verifies the target project exists (a stale id is rejected), then writes
/// the pointer through the same store as [`save`]. The frontend `useProjects().setActive`
/// calls the `set_active_project` command, which funnels here.
///
/// **Concurrency (intentional, no lock).** This does a load→mutate→persist with NO
/// `config.json` write lock, so it is last-write-wins against a concurrent [`save`]:
/// a `set_active_project` and a `save` racing could each clobber the other's write.
/// That is acceptable because `active_project_id` is UI navigation state, not a
/// correctness-critical field — the worst case is the focused project momentarily
/// reverts and the next UI action re-sets it. (The dedup/retention stores that ARE
/// correctness-critical have their own write locks; this pointer does not warrant one.)
pub fn set_active_project<R: tauri::Runtime>(app: &tauri::AppHandle<R>, id: &str) -> AppResult<()> {
    let mut config = load(app)?;
    if !config.projects.iter().any(|p| p.id == id) {
        return Err(AppError::new(format!("找不到项目: {id}")));
    }
    config.active_project_id = id.to_string();
    persist(app, &config)
}

/// `migrate_value` correctness lock (#35). The migration is the bridge between the
/// legacy flat single-project persisted shape and the multi-project [`AppConfig`];
/// it is pure (no IO) so it can be characterized directly here. These tests pin the
/// four cases — legacy flat → single default project, already-new identity, webhook
/// lift, first-launch empty — plus idempotence; a shape regression fails CI here.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NotificationKind;

    fn local_api_entrypoints(config: &AppConfig) -> Vec<&crate::config::model::RemoteEntrypoint> {
        config
            .remote_access
            .entrypoints
            .iter()
            .filter(|entrypoint| {
                entrypoint.routes.iter().any(|route| {
                    route.capability == crate::config::model::RemoteCapability::LocalApi
                })
            })
            .collect()
    }

    // SQLite blob round-trip (#70): an empty DB loads the default; a persisted config
    // reads back equal. The blob path is the storage swap — `migrate_value`/`validate`
    // (tested below) are unchanged.
    #[test]
    fn sqlite_blob_empty_loads_default_and_round_trips() {
        let db = Database::open_in_memory().expect("open db");
        // No row yet → default (first launch, empty projects).
        let first = load_db(&db).expect("load empty");
        assert!(first.projects.is_empty());
        assert_eq!(first.active_project_id, "");

        // Persist a non-default value (a webhook port) and read it back through the blob.
        let config = AppConfig {
            webhook_port: 9123,
            ..Default::default()
        };
        persist_db(&db, &config).expect("persist");
        let back = load_db(&db).expect("load");
        assert_eq!(back.webhook_port, 9123);
    }

    // One-time legacy import (#70) of the highest-risk case: a pre-#35 FLAT `config.json`
    // value imported into `config_blob` must, on the next `load_db`, surface as the
    // migrated multi-project shape (the import stores it raw; `migrate_value` lifts it on
    // read). This is the migration existing users depend on to keep their settings.
    #[test]
    fn legacy_flat_config_import_migrates_on_load() {
        let db = Database::open_in_memory().expect("open db");
        let legacy = json!({
            "repo": "octocat/hello",
            "repoRoot": "/tmp/hello",
            "autoReview": true
        });
        db.with_tx(|tx| import_legacy_config(tx, &legacy))
            .expect("import");

        let config = load_db(&db).expect("load");
        assert_eq!(config.projects.len(), 1, "flat shape lifted to one project");
        assert_eq!(config.projects[0].repo, "octocat/hello");
        assert_eq!(config.active_project_id, MIGRATED_PROJECT_ID);
        assert_eq!(
            config.rules.len(),
            0,
            "no legacy labels means no migrated rules"
        );
    }

    #[test]
    fn notification_test_validation_checks_disabled_draft_fields() {
        let mut channel = NotificationChannel {
            id: "slack-1".to_string(),
            name: "Slack".to_string(),
            kind: NotificationKind::Slack,
            enabled: false,
            webhook_url: "https://hooks.slack.test/services/T000/B000/XXX".to_string(),
            ..NotificationChannel::default()
        };
        validate_notification_channel_for_test(&channel).expect("disabled draft can be tested");

        channel.webhook_url.clear();
        let err = validate_notification_channel_for_test(&channel).expect_err("missing URL fails");
        assert!(
            err.to_string().contains("notificationWebhookUrl 不能为空"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn migrate_old_flat_produces_single_default_project() {
        let raw = json!({
            "repo": "octocat/hello",
            "repoRoot": "/tmp/hello",
            "pollIntervalSecs": 60,
            "authors": ["octocat"],
            "reviewLabel": "needs-review",
            "checkLabel": "needs-check",
            "skillRelPath": ".codex/skills/pr-review/SKILL.md",
            "prCooldownSeconds": 900,
            "sourceKind": "github",
            "engineKind": "codex",
            "autoReview": true
        });

        let migrated = migrate_value(raw);

        // Top-level multi-project shape.
        assert_eq!(migrated["activeProjectId"], MIGRATED_PROJECT_ID);
        let projects = migrated["projects"].as_array().expect("projects array");
        assert_eq!(projects.len(), 1);

        let project = &projects[0];
        assert_eq!(project["id"], MIGRATED_PROJECT_ID);
        assert_eq!(project["name"], MIGRATED_PROJECT_ID);
        assert_eq!(project["enabled"], true);
        // The lifted per-project keys carried over verbatim; legacy trigger fields
        // move to rules instead of remaining on the project runtime shape.
        assert_eq!(project["repo"], "octocat/hello");
        assert_eq!(project["repoRoot"], "/tmp/hello");
        assert_eq!(project["pollIntervalSecs"], 60);
        assert_eq!(project["authors"], json!(["octocat"]));
        assert!(project.get("reviewLabel").is_none());
        assert!(project.get("checkLabel").is_none());
        assert_eq!(project["skillRelPath"], ".codex/skills/pr-review/SKILL.md");
        assert_eq!(project["prCooldownSeconds"], 900);
        assert_eq!(project["sourceKind"], "github");
        assert_eq!(project["engineKind"], "codex");
        assert!(project.get("autoReview").is_none());

        let rules = migrated["rules"].as_array().expect("rules array");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["id"], "default-review");
        assert_eq!(rules[0]["enabled"], true);
        assert_eq!(rules[0]["projectId"], MIGRATED_PROJECT_ID);
        assert_eq!(rules[0]["repo"], "octocat/hello");
        assert_eq!(rules[0]["labelsAny"], json!(["needs-review"]));
        assert_eq!(rules[0]["actions"], json!(["review"]));
        assert_eq!(rules[1]["id"], "default-check");
        assert_eq!(rules[1]["labelsAny"], json!(["needs-check"]));
        assert_eq!(rules[1]["actions"], json!(["check"]));

        // The migrated shape must deserialize into a real `AppConfig` (lenient path
        // `load` uses) with the lifted values intact.
        let config: AppConfig =
            serde_json::from_value(migrated).expect("migrated shape deserializes");
        assert_eq!(config.active_project_id, MIGRATED_PROJECT_ID);
        assert_eq!(config.projects.len(), 1);
        assert_eq!(config.projects[0].repo, "octocat/hello");
        assert_eq!(config.rules.len(), 2);
        assert!(config.rules.iter().all(|rule| rule.enabled));
    }

    #[test]
    fn rule_interest_labels_include_disabled_rules_for_list_only_migration() {
        let rules = vec![
            RuleConfig {
                enabled: true,
                project_id: "p1".to_string(),
                labels_any: vec!["a".to_string()],
                labels_all: vec!["b".to_string(), "a".to_string()],
                ..RuleConfig::default()
            },
            RuleConfig {
                enabled: false,
                project_id: "p1".to_string(),
                labels_any: vec!["disabled-list-only".to_string()],
                ..RuleConfig::default()
            },
            RuleConfig {
                enabled: true,
                project_id: "p2".to_string(),
                labels_any: vec!["other-project".to_string()],
                ..RuleConfig::default()
            },
        ];
        assert_eq!(
            rule_interest_labels(&rules, "p1"),
            vec![
                "a".to_string(),
                "b".to_string(),
                "disabled-list-only".to_string()
            ]
        );
    }

    #[test]
    fn migrate_auto_review_false_rules_still_feed_interest_labels() {
        let config: AppConfig = serde_json::from_value(migrate_value(json!({
            "repo": "octocat/hello",
            "reviewLabel": "needs-review",
            "checkLabel": "needs-check",
            "autoReview": false
        })))
        .expect("migrated shape deserializes");

        assert_eq!(config.rules.len(), 2);
        assert!(config.rules.iter().all(|rule| !rule.enabled));
        assert_eq!(
            rule_interest_labels(&config.rules, MIGRATED_PROJECT_ID),
            vec!["needs-review".to_string(), "needs-check".to_string()]
        );
    }

    #[test]
    fn migrate_new_shape_without_legacy_port_is_identity() {
        // A value already carrying `projects` AND no legacy `localApiPort` is the new shape →
        // returned unchanged. (Identity is NOT a general invariant for the new shape anymore: a
        // new-shape input carrying `localApiPort` gets a seeded local-api listener — see
        // `migrate_new_shape_with_legacy_port_seeds_listener`.)
        let raw = json!({
            "projects": [{ "id": "p1", "repo": "owner/name" }],
            "activeProjectId": "p1",
            "webhookEnabled": false
        });
        let expected = json!({
            "projects": [{ "id": "p1", "repo": "owner/name" }],
            "activeProjectId": "p1",
            "webhookEnabled": false,
            "rules": []
        });
        assert_eq!(migrate_value(raw), expected);
    }

    /// F10: a NEW-shape input (`projects` present) that ALSO carries a legacy top-level
    /// `localApiPort` is NOT identity — the second pass seeds a `local-api` listener from it. This
    /// is the realistic migration path (an AB#1043 config saved before AB#1225 removed the field),
    /// and the reason `migrate_new_shape_is_identity` was narrowed to the no-legacy-port case.
    #[test]
    fn migrate_new_shape_with_legacy_port_seeds_listener() {
        let raw = json!({
            "projects": [{ "id": "p1", "repo": "owner/name" }],
            "activeProjectId": "p1",
            "localApiPort": 8790
        });
        let migrated = migrate_value(raw.clone());
        // Not identity: a listener was appended.
        assert_ne!(migrated, raw);

        let config: AppConfig =
            serde_json::from_value(migrated).expect("migrated shape deserializes");
        let local_api = local_api_entrypoints(&config);
        assert_eq!(local_api.len(), 1, "exactly one local-api listener seeded");
        assert!(local_api[0].enabled);
        assert_eq!(local_api[0].port, 8790);
    }

    #[test]
    fn migrate_lifts_webhook_to_top_level() {
        let raw = json!({
            "repo": "octocat/hello",
            "repoRoot": "/tmp/hello",
            "webhookEnabled": true,
            "webhookPort": 9000,
            "webhookSecret": "super-secret-0123456789",
            "cloudflaredBin": "/usr/bin/cloudflared",
            "webhookTunnelMode": "command",
            "webhookTunnelCommand": "cloudflared tunnel run --url http://127.0.0.1:{port} t",
            "webhookPublicUrl": "https://example.com"
        });

        let migrated = migrate_value(raw);

        // Webhook keys at the TOP level (not inside the project).
        assert_eq!(migrated["webhookEnabled"], true);
        assert_eq!(migrated["webhookPort"], 9000);
        assert_eq!(migrated["webhookSecret"], "super-secret-0123456789");
        assert_eq!(migrated["cloudflaredBin"], "/usr/bin/cloudflared");
        assert_eq!(migrated["webhookTunnelMode"], "command");
        assert_eq!(
            migrated["webhookTunnelCommand"],
            "cloudflared tunnel run --url http://127.0.0.1:{port} t"
        );
        assert_eq!(migrated["webhookPublicUrl"], "https://example.com");

        // And NOT duplicated into the project object.
        let project = &migrated["projects"][0];
        assert!(project.get("webhookEnabled").is_none());
        assert!(project.get("webhookSecret").is_none());

        // Round-trips into a real `AppConfig` with webhook fields at the top level.
        let config: AppConfig =
            serde_json::from_value(migrated).expect("migrated shape deserializes");
        assert!(config.webhook_enabled);
        assert_eq!(config.webhook_port, 9000);
        assert_eq!(config.webhook_public_url, "https://example.com");
    }

    #[test]
    fn migrate_empty_is_first_launch_empty_projects() {
        // `{}` → first launch: no project (so onboarding triggers), not a migrated
        // gocell-default project.
        let migrated = migrate_value(json!({}));
        assert_eq!(migrated["projects"], json!([]));
        assert_eq!(migrated["activeProjectId"], "");

        let config: AppConfig =
            serde_json::from_value(migrated).expect("first-launch shape deserializes");
        assert!(config.projects.is_empty());
        assert_eq!(config.active_project_id, "");
    }

    #[test]
    fn migrate_is_idempotent() {
        // Migrating a legacy flat config, then migrating the RESULT, is a no-op on
        // the second pass (the result already has a `projects` key). This is what
        // makes `save` (always new shape) → next `load` a stable fixed point.
        let raw = json!({
            "repo": "octocat/hello",
            "repoRoot": "/tmp/hello",
            "autoReview": false
        });
        let once = migrate_value(raw);
        let twice = migrate_value(once.clone());
        assert_eq!(once, twice);
    }

    /// AB#1225 local-api seed: an existing config carrying the legacy top-level `localApiPort`
    /// (the field being removed) must migrate into a `kind = local-api` listener so the user
    /// keeps their local trigger API. (An already-new config is the realistic case — its
    /// `localApiPort` survives `normalize_multiproject`'s identity early-return and is then
    /// seeded by the second pass.)
    #[test]
    fn migrate_seeds_local_api_listener_from_legacy_port() {
        let raw = json!({
            "projects": [],
            "activeProjectId": "",
            "localApiPort": 8788
        });

        let config: AppConfig =
            serde_json::from_value(migrate_value(raw)).expect("migrated shape deserializes");

        let local_api = local_api_entrypoints(&config);
        assert_eq!(local_api.len(), 1, "exactly one local-api listener seeded");
        assert!(local_api[0].enabled);
        assert_eq!(local_api[0].port, 8788);
    }

    /// AB#1225 local-api seed idempotence (detect-by-kind): migrating twice must not double-seed.
    #[test]
    fn migrate_local_api_seed_is_idempotent() {
        let raw = json!({
            "projects": [],
            "activeProjectId": "",
            "localApiPort": 8788
        });
        let once = migrate_value(raw);
        let twice = migrate_value(once.clone());
        assert_eq!(once, twice, "second pass is a no-op (already-seeded)");

        let config: AppConfig = serde_json::from_value(twice).expect("migrated shape deserializes");
        let count = local_api_entrypoints(&config).len();
        assert_eq!(count, 1, "not double-seeded");
    }

    /// AB#1225 local-api seed: `localApiPort: 0` (the old "disabled" sentinel) seeds a DISABLED
    /// entry preserving the port, so a user who had the API off stays off after migration.
    #[test]
    fn migrate_zero_local_api_port_seeds_disabled() {
        let raw = json!({
            "projects": [],
            "activeProjectId": "",
            "localApiPort": 0
        });

        let config: AppConfig =
            serde_json::from_value(migrate_value(raw)).expect("migrated shape deserializes");

        let local_api = local_api_entrypoints(&config);
        assert_eq!(local_api.len(), 1);
        assert!(!local_api[0].enabled, "port 0 → disabled");
        assert_eq!(local_api[0].port, 0);
    }

    /// AB#1225 local-api seed: a config whose `listeners` ALREADY has a local-api entry is NOT
    /// double-seeded even though a stray legacy `localApiPort` is also present (detect-by-kind).
    #[test]
    fn migrate_does_not_seed_when_local_api_listener_already_present() {
        let raw = json!({
            "projects": [],
            "activeProjectId": "",
            "localApiPort": 9999,
            "listeners": [{
                "id": "local-api",
                "name": "Local API",
                "kind": "local-api",
                "bindHost": "127.0.0.1",
                "port": 8788,
                "enabled": true,
                "auth": "bearer",
                "allowedOrigins": [],
                "publicUrl": ""
            }]
        });

        let config: AppConfig =
            serde_json::from_value(migrate_value(raw)).expect("migrated shape deserializes");

        let local_api = local_api_entrypoints(&config);
        assert_eq!(local_api.len(), 1, "existing entry kept, not duplicated");
        // The pre-existing entry (port 8788) wins — the stray `localApiPort: 9999` is ignored.
        assert_eq!(local_api[0].port, 8788);
    }

    /// AB#1225 F6 / #1553 (overflow guard): a legacy `localApiPort` ABOVE `u16::MAX` must NOT be
    /// seeded into `remoteAccess.entrypoints[]` — a truncating cast (e.g. `70000 as u16 == 4464`)
    /// would bind a bogus port. The deserialized helper must report disabled (`0`) rather than a
    /// truncated migrated endpoint.
    #[test]
    fn migrate_out_of_range_legacy_port_is_not_seeded() {
        let raw = json!({
            "projects": [],
            "activeProjectId": "",
            "localApiPort": 70000  // > u16::MAX (65535)
        });

        let config: AppConfig =
            serde_json::from_value(migrate_value(raw)).expect("migrated shape deserializes");
        let local_api = local_api_entrypoints(&config);
        assert!(
            local_api.iter().all(|entrypoint| entrypoint.port != 4464),
            "out-of-range port must not seed a truncated remoteAccess entrypoint: {local_api:?}"
        );
        assert_eq!(local_api_port(&config), 0);
    }

    /// AB#1225 F9 (documented pre-#35 edge — regression lock): a TRULY pre-#35 FLAT config (no
    /// `projects` key) carrying a top-level `localApiPort` has that key DROPPED by
    /// `normalize_multiproject` (it is in neither `PROJECT_KEYS` nor `WEBHOOK_KEYS`) before the seed
    /// pass runs, so the custom port is LOST and the config falls back to the default-seeded 8788
    /// listener on deserialize. This is acceptable (the pre-#35 flat shape predates AB#1043, which
    /// introduced `localApiPort`, so no real pre-#35 config carries it). Locking the documented
    /// behavior makes any future change to it a CONSCIOUS decision rather than a silent regression.
    #[test]
    fn migrate_pre35_flat_drops_legacy_local_api_port() {
        let raw = json!({
            "repo": "o/r",
            "localApiPort": 9999
        });

        let config: AppConfig =
            serde_json::from_value(migrate_value(raw)).expect("migrated shape deserializes");

        // Exactly one local-api listener, and it is the DEFAULT (8788) — NOT the dropped 9999.
        let local_api = local_api_entrypoints(&config);
        assert_eq!(
            local_api.len(),
            1,
            "default-seeded local-api listener present"
        );
        assert_eq!(
            local_api[0].port, 8788,
            "pre-#35 flat localApiPort is dropped → falls back to default 8788, not 9999"
        );
    }

    /// AB#1225 CLI port helper: resolves the enabled local-api listener's port, and 0 when there
    /// is none or it is disabled (the CLI treats 0 as "API disabled").
    #[test]
    fn local_api_port_helper_resolves_enabled_listener() {
        // Default config seeds an enabled local-api listener on 8788.
        assert_eq!(local_api_port(&AppConfig::default()), 8788);

        // Disabled local-api listener → 0 (off).
        let mut disabled = AppConfig::default();
        disabled.remote_access.entrypoints[0].enabled = false;
        assert_eq!(local_api_port(&disabled), 0);

        // No local-api entrypoint at all → 0 (off).
        let none = AppConfig {
            remote_access: crate::config::model::RemoteAccessConfig {
                entrypoints: Vec::new(),
                tunnels: Vec::new(),
            },
            ..AppConfig::default()
        };
        assert_eq!(local_api_port(&none), 0);
    }

    /// A bare project with the given `id` / `repo` for the `match_project_ref` cases
    /// (other fields irrelevant — the resolver only reads `id` / `repo`).
    fn proj(id: &str, repo: &str) -> Project {
        Project {
            id: id.to_string(),
            repo: repo.to_string(),
            ..Project::default()
        }
    }

    /// `match_project_ref` (AB#1042) — the pure trigger-funnel resolver. Pins: id-first
    /// (exact), repo case-insensitive, miss → err, repo ambiguity (>1) → err.
    #[test]
    fn match_project_ref_resolves_by_id_repo_ci_and_rejects_ambiguity() {
        let projects = vec![proj("alpha", "Owner/Repo-A"), proj("beta", "owner/repo-b")];

        // id hit (exact) → that project.
        assert_eq!(
            match_project_ref(&projects, "alpha").expect("id hit").id,
            "alpha"
        );

        // repo hit, case-insensitive (mirrors the webhook router's eq_ignore_ascii_case).
        assert_eq!(
            match_project_ref(&projects, "owner/repo-a")
                .expect("repo-ci hit")
                .id,
            "alpha"
        );
        assert_eq!(
            match_project_ref(&projects, "OWNER/REPO-B")
                .expect("repo-ci hit")
                .id,
            "beta"
        );

        // Miss (neither id nor known repo) → error.
        assert!(match_project_ref(&projects, "nope/missing").is_err());
        assert!(match_project_ref(&[], "alpha").is_err());
        // An empty reference matches no id and no repo → error (a blank trigger arg can't
        // accidentally resolve to a project).
        assert!(match_project_ref(&projects, "").is_err());

        // id takes precedence over a repo that equals another project's id: a project
        // whose REPO is literally "alpha" must not shadow the id hit on "alpha".
        let with_repo_named_like_id = vec![proj("alpha", "Owner/Repo-A"), proj("gamma", "alpha")];
        assert_eq!(
            match_project_ref(&with_repo_named_like_id, "alpha")
                .expect("id wins over repo named like id")
                .id,
            "alpha"
        );

        // >1 repo match (case-insensitive) → fail-closed reject with the projectId hint.
        // (config::validate forbids this, so it's a backstop, not an expected state.)
        let dup_repos = vec![proj("one", "owner/dup"), proj("two", "Owner/Dup")];
        let err =
            match_project_ref(&dup_repos, "owner/dup").expect_err("ambiguous repo must reject");
        assert!(err.message.contains("projectId"), "{}", err.message);
    }
}
