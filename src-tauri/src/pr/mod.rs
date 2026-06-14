//! PR-monitoring slice: discover PRs from a source, gate, de-dup, and schedule.
//!
//! `source` defines the [`source::PrSource`] seam (issue #11); GitHub via the
//! `gh` CLI is the MVP impl. PR3 fills `gh` (source and JSON parse), `discover`
//! (gating and dedup, a port of `router.py`), `ledger` (dedup keys and
//! cooldown), and `commands` (the `fetch_prs_now` and `gh_status` Tauri
//! commands). PR4 fills `scheduler`.

pub mod commands;
pub mod discover;
pub mod gh;
pub mod ledger;
pub mod scheduler;
pub mod source;
