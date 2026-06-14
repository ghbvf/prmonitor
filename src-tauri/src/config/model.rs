//! Config slice domain model.

use serde::{Deserialize, Serialize};

/// Persisted application configuration. Defaults target the gocell repo the
/// app is built to serve.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
        }
    }
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

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("repo_root").is_none());
        assert!(v.get("poll_interval_secs").is_none());
        assert!(v.get("review_label").is_none());
        assert!(v.get("check_label").is_none());
        assert!(v.get("skill_rel_path").is_none());
        assert!(v.get("pr_cooldown_seconds").is_none());
    }
}
