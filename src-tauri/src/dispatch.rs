//! Default auto-dispatch producer (#1379).
//!
//! Auto triggers no longer start review engines inline. They produce durable action-outbox rows:
//! `Candidate` → `ReviewActionPayload` → `ActionKind::Review` / `ActionKind::Check`. The outbox
//! worker is the only executor and owns retry / restart-resume.

use crate::error::{AppError, AppResult};
use crate::model::{ActionKind, Candidate, ReviewActionPayload};

type AutoDispatchEnqueue<'a> =
    dyn Fn(&Candidate, ActionKind, &str, &str, &str) -> AppResult<i64> + Send + Sync + 'a;

/// Produce review/check actions for dispatchable candidates.
///
/// The active snapshot remains a cheap pre-filter: a candidate already covered by a live/reserved
/// `(pr, kind)` does not need a new action. Correctness is enforced downstream by the outbox pending
/// dedupe key and the review executor's durable claim.
pub fn auto_dispatch(
    candidates: Vec<Candidate>,
    active: &[(u64, String)],
    enqueue: &AutoDispatchEnqueue<'_>,
    report_error: &(dyn Fn(String) + Send + Sync),
) {
    let candidates = dispatchable_after_guard(candidates, active);
    if candidates.is_empty() {
        return;
    }

    let mut failures = Vec::new();
    for cand in candidates {
        let kind = match action_kind_for_candidate(&cand) {
            Ok(kind) => kind,
            Err(e) => {
                failures.push(format!("PR #{} {} ({})", cand.number, cand.kind, e.message));
                continue;
            }
        };
        let payload = match serde_json::to_string(&ReviewActionPayload {
            candidate: cand.clone(),
        }) {
            Ok(payload) => payload,
            Err(e) => {
                failures.push(format!("PR #{} {} (payload: {e})", cand.number, cand.kind));
                continue;
            }
        };
        let summary = review_action_summary(&cand);
        let dedupe_key = review_action_dedupe_key(&cand);
        if let Err(e) = enqueue(&cand, kind, &summary, &payload, &dedupe_key) {
            eprintln!(
                "auto-dispatch 入队 review action 失败（PR {} {}）：{}",
                cand.number, cand.kind, e.message
            );
            failures.push(format!("PR #{} {}", cand.number, cand.kind));
        }
    }

    if !failures.is_empty() {
        report_error(format!(
            "{} 个 review action 入队失败：{}",
            failures.len(),
            failures.join("、")
        ));
    }
}

pub(crate) fn action_kind_for_candidate(cand: &Candidate) -> AppResult<ActionKind> {
    match cand.kind.as_str() {
        "review" => Ok(ActionKind::Review),
        "check" => Ok(ActionKind::Check),
        other => Err(AppError::new(format!("未知自动派发类型：{other}"))),
    }
}

pub(crate) fn review_action_summary(cand: &Candidate) -> String {
    format!("PR #{} {}", cand.number, cand.kind)
}

pub(crate) fn review_action_dedupe_key(cand: &Candidate) -> String {
    crate::pr::ledger::dispatch_key(cand.number, &cand.head_sha, &cand.kind)
}

fn dispatchable_after_guard(
    candidates: Vec<Candidate>,
    active: &[(u64, String)],
) -> Vec<Candidate> {
    candidates
        .into_iter()
        .filter(|c| {
            !active
                .iter()
                .any(|(pr, kind)| *pr == c.number && kind == &c.kind)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn cand(number: u64, kind: &str) -> Candidate {
        Candidate {
            number,
            head_sha: "sha".to_string(),
            head_ref: "ref".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            kind: kind.to_string(),
        }
    }

    fn pair(pr: u64, kind: &str) -> (u64, String) {
        (pr, kind.to_string())
    }

    #[test]
    fn guard_drops_candidate_with_active_same_kind() {
        let candidates = vec![cand(1, "review"), cand(2, "check")];
        let kept = dispatchable_after_guard(candidates, &[pair(1, "review")]);
        let nums: Vec<u64> = kept.iter().map(|c| c.number).collect();
        assert_eq!(nums, vec![2]);
    }

    #[test]
    fn guard_keeps_candidate_of_different_kind() {
        let candidates = vec![cand(1, "check")];
        let kept = dispatchable_after_guard(candidates, &[pair(1, "review")]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].kind, "check");
    }

    #[test]
    fn auto_dispatch_enqueues_review_and_check_actions_with_dedupe_keys() {
        let calls: Mutex<Vec<(u64, ActionKind, String, String)>> = Mutex::new(Vec::new());
        let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let enqueue = |cand: &Candidate, kind, _summary: &str, payload: &str, dedupe_key: &str| {
            let payload: ReviewActionPayload =
                serde_json::from_str(payload).expect("payload parses");
            assert_eq!(payload.candidate.number, cand.number);
            calls.lock().unwrap().push((
                cand.number,
                kind,
                cand.kind.clone(),
                dedupe_key.to_string(),
            ));
            Ok(1)
        };
        let report = |msg: String| errors.lock().unwrap().push(msg);

        auto_dispatch(
            vec![cand(1, "review"), cand(2, "check")],
            &[],
            &enqueue,
            &report,
        );

        let calls = calls.lock().unwrap();
        assert_eq!(
            *calls,
            vec![
                (
                    1,
                    ActionKind::Review,
                    "review".to_string(),
                    "1@sha:review".to_string()
                ),
                (
                    2,
                    ActionKind::Check,
                    "check".to_string(),
                    "2@sha:check".to_string()
                )
            ]
        );
        assert!(errors.lock().unwrap().is_empty());
    }

    #[test]
    fn auto_dispatch_reports_enqueue_failures() {
        let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let enqueue = |_cand: &Candidate, _kind, _summary: &str, _payload: &str, _key: &str| {
            Err(AppError::new("boom"))
        };
        let report = |msg: String| errors.lock().unwrap().push(msg);

        auto_dispatch(vec![cand(3, "review")], &[], &enqueue, &report);

        let errors = errors.lock().unwrap();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("#3"));
    }

    #[test]
    fn app_code_uses_no_gh_write_subcommands() {
        let forbidden: Vec<String> = vec![
            format!("pr ed{}", "it"),
            format!("pr com{}", "ment"),
            format!("pr rev{}", "iew"),
            format!("issue com{}", "ment"),
            format!("issue ed{}", "it"),
            format!("--add-l{}", "abel"),
            format!("--remove-l{}", "abel"),
        ];
        let src_root = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

        let mut offenders = Vec::new();
        for path in rs_files_under(std::path::Path::new(src_root)) {
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            for pat in &forbidden {
                if src.contains(pat.as_str()) {
                    offenders.push(format!(
                        "{} contains gh write pattern {:?}",
                        path.display(),
                        pat
                    ));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "app-side gh WRITE detected (only the pr-review skill may write to GitHub):\n{}",
            offenders.join("\n")
        );
    }

    fn rs_files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                out.extend(rs_files_under(&path));
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                out.push(path);
            }
        }
        out
    }
}
