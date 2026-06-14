//! Payloads streamed to the frontend.
//!
//! The codex slice maps codex app-server notifications into [`ReviewEvent`]s
//! (see `codex::events`); the frontend renders them in the review panel.

use serde::Serialize;

/// A single streamed unit of a review session, forwarded to the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ReviewEvent {
    /// Incremental assistant message text.
    MessageDelta {
        thread_id: String,
        item_id: String,
        text: String,
    },
    /// Incremental reasoning text.
    ReasoningDelta {
        thread_id: String,
        item_id: String,
        text: String,
    },
    /// The review turn ended (`completed` / `interrupted` / `failed`).
    TurnCompleted { thread_id: String, status: String },
    /// A session-level error.
    Error { thread_id: String, message: String },
}
