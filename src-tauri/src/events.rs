//! Payloads streamed to the frontend.
//!
//! The review slice maps codex app-server notifications into [`ReviewEvent`]s
//! (see `review::events`); the frontend renders them in the review panel.

use serde::Serialize;

use crate::model::{InboxEntry, OutboxEntry, TrackedPrView};

/// Tauri event name carrying a [`PrEvent`] (scheduled/manual PR-list refresh).
pub const PRS_UPDATED_EVENT: &str = "prs:updated";

/// Tauri event name carrying a [`ReviewEvent`] (one streamed unit of a review
/// session). Mirrored by `REVIEW_EVENT` in `src/review/api.ts`.
pub const REVIEW_EVENT: &str = "review:event";

/// Tauri event name carrying an [`InboxEvent`] (AB#1065): one inbox row was added or
/// re-processed (a webhook delivery persisted / replayed). Mirrored by
/// `INBOX_UPDATED_EVENT` in `src/inbox/api.ts`.
pub const INBOX_UPDATED_EVENT: &str = "inbox:updated";

/// Tauri event name carrying an [`OutboxEvent`] (AB#1066): one outbox row was enqueued or
/// transitioned (executed / retried / dead-lettered). Mirrored by `OUTBOX_UPDATED_EVENT` in
/// `src/outbox/api.ts`.
pub const OUTBOX_UPDATED_EVENT: &str = "outbox:updated";

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

/// Payload emitted on [`INBOX_UPDATED_EVENT`] (AB#1065) when an inbox row is added (a new
/// deduped webhook delivery) or re-processed (a replay). The frontend's event-inbox panel
/// upserts the carried [`InboxEntry`] into the list it keys by `project_id` (the routing key,
/// mirroring [`PrEvent`]).
///
/// Like [`PrEvent`] / [`ReviewEvent`], the container `rename_all` camelCases the *variant*
/// name into the `kind` tag and each struct variant carries its own `rename_all` (serde does
/// not propagate the container rule to a variant's fields).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum InboxEvent {
    /// An inbox row was added or re-processed. `entry` is the full current row (so the
    /// frontend can upsert it without a follow-up `inbox_list`); `project_id` is the routing
    /// key the panel scopes the upsert to.
    #[serde(rename_all = "camelCase")]
    Updated {
        project_id: String,
        entry: InboxEntry,
    },
}

/// Payload emitted on [`OUTBOX_UPDATED_EVENT`] (AB#1066) when an outbox row is enqueued or
/// transitions (executed → `done`, retried, or dead-lettered → `dead`). The frontend's
/// action-outbox panel upserts the carried [`OutboxEntry`] into the list it keys by `project_id`
/// (the routing key, mirroring [`InboxEvent`] / [`PrEvent`]).
///
/// Like the unions above, the container `rename_all` camelCases the *variant* name into the
/// `kind` tag and each struct variant carries its own `rename_all`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum OutboxEvent {
    /// An outbox row was enqueued or transitioned. `entry` is the full current row (so the
    /// frontend can upsert it without a follow-up `outbox_list`); `project_id` is the routing
    /// key the panel scopes the upsert to.
    #[serde(rename_all = "camelCase")]
    Updated {
        project_id: String,
        entry: OutboxEntry,
    },
    /// A worker-CYCLE-level failure NOT tied to one row (AB#1182): a `claim_due` query failed, a
    /// terminal-state write failed, or the `outbox:updated` re-read failed — the `eprintln!`-only
    /// paths that were invisible on the desktop app's stderr. Carries NO `project_id` (a cycle
    /// failure spans the whole queue — `claim_due` is not project-scoped, and the read-failure site
    /// may have no row left to attribute), so the panel surfaces it as a GLOBAL banner rather than a
    /// per-row upsert. `operation` names the failing site (`"claim"` / `"record"` / `"announce"`)
    /// so the banner is actionable. Its own `#[serde(rename_all)]` so `operation`/`message`
    /// serialize camelCase (the container tag rename maps only the variant name to `"error"`).
    #[serde(rename_all = "camelCase")]
    Error { operation: String, message: String },
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

