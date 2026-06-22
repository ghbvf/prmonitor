//! Review slice: drive a review engine (codex app-server MVP) to run the pr-review skill,
//! stream output to the UI, and stop sessions.
//!
//! `engine` defines the [`engine::ReviewEngine`] seam (issue #11); the codex
//! app-server is the MVP impl under [`engines::codex`]. `session` holds the
//! session state machine and the notification → [`crate::events::ReviewEvent`]
//! mapping (the pump task).

pub mod commands;
/// Read-only pr-review comment URL resolver (AB#1042). Slice-internal (not `pub`): only
/// the funnel in [`session`] calls it, so it is reachable as `super::comment_url` from
/// `session.rs` while staying off the review slice's public surface.
mod comment_url;
pub mod engine;
pub mod engines;
pub mod history_store;
/// Local REST API trigger transport (AB#1043): a resident `127.0.0.1`-only axum listener that
/// wraps the [`commands::trigger_review`] funnel so a third party (curl/CLI) can trigger a
/// review and poll for completion + comment URL. Token/Host/Origin fail-closed; never tunneled.
pub mod local_api;
pub mod session;
