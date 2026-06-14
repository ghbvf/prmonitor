//! `PrSource` — the extensibility seam for PR/MR data sources (issue #11).
//!
//! GitHub via the `gh` CLI is the MVP impl (`super::gh`). GitLab, Bitbucket, and
//! a future webhook receiver plug in by implementing this trait; the scheduler
//! and gating logic depend only on the trait, never a concrete source.

use crate::error::AppResult;
use crate::model::Candidate;

/// A source of open PRs/MRs that may need review.
#[allow(async_fn_in_trait)]
pub trait PrSource {
    /// Discover open PRs carrying the configured trigger labels.
    async fn discover(&self) -> AppResult<Vec<Candidate>>;
}
