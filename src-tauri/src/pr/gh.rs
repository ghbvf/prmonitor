//! GitHub [`super::source::PrSource`] implementation via the `gh` CLI.
//!
//! Implemented in PR3: `gh pr list --repo <repo> --state open --label <label>
//! --json ...` plus the discovery/gating/dedup port of `router.py`.
