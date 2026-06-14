//! Payloads streamed to the frontend.
//!
//! The review slice maps codex app-server notifications into [`ReviewEvent`]s
//! (see `review::events`); the frontend renders them in the review panel.

use serde::Serialize;

use crate::model::PullRequestView;

/// Tauri event name carrying a [`PrEvent`] (scheduled/manual PR-list refresh).
pub const PRS_UPDATED_EVENT: &str = "prs:updated";

/// Payload emitted on [`PRS_UPDATED_EVENT`] each poll cycle (scheduled or manual).
///
/// Like [`ReviewEvent`], the container `rename_all` camelCases the *variant*
/// names into the `kind` tag and each struct variant carries its own
/// `rename_all` (serde does not propagate the container rule to a variant's
/// fields).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum PrEvent {
    /// A fresh PR list (a successful discovery cycle).
    #[serde(rename_all = "camelCase")]
    Updated { prs: Vec<PullRequestView> },
    /// A discovery cycle failed; the loop keeps running.
    #[serde(rename_all = "camelCase")]
    Error { message: String },
}

/// A single streamed unit of a review session, forwarded to the frontend.
///
/// The container `rename_all` camelCases the *variant* names into the `kind`
/// tag; each struct variant carries its own `rename_all` because serde does not
/// propagate the container rule to a variant's fields — without it the field
/// keys would serialize snake_case and diverge from the `src/types.ts` contract.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ReviewEvent {
    /// Incremental assistant message text.
    #[serde(rename_all = "camelCase")]
    MessageDelta {
        thread_id: String,
        item_id: String,
        text: String,
    },
    /// Incremental reasoning text.
    #[serde(rename_all = "camelCase")]
    ReasoningDelta {
        thread_id: String,
        item_id: String,
        text: String,
    },
    /// The review turn ended (`completed` / `interrupted` / `failed`).
    #[serde(rename_all = "camelCase")]
    TurnCompleted { thread_id: String, status: String },
    /// A session-level error.
    #[serde(rename_all = "camelCase")]
    Error { thread_id: String, message: String },
}

/// Serde wire-shape lock for the `ReviewEvent` discriminated union.
///
/// This is the **Medium carrier** for the `events.rs` ↔ `src/types.ts` serde
/// contract per `.claude/rules/prmonitor/ai-robust.md` (same spirit as the
/// `model.rs` lock). It is a contract LOCK (characterization) test: it passes
/// on current code and only fails if a variant/field is renamed, the `kind`
/// tag stops being camelCase, or the camelCase serialization breaks. When a key
/// here changes, the downstream `src/types.ts` discriminated union must be
/// updated in lockstep — that downstream is the open end of this funnel (no
/// machine check on the TS side yet; future Hard path = codegen the TS union
/// from `events.rs` + `git diff --exit-code`).
///
/// The same lock applies to the [`PrEvent`] union below: its downstream is the
/// `PrEvent` discriminated union in `src/types.ts` — the open end of this funnel
/// (no machine check on the TS side yet; future Hard path = codegen the TS union
/// from `events.rs` + `git diff --exit-code`).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PullRequestView;

    fn sample_view() -> PullRequestView {
        PullRequestView {
            number: 1,
            title: "Add feature".to_string(),
            labels: vec!["review".to_string()],
            url: "https://example.com/pr/1".to_string(),
            kind: "review".to_string(),
            skip_reason: None,
        }
    }

    #[test]
    fn pr_updated_wire_shape_is_camel_case() {
        let event = PrEvent::Updated {
            prs: vec![sample_view()],
        };

        let v = serde_json::to_value(&event).expect("PrEvent serializes");

        assert_eq!(v["kind"], "updated");
        assert!(v.get("prs").is_some());
    }

    #[test]
    fn pr_error_wire_shape_is_camel_case() {
        let event = PrEvent::Error {
            message: "boom".to_string(),
        };

        let v = serde_json::to_value(&event).expect("PrEvent serializes");

        assert_eq!(v["kind"], "error");
        assert!(v.get("message").is_some());
    }

    #[test]
    fn prs_updated_event_name_is_pinned() {
        assert_eq!(PRS_UPDATED_EVENT, "prs:updated");
    }

    #[test]
    fn message_delta_wire_shape_is_camel_case() {
        let event = ReviewEvent::MessageDelta {
            thread_id: "t1".to_string(),
            item_id: "i1".to_string(),
            text: "hello".to_string(),
        };

        let v = serde_json::to_value(&event).expect("ReviewEvent serializes");

        // tag is camelCase.
        assert_eq!(v["kind"], "messageDelta");

        // camelCase field keys present.
        assert!(v.get("threadId").is_some());
        assert!(v.get("itemId").is_some());
        assert!(v.get("text").is_some());

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("thread_id").is_none());
        assert!(v.get("item_id").is_none());
    }

    #[test]
    fn turn_completed_wire_shape_is_camel_case() {
        let event = ReviewEvent::TurnCompleted {
            thread_id: "t1".to_string(),
            status: "completed".to_string(),
        };

        let v = serde_json::to_value(&event).expect("ReviewEvent serializes");

        // tag is camelCase.
        assert_eq!(v["kind"], "turnCompleted");

        // camelCase field keys present.
        assert!(v.get("threadId").is_some());
        assert!(v.get("status").is_some());

        // snake_case form absent — a rename would surface here.
        assert!(v.get("thread_id").is_none());
    }
}
