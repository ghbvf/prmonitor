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

/// Serde wire-shape locks for `model.rs`'s cross-slice types.
///
/// The **Medium carrier** for these serde shapes per
/// `.claude/rules/prmonitor/ai-robust.md`. Each is a LOCK (characterization)
/// test: it passes on current code and only fails if a field is renamed or the
/// camelCase serialization breaks. The two types differ in their *downstream*,
/// so their contracts are not the same thing:
///
/// - [`PullRequestView`] is a **front/back contract** mirrored in
///   `src/types.ts`; a key change must be synced there in lockstep — the open
///   end of that funnel (no machine check on the TS side yet; future Hard path =
///   codegen `types.ts` from `model.rs` + `git diff --exit-code`).
/// - [`Candidate`] is **backend-internal**, cross-Rust-slice only: per the
///   charter it is intentionally *not* mirrored in `src/types.ts`, so its lock
///   guards the camelCase wire shape the `pr`/`review` slices rely on, **not** a
///   front/back contract — do not sync it to the frontend.
#[cfg(test)]
mod tests {
    use super::*;

    // Backend-internal cross-slice lock: `Candidate` is not exposed to the
    // frontend and is intentionally absent from `src/types.ts` (per the charter).
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

    // Front/back contract lock: `PullRequestView` is mirrored in `src/types.ts`;
    // a field change here must be synced to that interface in lockstep.
    #[test]
    fn pull_request_view_wire_shape_is_camel_case() {
        let view = PullRequestView {
            number: 1,
            title: "Add feature".to_string(),
            labels: vec!["review".to_string()],
            url: "https://example.com/pr/1".to_string(),
        };

        let v = serde_json::to_value(&view).expect("PullRequestView serializes");

        // No snake_case negative assertions: every PullRequestView field name is
        // single-word (no underscores), so camelCase serialization is a no-op and
        // there is no snake_case form to guard against. If a multi-word field is
        // added later, add `is_none()` guards like the Candidate test above.
        // camelCase / flat keys present.
        assert!(v.get("number").is_some());
        assert!(v.get("title").is_some());
        assert!(v.get("labels").is_some());
        assert!(v.get("url").is_some());
    }
}
