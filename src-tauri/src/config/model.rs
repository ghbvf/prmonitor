//! Config slice domain model.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::model::{EngineKind, SourceKind};

/// Persisted application configuration. Defaults target the gocell repo the
/// app is built to serve.
///
/// `#[serde(default)]` makes deserialization forward-compatible: a persisted
/// config missing fields (older versions, or before a #11 field is added) fills
/// absent fields from [`Default`] instead of failing. Do not add
/// `#[serde(deny_unknown_fields)]` — it would break that forward-compat.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppConfig {
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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
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
        }
    }
}

/// Validates filesystem-dependent config fields before persisting.
///
/// Pure (no I/O beyond `stat`): `repo_root` must be a non-empty path to an
/// existing directory, and `skill_rel_path` resolved against it must be an
/// existing file. Errors funnel through [`AppError`] naming the offending field.
pub fn validate(config: &AppConfig) -> AppResult<()> {
    let repo_root = config.repo_root.trim();
    if repo_root.is_empty() || !Path::new(repo_root).is_dir() {
        return Err(AppError::new(format!(
            "repoRoot 路径不存在或不是目录: {}",
            config.repo_root
        )));
    }

    let skill = Path::new(repo_root).join(&config.skill_rel_path);
    if !skill.is_file() {
        return Err(AppError::new(format!(
            "skill 路径不存在: {}",
            skill.display()
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

    #[test]
    fn app_config_wire_shape_is_camel_case() {
        let config = AppConfig {
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
        };

        let v = serde_json::to_value(&config).expect("AppConfig serializes");

        // camelCase keys present.
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

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("repo_root").is_none());
        assert!(v.get("poll_interval_secs").is_none());
        assert!(v.get("review_label").is_none());
        assert!(v.get("check_label").is_none());
        assert!(v.get("skill_rel_path").is_none());
        assert!(v.get("pr_cooldown_seconds").is_none());
        assert!(v.get("source_kind").is_none());
        assert!(v.get("engine_kind").is_none());
    }

    #[test]
    fn validate_accepts_existing_repo_root_and_skill() {
        let config = AppConfig {
            repo_root: env!("CARGO_MANIFEST_DIR").to_string(),
            skill_rel_path: "Cargo.toml".to_string(),
            ..AppConfig::default()
        };
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn validate_rejects_empty_repo_root() {
        let config = AppConfig {
            repo_root: String::new(),
            skill_rel_path: "Cargo.toml".to_string(),
            ..AppConfig::default()
        };
        assert!(validate(&config).is_err());
    }

    #[test]
    fn validate_rejects_missing_repo_root() {
        let config = AppConfig {
            repo_root: "/no/such/dir/xyz".to_string(),
            skill_rel_path: "Cargo.toml".to_string(),
            ..AppConfig::default()
        };
        assert!(validate(&config).is_err());
    }

    #[test]
    fn validate_rejects_missing_skill() {
        let config = AppConfig {
            repo_root: env!("CARGO_MANIFEST_DIR").to_string(),
            skill_rel_path: "definitely_missing.md".to_string(),
            ..AppConfig::default()
        };
        assert!(validate(&config).is_err());
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
        let parsed: AppConfig = serde_json::from_value(serde_json::json!({"repo": "x/y"}))
            .expect("partial object deserializes via serde(default)");
        let expected = AppConfig {
            repo: "x/y".to_string(),
            ..AppConfig::default()
        };
        assert_eq!(
            serde_json::to_value(&parsed).expect("parsed serializes"),
            serde_json::to_value(&expected).expect("expected serializes")
        );
    }
}
