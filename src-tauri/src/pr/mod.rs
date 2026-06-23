//! PR-monitoring slice: discover PRs from a source, gate, de-dup, and schedule.
//!
//! `source` defines the [`source::EventSourceProvider`] seam (issue #11, AB#1070);
//! GitHub via the `gh` CLI is the MVP impl. `gh` (source and JSON parse), `discover` (gating
//! and dedup, a port of `router.py`), and `ledger` (dedup keys and cooldown)
//! supply the discovery body. `scheduler` drives that body on a poll loop;
//! `commands` exposes the `poll_now` / `start_polling` / `stop_polling` /
//! `reschedule` / `gh_status` Tauri commands.

pub mod azure;
pub mod bitbucket;
pub mod commands;
pub mod discover;
pub mod gh;
pub mod labels;
pub mod ledger;
pub mod registry;
pub mod scheduler;
pub mod source;
pub mod webhook;
