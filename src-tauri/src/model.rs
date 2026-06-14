//! Cross-slice shared types — the contract boundary between slices.
//!
//! Slices must not import each other's internals; any type that crosses a slice
//! boundary lives here. Serialized fields use camelCase for the frontend.

use serde::{Deserialize, Serialize};

/// A PR discovered by a [`crate::pr::source::PrSource`] that may need review.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub number: u64,
    pub head_sha: String,
    pub head_ref: String,
    pub author: String,
    pub is_cross_repository: bool,
    pub is_draft: bool,
    /// `"review"` or `"check"` — which pr-review mode the trigger label maps to.
    pub kind: String,
}

/// A PR row shown in the UI (display superset of [`Candidate`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequestView {
    pub number: u64,
    pub title: String,
    pub labels: Vec<String>,
    pub url: String,
}

/// Serde wire-shape lock for the cross-slice contract.
///
/// This is the **Medium carrier** for the `model.rs` ↔ `src/types.ts` serde
/// contract per `.claude/rules/prmonitor/ai-robust.md`. It is a contract LOCK
/// (characterization) test: it passes on current code and only fails if a field
/// is renamed or the camelCase serialization breaks. When a key here changes,
/// the downstream `src/types.ts` mirror must be updated in lockstep — that
/// downstream is the open end of this funnel (no machine check on the TS side
/// yet; future Hard path = codegen `types.ts` from `model.rs` + `git diff
/// --exit-code`).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_wire_shape_is_camel_case() {
        let candidate = Candidate {
            number: 1,
            head_sha: "abc123".to_string(),
            head_ref: "feature/x".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            kind: "review".to_string(),
        };

        let v = serde_json::to_value(&candidate).expect("Candidate serializes");

        // camelCase keys present.
        assert!(v.get("number").is_some());
        assert!(v.get("headSha").is_some());
        assert!(v.get("headRef").is_some());
        assert!(v.get("author").is_some());
        assert!(v.get("isCrossRepository").is_some());
        assert!(v.get("isDraft").is_some());
        assert!(v.get("kind").is_some());

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("head_sha").is_none());
        assert!(v.get("head_ref").is_none());
        assert!(v.get("is_cross_repository").is_none());
        assert!(v.get("is_draft").is_none());
    }

    #[test]
    fn pull_request_view_wire_shape_is_camel_case() {
        let view = PullRequestView {
            number: 1,
            title: "Add feature".to_string(),
            labels: vec!["review".to_string()],
            url: "https://example.com/pr/1".to_string(),
        };

        let v = serde_json::to_value(&view).expect("PullRequestView serializes");

        // camelCase / flat keys present.
        assert!(v.get("number").is_some());
        assert!(v.get("title").is_some());
        assert!(v.get("labels").is_some());
        assert!(v.get("url").is_some());
    }
}
