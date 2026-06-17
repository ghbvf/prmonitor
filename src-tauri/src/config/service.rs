//! Config slice logic.
//!
//! Backend-owned persistence via `tauri-plugin-store`'s Rust `StoreExt`. The
//! frontend calls the `get_config` / `set_config` commands (not the store plugin
//! directly), so all reads/writes funnel through here.

use serde_json::{json, Map, Value};
use tauri_plugin_store::StoreExt;

use super::model::{AppConfig, Project};
use crate::error::{AppError, AppResult};

/// Store file holding the persisted config.
const STORE_FILE: &str = "config.json";
/// Key under which the [`AppConfig`] value lives in the store.
const CONFIG_KEY: &str = "appConfig";

/// The 11 per-project keys lifted out of the legacy flat single-project config
/// into the migrated [`Project`] object (#35). camelCase wire names (the persisted
/// shape — `save` writes `serde_json::to_value(&AppConfig)`, which is camelCase).
const PROJECT_KEYS: &[&str] = &[
    "repo",
    "repoRoot",
    "pollIntervalSecs",
    "authors",
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
fn migrate_value(raw: Value) -> Value {
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
    project.insert("id".to_string(), json!("default"));
    project.insert("name".to_string(), json!("default"));
    project.insert("enabled".to_string(), json!(true));
    for key in PROJECT_KEYS {
        if let Some(v) = old.get(*key) {
            project.insert((*key).to_string(), v.clone());
        }
    }

    let mut new = Map::new();
    new.insert("projects".to_string(), json!([Value::Object(project)]));
    new.insert("activeProjectId".to_string(), json!("default"));
    for key in WEBHOOK_KEYS {
        if let Some(v) = old.get(*key) {
            new.insert((*key).to_string(), v.clone());
        }
    }

    Value::Object(new)
}

/// Loads the persisted configuration, falling back to [`AppConfig::default`]
/// when nothing has been stored yet. The raw stored value is run through
/// [`migrate_value`] first so a legacy flat single-project config (#35) upgrades to
/// the multi-project shape before deserialization.
pub fn load<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> AppResult<AppConfig> {
    let store = app
        .store(STORE_FILE)
        .map_err(|e| AppError::new(format!("打开配置存储失败: {e}")))?;

    match store.get(CONFIG_KEY) {
        None => Ok(AppConfig::default()),
        // Surface (don't silently discard) a corrupt/incompatible persisted
        // config so the user can fix it rather than lose their settings.
        Some(value) => serde_json::from_value(migrate_value(value))
            .map_err(|e| AppError::new(format!("解析持久化配置失败: {e}"))),
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
    let store = app
        .store(STORE_FILE)
        .map_err(|e| AppError::new(format!("打开配置存储失败: {e}")))?;

    let value = serde_json::to_value(config).map_err(|e| AppError::new(e.to_string()))?;
    // tauri-plugin-store 2.x: `Store::set` is infallible and returns `()`.
    store.set(CONFIG_KEY, value);
    store
        .save()
        .map_err(|e| AppError::new(format!("写入配置存储失败: {e}")))?;
    Ok(())
}

/// Persists `active_project_id` (#35) WITHOUT re-validating the whole config —
/// switching the viewed project is a UI navigation action, not a config edit, and
/// must not be blocked because some OTHER enabled project's filesystem field went
/// stale. Verifies the target project exists (a stale id is rejected), then writes
/// the pointer through the same store as [`save`]. The frontend `useProjects().setActive`
/// calls the `set_active_project` command, which funnels here.
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
        assert_eq!(migrated["activeProjectId"], "default");
        let projects = migrated["projects"].as_array().expect("projects array");
        assert_eq!(projects.len(), 1);

        let project = &projects[0];
        assert_eq!(project["id"], "default");
        assert_eq!(project["name"], "default");
        assert_eq!(project["enabled"], true);
        // The 11 lifted per-project keys carried over verbatim.
        assert_eq!(project["repo"], "octocat/hello");
        assert_eq!(project["repoRoot"], "/tmp/hello");
        assert_eq!(project["pollIntervalSecs"], 60);
        assert_eq!(project["authors"], json!(["octocat"]));
        assert_eq!(project["reviewLabel"], "needs-review");
        assert_eq!(project["checkLabel"], "needs-check");
        assert_eq!(project["skillRelPath"], ".codex/skills/pr-review/SKILL.md");
        assert_eq!(project["prCooldownSeconds"], 900);
        assert_eq!(project["sourceKind"], "github");
        assert_eq!(project["engineKind"], "codex");
        assert_eq!(project["autoReview"], true);

        // The migrated shape must deserialize into a real `AppConfig` (lenient path
        // `load` uses) with the lifted values intact.
        let config: AppConfig =
            serde_json::from_value(migrated).expect("migrated shape deserializes");
        assert_eq!(config.active_project_id, "default");
        assert_eq!(config.projects.len(), 1);
        assert_eq!(config.projects[0].repo, "octocat/hello");
        assert!(config.projects[0].auto_review);
    }

    #[test]
    fn migrate_new_shape_is_identity() {
        // A value already carrying `projects` is the new shape → returned unchanged.
        let raw = json!({
            "projects": [{ "id": "p1", "repo": "owner/name" }],
            "activeProjectId": "p1",
            "webhookEnabled": false
        });
        assert_eq!(migrate_value(raw.clone()), raw);
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
}
