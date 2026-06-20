//! Review slice: drive a review engine (codex app-server MVP) to run the pr-review skill,
//! stream output to the UI, and stop sessions.
//!
//! `engine` defines the [`engine::ReviewEngine`] seam (issue #11); the codex
//! app-server is the MVP impl under [`engines::codex`]. `session` holds the
//! session state machine and the notification → [`crate::events::ReviewEvent`]
//! mapping (the pump task).

pub mod commands;
pub mod engine;
pub mod engines;
pub mod history_store;
pub mod session;