/// The unified realtime-stream envelope (AB#1072 / #1373) — the single type the in-process
/// [`crate::stream::StreamBus`] broadcasts. It wraps the existing per-domain event unions so a
/// SECOND consumer (the local-api SSE endpoint) can subscribe to ONE typed stream, while the
/// desktop frontend keeps receiving the inner [`ReviewEvent`] / [`OutboxEvent`] on their existing
/// Tauri channels. The demux + dual emit live in [`crate::stream::emit`], whose exhaustive `match`
/// over this sealed enum is the **Hard carrier**: a new domain without an arm is a compile error,
/// forcing it to declare its desktop channel there.
///
/// `#[serde(tag = "domain")]` adds a `domain` discriminant ALONGSIDE each inner union's own
/// internal `kind` tag (both inner enums serialize as a JSON object, so serde's internal tagging
/// merges the `domain` key in): e.g. `{"domain":"review","kind":"messageDelta","projectId":…}`.
/// Only domains with a LIVE producer are present (review + action); `terminal` / `workflow` are
/// added when their producers land (#1372 etc.), never pre-declared empty (no speculative variant).
///
/// HTTP-only (SSE) wire type: like the `local_api` request/response structs it is NOT a Tauri
/// command payload, so it intentionally has NO `src/types.ts` mirror — the same cross-Rust-slice-
/// but-backend-internal status as `model::Candidate` / `model::Notification`. The camelCase +
/// `domain` contract is locked by the goldens below (**Medium carrier**), not a hand-mirrored TS
/// interface (Serialize-only, like the inner unions — the bus never deserializes events).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "domain", rename_all = "camelCase")]
pub enum StreamEvent {
    /// A review-session event (deltas / terminal / session error). Desktop channel: `review:event`.
    Review(ReviewEvent),
    /// An action-outbox event (row enqueued / transitioned / cycle error). Desktop channel:
    /// `outbox:updated`.
    Action(OutboxEvent),
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
    use crate::model::{
        ActionKind, ActionStatus, Event, EventType, InboxStatus, OutboxEntry, PrPresence,
        PullRequestView, SourceKind, TrackedPrView,
    };

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
    fn inbox_updated_event_name_is_pinned() {
        // Mirrored by `INBOX_UPDATED_EVENT` in `src/inbox/api.ts`; a drift breaks the frontend's
        // `listen` registration for the event-inbox panel.
        assert_eq!(INBOX_UPDATED_EVENT, "inbox:updated");
    }

    // Serde wire-shape lock for the AB#1065 `InboxEvent` discriminated union (Medium carrier
    // per ai-robust.md): the `kind` tag is camelCase, `projectId` is the camelCase routing key,
    // and the NESTED `entry` carries the full `InboxEntry` wire shape (a drift in `InboxEntry` /
    // `Event` surfaces here too). The downstream `src/types.ts` `InboxEvent` union must be synced
    // in lockstep (the open end of this funnel; future Hard path = codegen from `events.rs`).
    #[test]
    fn inbox_updated_wire_shape_is_camel_case() {
        let event = InboxEvent::Updated {
            project_id: "p1".to_string(),
            entry: InboxEntry {
                id: 7,
                event: Event {
                    dedupe_key: "github:abc-123".to_string(),
                    source: SourceKind::Github,
                    event_type: EventType::PullRequest,
                    project_id: "p1".to_string(),
                    repo: "owner/repo".to_string(),
                    number: Some(7),
                    title: "Add feature".to_string(),
                    body: String::new(),
                    labels: vec!["pr-review".to_string()],
                    url: "https://example.com/pr/7".to_string(),
                    received_at_epoch: 1_700_000_000,
                },
                status: InboxStatus::Received,
                processed_at_epoch: None,
                error: None,
            },
        };

        let v = serde_json::to_value(&event).expect("InboxEvent serializes");

        assert_eq!(v["kind"], "updated");
        assert!(v.get("projectId").is_some());
        assert!(v.get("project_id").is_none());

        // The nested `entry` carries the `InboxEntry` wire shape: camelCase keys, a nested
        // `event` object, and the pinned `status` string.
        let entry = &v["entry"];
        assert!(entry.get("id").is_some());
        assert!(entry.get("processedAtEpoch").is_some());
        assert!(entry.get("processed_at_epoch").is_none());
        assert_eq!(entry["status"], "received");
        assert!(entry["event"].is_object());
        assert!(entry["event"].get("dedupeKey").is_some());
    }

    #[test]
    fn outbox_updated_event_name_is_pinned() {
        // Mirrored by `OUTBOX_UPDATED_EVENT` in `src/outbox/api.ts`; a drift breaks the frontend's
        // `listen` registration for the action-outbox panel.
        assert_eq!(OUTBOX_UPDATED_EVENT, "outbox:updated");
    }

