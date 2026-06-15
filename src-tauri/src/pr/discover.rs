//! Gating + dedup — pure port of `router.py`'s `should_skip` /
//! `recent_dispatch_reason` / `live_gate_skip`.
//!
//! These are pure predicates over a [`Candidate`], the [`MonitorParams`] config
//! snapshot, the [`Ledger`], and a clock, so the discovery semantics are unit-
//! tested without `gh`. The `gh` subprocess shells live in [`super::gh`]; the
//! `discover_views` body in [`super::commands`] composes them, driven by the
//! scheduler's poll loop (and the `poll_now` manual trigger).

use crate::model::Candidate;

use super::ledger::{cooldown_remaining, dispatch_key, Ledger};

/// Skip reason for a PR carrying BOTH trigger labels (ambiguous mode). Single
/// source shared by the live gate here and the discovery-stage drop in
/// [`super::commands`] so the two call sites cannot drift.
pub const BOTH_TRIGGER_LABELS_REASON: &str = "both review and check trigger labels are present";

/// The config the gating needs, snapshotted from `AppConfig` by the command.
///
/// The `pr` slice stays self-contained: it does not import the config slice's
/// `AppConfig`; the command maps the fields it needs into this pr-local struct.
#[derive(Debug, Clone)]
pub struct MonitorParams {
    pub repo: String,
    pub review_label: String,
    pub check_label: String,
    pub authors: Vec<String>,
    pub pr_cooldown_seconds: u64,
}

/// Static-gate skip reason (no live `gh` call): cross-repository → draft →
/// author-not-in-allowlist → already-dispatched. Order mirrors `router.py`
/// `should_skip`.
///
/// **Divergence from `router.py`**: the dispatcher *requires* a non-empty author
/// allowlist and skips anyone outside it. prmonitor's `AppConfig` documents an
/// empty `authors` as "no author gate" (the default), so an empty allowlist
/// admits every author; a non-empty one gates exactly like `router.py`.
pub fn should_skip(cand: &Candidate, params: &MonitorParams, ledger: &Ledger) -> Option<String> {
    if cand.is_cross_repository {
        return Some("cross-repository PR".to_string());
    }
    if cand.is_draft {
        return Some("draft PR".to_string());
    }
    if !params.authors.is_empty() && !params.authors.iter().any(|a| a == &cand.author) {
        return Some(format!("author {:?} not in allowlist", cand.author));
    }
    let key = dispatch_key(cand.number, &cand.head_sha, &cand.kind);
    if ledger.has_dispatched(&key) {
        return Some(format!("already dispatched key {key}"));
    }
    None
}

/// Cooldown skip reason: the same `(pr, kind)` dispatched within
/// `pr_cooldown_seconds`. Mirrors `router.py` `recent_dispatch_reason` — a zero
/// cooldown disables the gate.
pub fn cooldown_skip(
    cand: &Candidate,
    params: &MonitorParams,
    ledger: &Ledger,
    now: u64,
) -> Option<String> {
    if params.pr_cooldown_seconds == 0 {
        return None;
    }
    let last = ledger.last_dispatch_at(cand.number, &cand.kind)?;
    let remaining = cooldown_remaining(now, last, params.pr_cooldown_seconds)?;
    Some(format!(
        "recent {} dispatch within cooldown ({remaining}s remaining)",
        cand.kind
    ))
}

/// Live-state re-validation (`router.py` `live_gate_skip`) — a fresh-`gh-pr-view`
/// gate: head moved → draft → trigger label removed → both labels present.
///
/// **Currently reserved / unused in the dispatch path.** #8 deliberately declined
/// the live gate: the dispatcher trusts the just-fetched discovery list as the
/// live state rather than spending a second `gh pr view` per PR per poll. This fn
/// (and its tests) is kept for a future webhook real-time trigger, where a push
/// event would re-validate one PR's live state before starting a turn.
pub fn live_gate_skip(
    cand: &Candidate,
    params: &MonitorParams,
    live_head: &str,
    live_is_draft: bool,
    live_labels: &[String],
) -> Option<String> {
    if live_head != cand.head_sha {
        return Some(format!(
            "head moved (listed={} live={})",
            short_sha(&cand.head_sha),
            short_sha(live_head)
        ));
    }
    if live_is_draft {
        return Some("draft PR".to_string());
    }
    let (want, other) = if cand.kind == "review" {
        (&params.review_label, &params.check_label)
    } else {
        (&params.check_label, &params.review_label)
    };
    if !live_labels.iter().any(|l| l == want) {
        return Some(format!("trigger label {want:?} no longer present"));
    }
    if live_labels.iter().any(|l| l == other) {
        return Some(BOTH_TRIGGER_LABELS_REASON.to_string());
    }
    None
}

