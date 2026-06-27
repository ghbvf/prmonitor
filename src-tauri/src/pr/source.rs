//! `EventSourceProvider` — the normalized-Event source seam (AB#1070).
//!
//! A connector (the `gh` / `az` CLI, the Bitbucket REST API, a future webhook
//! receiver) implements this trait to pull/produce the cross-slice AB#1079
//! [`crate::model::Event`] envelope directly, rather than a source-specific shape.
//! The scheduler and gating logic depend only on the trait, never a concrete source.
//!
//! Issue history: started as `PrSource` (issue #11), whose trait method returned
//! `Vec<Candidate>` (gating-only). AB#1070 generalizes the seam to produce the
//! normalized `Event` so discovery feeds the event pipeline (epic AB#1078) directly —
//! WITHOUT losing the review-gating `Candidate`, which rides along on
//! [`DiscoveredEvent`].

use crate::error::AppResult;

/// One discovered item from an [`EventSourceProvider`]: the normalized AB#1079
/// [`crate::model::Event`] (for the event pipeline) PLUS the review-gating
/// [`crate::model::Candidate`] it derives from, and the both-trigger-label
/// `conflict` flag the view path turns into a skip reason.
///
/// Backend-internal glue (NOT serialized to the frontend, like the per-source row
/// structs `GhRow` / `AzRow` / `BbRow`): it is `pr`-slice-internal and lives here
/// only to carry both the `Event` and the `Candidate` out of `discover_events`
/// without flattening one into the other. The cross-slice contract types it wraps
/// (`Event` / `Candidate`) live in `crate::model`.
#[derive(Clone)]
pub struct DiscoveredEvent {
    pub event: crate::model::Event,
    pub candidate: crate::model::Candidate,
    pub conflict: bool,
}

/// A source of normalized inbound [`crate::model::Event`]s that may need review.
#[allow(async_fn_in_trait)]
pub trait EventSourceProvider {
    /// Discover open PRs carrying the configured trigger labels as normalized
    /// [`DiscoveredEvent`]s (`Event` + gating `Candidate`). The source produces the
    /// content envelope; the `Event`'s ingress-context fields (`project_id` /
    /// `received_at_epoch`) are left at zero/empty values for the future inbox
    /// (AB#1065) to stamp on ingest — the source can't know the matched project id
    /// or the receive time, and no consumer reads them until the inbox lands.
    async fn discover_events(&self) -> AppResult<Vec<DiscoveredEvent>>;
}

/// Compose the discovery-stable dedupe key for a PR [`crate::model::Event`] (AB#1070):
/// `"{source_wire}:pullRequest:{repo}#{number}@{head_sha}"`. Single-sourced here so the three
/// sources can't drift on the format; `source_wire` is the source's [`crate::model::SourceKind`]
/// serde string (pinned by `model.rs`'s discriminator golden). The exact key is the inbox's
/// (AB#1065) to finalize — this is the discovery-time seed, stable for the same PR head.
pub(crate) fn pr_dedupe_key(source_wire: &str, repo: &str, number: u64, head_sha: &str) -> String {
    format!("{source_wire}:pullRequest:{repo}#{number}@{head_sha}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_dedupe_key_pins_the_format() {
        assert_eq!(
            pr_dedupe_key("github", "o/r", 12, "abc123"),
            "github:pullRequest:o/r#12@abc123"
        );
    }
}