    // Serde wire-shape lock for the AB#1066 `OutboxEvent` discriminated union (Medium carrier per
    // ai-robust.md): the `kind` tag is camelCase, `projectId` is the camelCase routing key, and the
    // NESTED `entry` carries the full `OutboxEntry` wire shape (a drift in `OutboxEntry` surfaces
    // here too). The downstream `src/types.ts` `OutboxEvent` union must be synced in lockstep (the
    // open end of this funnel; future Hard path = codegen from `events.rs`).
    #[test]
    fn outbox_updated_wire_shape_is_camel_case() {
        let event = OutboxEvent::Updated {
            project_id: "p1".to_string(),
            entry: OutboxEntry {
                id: 7,
                project_id: "p1".to_string(),
                kind: ActionKind::Notification,
                summary: "PR #7 review 完成".to_string(),
                status: ActionStatus::Pending,
                attempt_count: 0,
                next_attempt_at: 1_700_000_000,
                last_error: None,
                created_at: 1_700_000_000,
                updated_at: 1_700_000_000,
            },
        };

        let v = serde_json::to_value(&event).expect("OutboxEvent serializes");

        assert_eq!(v["kind"], "updated");
        assert!(v.get("projectId").is_some());
        assert!(v.get("project_id").is_none());

        // The nested `entry` carries the `OutboxEntry` wire shape: camelCase keys + pinned strings.
        let entry = &v["entry"];
        assert!(entry.get("id").is_some());
        assert!(entry.get("attemptCount").is_some());
        assert!(entry.get("attempt_count").is_none());
        assert!(entry.get("nextAttemptAt").is_some());
        assert_eq!(entry["kind"], "notification");
        assert_eq!(entry["status"], "pending");
        assert_eq!(entry["lastError"], serde_json::Value::Null);
    }

    // Serde wire-shape lock for the AB#1182 `OutboxEvent::Error` variant (Medium carrier): the
    // `kind` tag is camelCase `"error"`, `operation`/`message` are present, and the variant carries
    // NO project routing key (a cycle-level failure isn't project-scoped). The downstream
    // `src/types.ts` `OutboxEvent` union must mirror this 2-arm shape in lockstep (the open end of
    // the funnel; future Hard path = codegen from `events.rs`).
    #[test]
    fn outbox_error_wire_shape_is_camel_case_and_project_less() {
        let event = OutboxEvent::Error {
            operation: "claim".to_string(),
            message: "database is locked".to_string(),
        };

        let v = serde_json::to_value(&event).expect("OutboxEvent serializes");

        assert_eq!(v["kind"], "error");
        assert!(v.get("operation").is_some());
        assert!(v.get("message").is_some());
        // Project-less (a cycle failure spans the queue): a regression that added a routing key
        // surfaces here, and the TS mirror must stay project-less in lockstep.
        assert!(v.get("projectId").is_none());
        assert!(v.get("project_id").is_none());
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

    // Serde wire-shape lock for the AB#1072 `StreamEvent` envelope (Medium carrier per
    // ai-robust.md): the `domain` discriminant is present + camelCase, and the inner union's own
    // `kind` tag + camelCase fields coexist in the SAME object (serde internal tagging merges the
    // `domain` key into the inner map). This is the SSE wire contract — there is no `src/types.ts`
    // mirror (HTTP-only, backend-internal), so this golden is the SOLE machine check on the shape.
    #[test]
    fn stream_event_review_wire_shape_has_domain_and_inner_kind() {
        let event = StreamEvent::Review(ReviewEvent::MessageDelta {
            project_id: "p1".to_string(),
            thread_id: "t1".to_string(),
            item_id: "i1".to_string(),
            text: "hello".to_string(),
        });

        let v = serde_json::to_value(&event).expect("StreamEvent serializes");

        // `domain` discriminant present + camelCase value.
        assert_eq!(v["domain"], "review");
        // The inner ReviewEvent is FLATTENED: its own `kind` tag + camelCase fields coexist.
        assert_eq!(v["kind"], "messageDelta");
        assert!(v.get("projectId").is_some());
        assert!(v.get("threadId").is_some());
        // snake_case must not leak through the envelope.
        assert!(v.get("project_id").is_none());
        assert!(v.get("thread_id").is_none());
    }

    #[test]
    fn stream_event_action_wire_shape_has_domain_and_inner_kind() {
        let event = StreamEvent::Action(OutboxEvent::Error {
            operation: "claim".to_string(),
            message: "database is locked".to_string(),
        });

        let v = serde_json::to_value(&event).expect("StreamEvent serializes");

        assert_eq!(v["domain"], "action");
        // The inner OutboxEvent's `kind` tag + fields coexist with `domain`.
        assert_eq!(v["kind"], "error");
        assert!(v.get("operation").is_some());
        assert!(v.get("message").is_some());
    }
}
