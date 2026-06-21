//! Shared label resolution + trigger classification for every PR source (AB#717).
//!
//! A project's effective labels come from one of two places ([`LabelSource`]):
//! the provider's own PR labels (`Native`), or bracketed segments parsed out of the
//! PR title (`Title`, e.g. `[pr-status/need-fix]`). Bitbucket Server has no native
//! PR labels, so it always uses `Title`; GitHub / Azure can use either.
//!
//! [`classify`] is the single home for the review/check/conflict decision that gh.rs
//! and azure.rs previously each open-coded — both now feed their effective labels here.

use crate::model::LabelSource;

/// Extracts bracketed label segments from a PR title, in order, de-duplicated.
///
/// Each `[...]` group's inner text is trimmed; empty groups are dropped. Matching is
/// simple (non-nested): the first `[` pairs with the next `]`. A trailing unclosed `[`
/// stops the scan. `[` / `]` are ASCII, so slicing stays on UTF-8 char boundaries.
///
/// `"Fix login [pr-status/need-fix][wip]"` → `["pr-status/need-fix", "wip"]`.
pub fn parse_title_labels(title: &str) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    let mut rest = title;
    while let Some(open) = rest.find('[') {
        let after_open = &rest[open + 1..];
        match after_open.find(']') {
            Some(close) => {
                let content = after_open[..close].trim();
                if !content.is_empty() && !labels.iter().any(|l| l == content) {
                    labels.push(content.to_string());
                }
                rest = &after_open[close + 1..];
            }
            // Unclosed '[' — nothing more to extract.
            None => break,
        }
    }
    labels
}

/// Resolves a PR's effective labels from its native labels + title, per the project's
/// [`LabelSource`]. Exhaustive over `LabelSource` (Hard carrier): a new variant must
/// be handled here or it fails to compile.
pub fn effective_labels(native: Vec<String>, title: &str, source: LabelSource) -> Vec<String> {
    match source {
        LabelSource::Native => native,
        LabelSource::Title => parse_title_labels(title),
    }
}

/// Classifies a PR by its (already effective) labels against the project's trigger
/// labels, returning `(kind, conflict)` or `None` when the PR carries neither trigger
/// label (not monitored — dropped).
///
/// Mirrors the long-standing gh `merge_rows` / azure `parse_rows` semantics, now in one
/// place: both labels → `("review", true)` (kept with the review row, marked conflict);
/// only review → `("review", false)`; only check → `("check", false)`; neither → `None`.
pub fn classify(
    labels: &[String],
    review_label: &str,
    check_label: &str,
) -> Option<(&'static str, bool)> {
    let has_review = labels.iter().any(|n| n == review_label);
    let has_check = labels.iter().any(|n| n == check_label);
    match (has_review, has_check) {
        (true, true) => Some(("review", true)),
        (true, false) => Some(("review", false)),
        (false, true) => Some(("check", false)),
        (false, false) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_title_labels_single_segment() {
        assert_eq!(
            parse_title_labels("Fix login [pr-status/need-fix]"),
            v(&["pr-status/need-fix"])
        );
    }

    #[test]
    fn parse_title_labels_multiple_segments_in_order() {
        assert_eq!(
            parse_title_labels("[pr-status/need-fix][wip] Add widget"),
            v(&["pr-status/need-fix", "wip"])
        );
    }

    #[test]
    fn parse_title_labels_trims_and_dedups_preserving_order() {
        assert_eq!(parse_title_labels("[ a ] foo [b] bar [a]"), v(&["a", "b"]));
    }

    #[test]
    fn parse_title_labels_drops_empty_groups() {
        assert_eq!(parse_title_labels("[] [  ] [b]"), v(&["b"]));
    }

    #[test]
    fn parse_title_labels_no_brackets_is_empty() {
        assert!(parse_title_labels("plain title, no tags").is_empty());
    }

    #[test]
    fn parse_title_labels_unclosed_bracket_stops() {
        assert_eq!(parse_title_labels("[ok] then [oops"), v(&["ok"]));
    }

    #[test]
    fn parse_title_labels_is_utf8_safe() {
        // Non-ASCII text around / inside brackets must not panic on slicing.
        assert_eq!(parse_title_labels("修复登录 [需修复] 完成"), v(&["需修复"]));
    }

    #[test]
    fn effective_labels_native_returns_provider_labels() {
        let native = v(&["area/ui", "bug"]);
        assert_eq!(
            effective_labels(native.clone(), "[ignored]", LabelSource::Native),
            native
        );
    }

    #[test]
    fn effective_labels_title_parses_title_ignoring_native() {
        assert_eq!(
            effective_labels(v(&["area/ui"]), "Add [check] thing", LabelSource::Title),
            v(&["check"])
        );
    }

    #[test]
    fn classify_quadrants() {
        let review = "pr-status/needs-review";
        let check = "pr-status/need-fix";
        assert_eq!(
            classify(&v(&[review, check]), review, check),
            Some(("review", true))
        );
        assert_eq!(
            classify(&v(&[review]), review, check),
            Some(("review", false))
        );
        assert_eq!(
            classify(&v(&[check]), review, check),
            Some(("check", false))
        );
        assert_eq!(classify(&v(&["area/ui"]), review, check), None);
        assert_eq!(classify(&[], review, check), None);
    }
}
