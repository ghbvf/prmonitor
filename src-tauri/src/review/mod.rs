//! Review slice: drive a review engine (codex app-server MVP) to run the pr-review skill,
//! stream output to the UI, and stop sessions.
//!
//! `engine` defines the [`engine::ReviewEngine`] seam (issue #11); the codex
//! app-server is the MVP impl under [`engines::codex`]. `session` holds the
//! session state machine and the notification → [`crate::events::ReviewEvent`]
//! mapping (the pump task).

/// Outbox review-execution claim (AB#1204): a write-ahead, `outbox_id`-keyed durable claim that
/// lets the at-least-once outbox executor suppress a duplicate review after a crash-restart,
/// without over-blocking a new-commit re-review. Consulted by [`commands::start_for_outbox`].
pub mod claim_store;
pub mod commands;
/// Read-only pr-review comment URL resolver (AB#1042). Slice-internal (not `pub`): only
/// the funnel in [`session`] calls it, so it is reachable as `super::comment_url` from
/// `session.rs` while staying off the review slice's public surface.
mod comment_url;
/// Deeplink request transport (AB#1045): parses + validates a
/// `prmonitor://review?pr=N&repo=R&requestId=<32-lower-hex>`
/// URL and records it through the durable external-review ingress, then surfaces fire-and-forget
/// completion through the receipt-driven workflow. The parser is pure (table-unit-tested); a
/// golden locks the registered scheme.
pub mod deeplink;
pub mod engine;
pub mod engines;
pub mod history_store;
/// Local REST API request transport (AB#1043): an axum router that
/// lets a third party (curl/CLI) create a durable review receipt and poll for completion + comment
/// URL. Token/Host/Origin fail-closed; never tunneled.
pub mod local_api;
/// Outbound notification seam (AB#1070): the [`notify::NotificationProvider`] trait + the
/// reference desktop notifier, dispatched by channel via an exhaustive `match NotificationKind`
/// ([`notify::deliver`], the Hard carrier). The output-side mirror of `pr::source`; the
/// deeplink completion/failure path delivers through it.
pub mod notify;
pub mod session;