/// First 12 chars of a sha for compact "head moved" messages (matches
/// `router.py`'s `[:12]` slice).
fn short_sha(sha: &str) -> String {
    sha.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr::ledger::DispatchEvent;
    use std::collections::HashSet;

    fn params() -> MonitorParams {
        MonitorParams {
            repo: "o/r".to_string(),
            review_label: "pr-status/needs-review-again".to_string(),
            check_label: "pr-status/needs-check-fix".to_string(),
            authors: vec![],
            pr_cooldown_seconds: 1800,
        }
    }

    fn cand(kind: &str) -> Candidate {
        Candidate {
            number: 7,
            head_sha: "abc123def456".to_string(),
            head_ref: "feature/x".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            kind: kind.to_string(),
        }
    }

    #[test]
    fn should_skip_passes_clean_pr_with_empty_allowlist() {
        // Empty authors = no gate (prmonitor adaptation) → clean PR not skipped.
        assert_eq!(
            should_skip(&cand("review"), &params(), &Ledger::default()),
            None
        );
    }

    #[test]
    fn should_skip_order_cross_repo_first() {
        let mut c = cand("review");
        c.is_cross_repository = true;
        c.is_draft = true; // cross-repo wins over draft (checked first)
        assert_eq!(
            should_skip(&c, &params(), &Ledger::default()),
            Some("cross-repository PR".to_string())
        );
    }

    #[test]
    fn should_skip_draft() {
        let mut c = cand("review");
        c.is_draft = true;
        assert_eq!(
            should_skip(&c, &params(), &Ledger::default()),
            Some("draft PR".to_string())
        );
    }

    #[test]
    fn should_skip_author_allowlist() {
        let mut p = params();
        p.authors = vec!["alice".to_string(), "bob".to_string()];
        // octocat not in allowlist → skip.
        assert_eq!(
            should_skip(&cand("review"), &p, &Ledger::default()),
            Some("author \"octocat\" not in allowlist".to_string())
        );
        // author in allowlist → pass.
        let mut c = cand("review");
        c.author = "bob".to_string();
        assert_eq!(should_skip(&c, &p, &Ledger::default()), None);
    }

    #[test]
    fn should_skip_already_dispatched() {
        let c = cand("review");
        let key = dispatch_key(c.number, &c.head_sha, &c.kind);
        let ledger = Ledger {
            dispatched: HashSet::from([key.clone()]),
            events: vec![],
        };
        assert_eq!(
            should_skip(&c, &params(), &ledger),
            Some(format!("already dispatched key {key}"))
        );
    }

    #[test]
    fn cooldown_skip_within_window() {
        let c = cand("review");
        let ledger = Ledger {
            dispatched: HashSet::new(),
            events: vec![DispatchEvent {
                pr: c.number,
                kind: "review".to_string(),
                head_sha: c.head_sha.clone(),
                key: dispatch_key(c.number, &c.head_sha, "review"),
                dispatched_at_epoch: 1_000,
            }],
        };
        // 1800s cooldown, dispatched 600s before now → within window.
        let reason = cooldown_skip(&c, &params(), &ledger, 1_600).unwrap();
        assert!(reason.contains("within cooldown"), "{reason}");
        assert!(reason.contains("1200s remaining"), "{reason}");
    }

    #[test]
    fn cooldown_skip_elapsed_and_zero_and_missing() {
        let c = cand("review");
        let ledger = Ledger {
            dispatched: HashSet::new(),
            events: vec![DispatchEvent {
                pr: c.number,
                kind: "review".to_string(),
                head_sha: c.head_sha.clone(),
                key: dispatch_key(c.number, &c.head_sha, "review"),
                dispatched_at_epoch: 1_000,
            }],
        };
        // elapsed: now well past cooldown.
        assert_eq!(cooldown_skip(&c, &params(), &ledger, 5_000), None);
        // zero cooldown disables the gate even within window.
        let mut p = params();
        p.pr_cooldown_seconds = 0;
        assert_eq!(cooldown_skip(&c, &p, &ledger, 1_100), None);
        // no matching event (different kind) → no cooldown.
        assert_eq!(
            cooldown_skip(&cand("check"), &params(), &ledger, 1_100),
            None
        );
    }

    #[test]
    fn live_gate_skip_head_moved() {
        let c = cand("review");
        let labels = vec![params().review_label];
        let reason = live_gate_skip(&c, &params(), "999999999999zzz", false, &labels).unwrap();
        assert!(reason.starts_with("head moved"), "{reason}");
    }

    #[test]
    fn live_gate_skip_draft_and_label_states() {
        let c = cand("review");
        let p = params();
        // draft now.
        assert_eq!(
            live_gate_skip(
                &c,
                &p,
                &c.head_sha,
                true,
                std::slice::from_ref(&p.review_label)
            ),
            Some("draft PR".to_string())
        );
        // trigger label gone.
        assert_eq!(
            live_gate_skip(&c, &p, &c.head_sha, false, &["other".to_string()]),
            Some(format!(
                "trigger label {:?} no longer present",
                p.review_label
            ))
        );
        // both labels present.
        assert_eq!(
            live_gate_skip(
                &c,
                &p,
                &c.head_sha,
                false,
                &[p.review_label.clone(), p.check_label.clone()]
            ),
            Some("both review and check trigger labels are present".to_string())
        );
        // clean live state → pass.
        assert_eq!(
            live_gate_skip(
                &c,
                &p,
                &c.head_sha,
                false,
                std::slice::from_ref(&p.review_label)
            ),
            None
        );
    }

    #[test]
    fn live_gate_skip_check_kind_uses_check_label() {
        let c = cand("check");
        let p = params();
        // check label present, review absent → pass.
        assert_eq!(
            live_gate_skip(
                &c,
                &p,
                &c.head_sha,
                false,
                std::slice::from_ref(&p.check_label)
            ),
            None
        );
        // check label gone → skip (the `else` branch wires `want` to check_label).
        assert_eq!(
            live_gate_skip(&c, &p, &c.head_sha, false, &["other".to_string()]),
            Some(format!(
                "trigger label {:?} no longer present",
                p.check_label
            ))
        );
        // both labels present → conflict.
        assert_eq!(
            live_gate_skip(
                &c,
                &p,
                &c.head_sha,
                false,
                &[p.check_label.clone(), p.review_label.clone()]
            ),
            Some(BOTH_TRIGGER_LABELS_REASON.to_string())
        );
    }
}
