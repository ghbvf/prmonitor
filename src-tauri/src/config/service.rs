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

/// Re-export the project domain type THROUGH the config public service surface (#35,
/// F9). The `pr` slice (scheduler / commands) depends on `Project` via
/// `config::service::Project`, not `config::model::Project` — so its cross-slice
/// coupling is to the service (the slice's public API), keeping the model an internal
/// detail the service mediates. The functions below (`project` / `project_validated`)
/// use `Project` through this same re-export.
pub use super::model::Project;

/// `id`/`name` assigned to the single project lifted out of a legacy flat config by
/// [`migrate_value`] (#35). One source so the migration and its tests agree on the
/// id the active-project pointer (`activeProjectId`) is also set to.
const MIGRATED_PROJECT_ID: &str = "default";

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
        assert!(config.projects[0].auto_review);
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
        assert_eq!(config.active_project_id, MIGRATED_PROJECT_ID);
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
