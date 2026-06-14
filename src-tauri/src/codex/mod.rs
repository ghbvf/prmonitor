//! Codex review slice: drive codex app-server to run the pr-review skill,
//! stream output to the UI, and stop sessions.
//!
//! `engine` defines the [`engine::ReviewEngine`] seam (issue #11); the codex
//! app-server is the MVP impl under `app_server`. `session` (state machine) and
//! `events` (notification → [`crate::events::ReviewEvent`] mapping) are filled
//! in by PR5/PR6.

pub mod app_server;
pub mod engine;
pub mod events;
pub mod session;
