//! Review slice: drive a review engine (codex app-server MVP) to run the pr-review skill,
//! stream output to the UI, and stop sessions.
//!
//! `engine` defines the [`engine::ReviewEngine`] seam (issue #11); the codex
//! app-server is the MVP impl under [`engines::codex`]. `session` (state machine) and
//! `events` (notification → [`crate::events::ReviewEvent`] mapping) are filled
//! in by PR5/PR6.

pub mod commands;
pub mod engine;
pub mod engines;
pub mod events;
pub mod session;
