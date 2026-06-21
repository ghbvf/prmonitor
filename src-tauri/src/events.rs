//! Payloads streamed to the frontend.
//!
//! The review slice maps codex app-server notifications into [`ReviewEvent`]s
//! (see `review::events`); the frontend renders them in the review panel.

use serde::Serialize;

use crate::model::TrackedPrView;

/// Tauri event name carrying a [`PrEvent`] (scheduled/manual PR-list refresh).
pub const PRS_UPDATED_EVENT: &str = "prs:updated";

/// Tauri event name carrying a [`ReviewEvent`] (one streamed unit of a review
/// session). Mirrored by `REVIEW_EVENT` in `src/review/api.ts`.
pub const REVIEW_EVENT: &str = "review:event";

/// Payload emitted on [`PRS_UPDATED_EVENT`] each poll cycle (scheduled or manual).
///
/// Like [`ReviewEvent`], the container `rename_all` camelCases the *variant*
/// names into the `kind` tag and each struct variant carries its own
/// `rename_all` (serde does not propagate the container rule to a variant's
/// fields).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum PrEvent {
    /// The retained tracked-PR list (the persisted-retention view, not a raw
    /// per-round discovery — a transient miss flips presence rather than dropping
    /// a row). `project_id` is the routing key (#35): the frontend keys the PR list
    /// it updates by which project this refresh belongs to.
    #[serde(rename_all = "camelCase")]
    Updated {
        project_id: String,
        prs: Vec<TrackedPrView>,
    },
    /// A discovery cycle failed; the loop keeps running. `project_id` scopes the
    /// error to the offending project (#35).
    #[serde(rename_all = "camelCase")]
    Error { project_id: String, message: String },
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
    /// Incremental assistant message text. `project_id` is the routing key (#35):
    /// the frontend attributes the streamed delta to the owning project's session.
    #[serde(rename_all = "camelCase")]
    MessageDelta {
        project_id: String,
        thread_id: String,
        item_id: String,
        text: String,
    },
    /// Incremental reasoning text.
    #[serde(rename_all = "camelCase")]
    ReasoningDelta {
        project_id: String,
        thread_id: String,
        item_id: String,
        text: String,
    },
    /// The review turn ended (`completed` / `interrupted` / `failed`).
    #[serde(rename_all = "camelCase")]
    TurnCompleted {
        project_id: String,
        thread_id: String,
        status: String,
        /// The resolved pr-review comment URL (AB#1042), present on a `completed` turn
        /// when the source kind resolves one (GitHub: exact comment URL; Azure: PR URL;
        /// Bitbucket / a failed resolve: `None`). `skip_serializing_if` OMITS the key
        /// when `None`, so the wire matches the optional `commentUrl?: string` on
        /// `src/types.ts`'s `turnCompleted` (an absent key, not a JSON `null`).
        #[serde(skip_serializing_if = "Option::is_none")]
        comment_url: Option<String>,
    },
    /// A session-level error.
    #[serde(rename_all = "camelCase")]
    Error {
        project_id: String,
        thread_id: String,
        message: String,
    },
    /// An app-level background-write notice NOT tied to any one session — config
    /// invalid, one/more `start_review` failures, or a ledger-write failure during
    /// `crate::dispatch::auto_dispatch`; ALSO the one-time review-session persistence
    /// failure notice (review F9), the same class of silent background-write failure the
    /// user should see. Carries no `threadId` (session-less), but
    /// DOES carry `project_id` (#35) so the frontend can scope the app-level "auto
    /// review" notice to the offending project. Now that it has >1 field, it needs
    /// its own `#[serde(rename_all = "camelCase")]` so `projectId` serializes
    /// camelCase (the container tag rename only maps the variant name to the
    /// camelCase `"dispatchError"` — it does not propagate to field keys).
    #[serde(rename_all = "camelCase")]
    DispatchError { project_id: String, message: String },
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
    use crate::model::{PrPresence, PullRequestView, TrackedPrView};

    fn sample_view() -> TrackedPrView {
        TrackedPrView {
            pr: PullRequestView {
                number: 1,
                title: "Add feature".to_string(),
                labels: vec!["review".to_string()],
                url: "https://example.com/pr/1".to_string(),
                kind: "review".to_string(),
                skip_reason: None,
            },
            presence: PrPresence::Current,
            archived: false,
        }
    }

    #[test]
    fn pr_updated_wire_shape_is_camel_case() {
        let event = PrEvent::Updated {
            project_id: "p1".to_string(),
            prs: vec![sample_view()],
        };

        let v = serde_json::to_value(&event).expect("PrEvent serializes");

        assert_eq!(v["kind"], "updated");
        // `projectId` routing key present (camelCase); snake_case absent (#35).
        assert!(v.get("projectId").is_some());
        assert!(v.get("project_id").is_none());
        assert!(v.get("prs").is_some());
        // The row carries the flattened `PullRequestView` keys plus the retention
        // fields — a drift in `TrackedPrView`'s wire shape surfaces here too.
        let row = &v["prs"][0];
        assert_eq!(row["number"], 1);
        assert!(row.get("title").is_some());
        assert!(row.get("url").is_some());
        assert!(row.get("kind").is_some());
        // `sample_view` has `skip_reason: None` → JSON null at the flattened depth;
        // the snake_case form must not leak through the union either.
        assert_eq!(row["skipReason"], serde_json::Value::Null);
        assert!(row.get("skip_reason").is_none());
        assert_eq!(row["presence"], "current");
        assert_eq!(row["archived"], false);
    }

    #[test]
    fn pr_error_wire_shape_is_camel_case() {
        let event = PrEvent::Error {
            project_id: "p1".to_string(),
            message: "boom".to_string(),
        };

        let v = serde_json::to_value(&event).expect("PrEvent serializes");

        assert_eq!(v["kind"], "error");
        assert!(v.get("projectId").is_some());
        assert!(v.get("project_id").is_none());
        assert!(v.get("message").is_some());
    }

    #[test]
    fn prs_updated_event_name_is_pinned() {
        assert_eq!(PRS_UPDATED_EVENT, "prs:updated");
    }

    #[test]
    fn review_event_name_is_pinned() {
        // Mirrored by `REVIEW_EVENT` in `src/review/api.ts`; a drift breaks the
        // frontend's `listen` registration.
        assert_eq!(REVIEW_EVENT, "review:event");
    }

    #[test]
    fn message_delta_wire_shape_is_camel_case() {
        let event = ReviewEvent::MessageDelta {
            project_id: "p1".to_string(),
            thread_id: "t1".to_string(),
            item_id: "i1".to_string(),
            text: "hello".to_string(),
        };

        let v = serde_json::to_value(&event).expect("ReviewEvent serializes");

        // tag is camelCase.
        assert_eq!(v["kind"], "messageDelta");

        // camelCase field keys present.
        assert!(v.get("projectId").is_some());
        assert!(v.get("threadId").is_some());
        assert!(v.get("itemId").is_some());
        assert!(v.get("text").is_some());

        // snake_case forms absent — a rename would surface here.
        assert!(v.get("project_id").is_none());
        assert!(v.get("thread_id").is_none());
        assert!(v.get("item_id").is_none());
    }

    #[test]
    fn reasoning_delta_wire_shape_is_camel_case() {
        let event = ReviewEvent::ReasoningDelta {
            project_id: "p1".to_string(),
            thread_id: "t1".to_string(),
            item_id: "i1".to_string(),
            text: "why".to_string(),
        };
        let v = serde_json::to_value(&event).expect("ReviewEvent serializes");
        assert_eq!(v["kind"], "reasoningDelta");
        assert!(v.get("projectId").is_some());
        assert!(v.get("threadId").is_some());
        assert!(v.get("itemId").is_some());
        assert!(v.get("text").is_some());
        assert!(v.get("project_id").is_none());
        assert!(v.get("thread_id").is_none());
        assert!(v.get("item_id").is_none());
    }

    #[test]
    fn error_event_wire_shape_is_camel_case() {
        let event = ReviewEvent::Error {
            project_id: "p1".to_string(),
            thread_id: "t1".to_string(),
            message: "boom".to_string(),
        };
        let v = serde_json::to_value(&event).expect("ReviewEvent serializes");
        assert_eq!(v["kind"], "error");
        assert!(v.get("projectId").is_some());
        assert!(v.get("threadId").is_some());
        assert!(v.get("message").is_some());
        assert!(v.get("project_id").is_none());
        assert!(v.get("thread_id").is_none());
    }

    #[test]
    fn dispatch_error_wire_shape_is_camel_case_and_session_less() {
        let event = ReviewEvent::DispatchError {
            project_id: "p1".to_string(),
            message: "boom".to_string(),
        };
        let v = serde_json::to_value(&event).expect("ReviewEvent serializes");
        // Variant tag camelCased by the container rule; carries `projectId` + `message`.
        assert_eq!(v["kind"], "dispatchError");
        assert!(v.get("projectId").is_some());
        assert!(v.get("project_id").is_none());
        assert!(v.get("message").is_some());
        // Session-less: no thread id (a rename / accidental field would surface here,
        // and the `src/types.ts` mirror must stay session-less in lockstep).
        assert!(v.get("threadId").is_none());
        assert!(v.get("thread_id").is_none());
    }

    #[test]
    fn turn_completed_wire_shape_is_camel_case() {
        let event = ReviewEvent::TurnCompleted {
            project_id: "p1".to_string(),
            thread_id: "t1".to_string(),
            status: "completed".to_string(),
            // AB#1042: a resolved comment URL must surface as the camelCase `commentUrl`.
            comment_url: Some("https://example.com/pr/1#c".to_string()),
        };

        let v = serde_json::to_value(&event).expect("ReviewEvent serializes");

        // tag is camelCase.
        assert_eq!(v["kind"], "turnCompleted");

        // camelCase field keys present.
        assert!(v.get("projectId").is_some());
        assert!(v.get("threadId").is_some());
        assert!(v.get("status").is_some());
        // AB#1042: the new `commentUrl` field serializes camelCase; the snake_case form
        // must stay absent (mirrored by the optional `commentUrl` on `src/types.ts`).
        assert!(v.get("commentUrl").is_some());
        assert_eq!(v["commentUrl"], "https://example.com/pr/1#c");
        assert!(v.get("comment_url").is_none());

        // snake_case form absent — a rename would surface here.
        assert!(v.get("project_id").is_none());
        assert!(v.get("thread_id").is_none());

        // `comment_url: None` OMITS the key (skip_serializing_if), matching the optional
        // `commentUrl?: string` TS mirror — an absent key, not a JSON `null`.
        let no_url = serde_json::to_value(&ReviewEvent::TurnCompleted {
            project_id: "p1".to_string(),
            thread_id: "t1".to_string(),
            status: "interrupted".to_string(),
            comment_url: None,
        })
        .expect("ReviewEvent serializes");
        assert!(no_url.get("commentUrl").is_none(), "None omits commentUrl");
    }
}
