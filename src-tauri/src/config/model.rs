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
