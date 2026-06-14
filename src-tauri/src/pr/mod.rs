//! PR-monitoring slice: discover PRs from a source, gate, de-dup, and schedule.
//!
//! `source` defines the [`source::PrSource`] seam (issue #11); GitHub via the
//! `gh` CLI is the MVP impl. `gh` / `ledger` / `scheduler` are filled in by
//! PR3 (discovery + gating + dedup) and PR4 (scheduler).

pub mod gh;
pub mod ledger;
pub mod scheduler;
pub mod source;
