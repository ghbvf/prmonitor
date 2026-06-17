//! Auto-trigger dispatch — turns a cycle's dispatchable [`Candidate`]s into running
//! reviews against an injected [`ReviewEngine`].
//!
//! **Engine-agnostic by construction.** This module names NO concrete engine and
//! reaches into NO slice's internals: it depends only on the [`ReviewEngine`] trait
//! (the extensibility seam reserved by #11) plus the [`Candidate`] cross-slice
//! contract. The composition root (`lib.rs`) picks the concrete engine — codex
//! today, a future Claude engine — and injects it together with the registry's
//! active-session snapshot, the ledger recorder, and the UI error reporter. Adding
//! an engine therefore never edits this file: it implements [`ReviewEngine`] and the
//! root wires it in. This is what closes the "composition glue must not bypass the
//! trait seam" gap (PR #31 finding F1).
//!
//! **Reused by multiple trigger sources.** The scheduler drives it today (via the
//! `pr` slice's review-agnostic [`crate::pr::scheduler::Dispatcher`] hook); a future
//! webhook trigger calls the same [`auto_dispatch`] with the candidates a push event
//! yields.
//!
//! **The app writes NO labels / comments.** Its only side effect is *starting*
//! reviews (and recording the dedup ledger, via the injected recorder). All GitHub
//! writes — the `pm:` review comment and the status-label transition — are done by
//! the codex pr-review skill the started turn runs, never by app code. The app's own
//! `gh` surface stays read-only (`gh pr list` / `gh auth status`); the Medium guard
//! test below scans ALL of `src` so no write subcommand creeps in anywhere.

use crate::error::AppResult;
use crate::model::Candidate;
use crate::review::engine::{ReviewEngine, StartReviewOutcome};

/// Auto-start reviews for a cycle's dispatchable candidates against `engine`,
/// concurrently and unbounded, then land the dedup ledger for the ones that started.
///
/// Every slice-specific capability is injected so this stays engine-agnostic:
/// - `engine` — the [`ReviewEngine`] the composition root chose (codex / future).
/// - `active` — the `(pr, kind)` pairs already covered by an in-flight session (the
///   registry guard); the review slice computes this so [`SessionStatus`] never
///   leaks here. (`SessionStatus` lives in `crate::review::session`.)
/// - `record` — lands the started candidates in the dedup ledger (pr slice).
/// - `report_error` — surfaces a session-less failure notice to the UI.
///
/// Flow: registry guard → unbounded concurrent `engine.start` (`join_all`) → batched
/// ledger landing. A failed start stays unrecorded (retried next cycle); a ledger
/// write failure is reported, never propagated — it must not crash the poll loop.
pub async fn auto_dispatch<E: ReviewEngine>(
    candidates: Vec<Candidate>,
    engine: &E,
    active: &[(u64, String)],
    // `Send + Sync`: these are held across the concurrent-start `.await`, and the
    // scheduler boxes this future as `Send` (the `Dispatcher` hook). The composition
    // root's closures capture only `&AppHandle` (itself `Send + Sync`), so they fit.
    record: &(dyn Fn(&[Candidate]) -> AppResult<()> + Send + Sync),
    report_error: &(dyn Fn(String) + Send + Sync),
) {
    // Registry guard: a cheap PRE-FILTER dropping candidates a snapshot already shows
    // in flight (or reserved), so we don't even attempt an obviously-redundant start.
    // It is NO LONGER the correctness boundary — `engine.start` reserves `(pr, kind)`
    // atomically (`Ok(None)` if it loses), which closes the within/overlapping-cycle
    // race a stale snapshot can't (two concurrent dispatches both passing this filter).
    let candidates = dispatchable_after_guard(candidates, active);
    if candidates.is_empty() {
        return;
    }

    // Unbounded concurrent start, one task, all sharing `&engine`. Unbounded is the
    // chosen design and is transport-safe: the codex `RpcClient` serializes every
    // write through `Arc<tokio::sync::Mutex<W>>`, so concurrent starts can't
    // interleave bytes on the engine's stdin.
    let starts = candidates.iter().map(|c| engine.start(c.number, &c.kind));
    let results = futures::future::join_all(starts).await;

    // Record only the candidates whose start succeeded (a failure retries next
    // cycle). Failed starts aggregate into ONE UI notice; per-start lines stay in
    // logs (so N failures don't fan out into N banner events).
    let mut succeeded: Vec<Candidate> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for (cand, result) in candidates.into_iter().zip(results) {
        match result {
            // Started → record it in the dedup ledger.
            Ok(StartReviewOutcome::Started(_session_id)) => succeeded.push(cand),
            // Deduped by the registry reservation (a concurrent start already owns this
            // `(pr, kind)`): NOT started, so nothing to record, and NOT a failure — no
            // error banner. This is the race the snapshot guard could miss, now caught
            // atomically downstream; here it is simply a silent skip.
            Ok(StartReviewOutcome::Deduped) => {}
            Err(e) => {
                eprintln!(
                    "auto-dispatch 启动 review 失败（PR {} {}）：{}",
                    cand.number, cand.kind, e.message
                );
                failures.push(format!("PR #{} {}", cand.number, cand.kind));
            }
        }
    }
    if !failures.is_empty() {
        report_error(format!(
            "{} 个 review 启动失败：{}",
            failures.len(),
            failures.join("、")
        ));
    }
    if !succeeded.is_empty() {
        // A persist failure leaves the started sessions UNRECORDED: the in-process
        // registry guard still blocks a duplicate while each session lives, but a
        // cross-restart re-dispatch becomes possible if the store write failed
        // (rare). Reported to the UI, never propagated. (A transactional / recoverable
        // dedup — write-ahead + restart reconciliation — is tracked as a follow-up.)
        if let Err(e) = record(&succeeded) {
            eprintln!("auto-dispatch 写入 ledger 失败：{}", e.message);
            report_error(format!(
                "ledger 落账失败（重启后可能重复派发）：{}",
                e.message
            ));
        }
    }
}

/// Drop candidates already covered by an active session (the registry guard).
/// `active` is the `(pr, kind)` set the caller computed from the live registry; a
/// candidate matching one is in flight and must not start a second review. Pure over
/// the candidate list + the pair set, so it is unit-tested without an app or any
/// slice — the dedup key is `(pr, kind)`, not `pr` (a `review` and a `check` for the
/// same PR are independent).
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
        // PR 1 review is in flight → dropped; PR 2 check has no active session.
        let active = [pair(1, "review")];
        let kept = dispatchable_after_guard(candidates, &active);
        let nums: Vec<u64> = kept.iter().map(|c| c.number).collect();
        assert_eq!(nums, vec![2]);
    }

    #[test]
    fn guard_keeps_candidate_of_different_kind() {
        // An active `review` must NOT block a `check` for the same PR — key is
        // `(pr, kind)`, not `pr`.
        let candidates = vec![cand(1, "check")];
        let active = [pair(1, "review")];
        let kept = dispatchable_after_guard(candidates, &active);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].kind, "check");
    }

    #[test]
    fn guard_no_active_keeps_all() {
        let candidates = vec![cand(1, "review"), cand(2, "check")];
        let kept = dispatchable_after_guard(candidates, &[]);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn guard_filters_across_multiple_active_and_candidates() {
        // PR1/review and PR2/review are active; PR1/check is not → only PR1/check
        // survives (per-pair filtering).
        let candidates = vec![cand(1, "review"), cand(1, "check"), cand(2, "review")];
        let active = [pair(1, "review"), pair(2, "review")];
        let kept = dispatchable_after_guard(candidates, &active);
        let kv: Vec<(u64, &str)> = kept.iter().map(|c| (c.number, c.kind.as_str())).collect();
        assert_eq!(kv, vec![(1, "check")]);
    }

    /// Fake engine encoding the three `start` outcomes by PR number: PR1 starts
    /// (`Started`), PR2 is deduped (`Deduped`), PR3+ fails (`Err`).
    struct FakeEngine;

    impl ReviewEngine for FakeEngine {
        async fn start(&self, pr_number: u64, _kind: &str) -> AppResult<StartReviewOutcome> {
            match pr_number {
                1 => Ok(StartReviewOutcome::Started("session-1".to_string())),
                2 => Ok(StartReviewOutcome::Deduped),
                _ => Err(crate::error::AppError::new("boom".to_string())),
            }
        }
        async fn stop(&self, _session: &String) -> AppResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn auto_dispatch_records_started_skips_deduped_reports_failed() {
        use std::sync::Mutex;
        // Locks the `Deduped` arm: a deduped candidate is neither recorded in the
        // ledger nor reported as a start failure — distinct from both `Started` and
        // `Err`. Without the dedicated arm, a dedup would fall into the failure banner.
        let candidates = vec![cand(1, "review"), cand(2, "review"), cand(3, "review")];
        let recorded: Mutex<Vec<u64>> = Mutex::new(Vec::new());
        let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let record = |cands: &[Candidate]| {
            recorded
                .lock()
                .unwrap()
                .extend(cands.iter().map(|c| c.number));
            Ok(())
        };
        let report = |msg: String| errors.lock().unwrap().push(msg);

        auto_dispatch(candidates, &FakeEngine, &[], &record, &report).await;

        // Only PR1 (Started) recorded; PR2 (Deduped) and PR3 (Err) not.
        assert_eq!(*recorded.lock().unwrap(), vec![1]);
        // PR3 failure reported; the PR2 dedup is NOT a failure (no banner for it).
        let errs = errors.lock().unwrap();
        assert_eq!(errs.len(), 1, "exactly one failure banner");
        assert!(errs[0].contains("#3"), "names the failed PR: {}", errs[0]);
        assert!(
            !errs[0].contains("#2"),
            "dedup must not be reported as a failure"
        );
    }

    // ── Governance: "the app writes NO labels/comments" (Medium carrier) ────────
    //
    // The acceptance invariant per `.claude/rules/prmonitor/ai-robust.md`: only the
    // codex pr-review skill (run by a started turn) writes to GitHub — the pm:
    // review comment and the status-label transition. App code's own `gh` surface
    // stays read-only (`gh pr list`, `gh auth status`). A `gh` *write* subcommand
    // creeping into ANY app source would silently make the app double-write
    // labels/comments.
    //
    // Carrier strength: **Medium** — a type-aware-ish scan over the app's `.rs`
    // sources at test time. (A Hard carrier would forbid the write at the type
    // level — e.g. a sealed gh-arg builder admitting only read subcommands — but
    // the `gh` args are plain `&str` slices, so the violation stays expressible;
    // this test is the machine check that catches it.) Funnel: the only gh
    // *callsites* are `pr::gh`'s `run_pr_list` / `gh_auth_status`; this scan closes
    // the downstream by walking the WHOLE `src` tree (every slice plus the root
    // composition modules — this `dispatch.rs` itself, and any future module), so a
    // write subcommand anywhere in the app fails CI, not only at those callsites.
    //
    // The forbidden patterns are BUILT from fragments at runtime so the denylist
    // literals do not appear verbatim in this file — otherwise the scan would match
    // its own source.
    #[test]
    fn app_code_uses_no_gh_write_subcommands() {
        // Built from split fragments so no forbidden subcommand appears verbatim in
        // this source — the scan now walks all of `src`, including this file.
        let forbidden: Vec<String> = vec![
            format!("pr ed{}", "it"),
            format!("pr com{}", "ment"),
            format!("pr rev{}", "iew"),
            format!("issue com{}", "ment"),
            format!("issue ed{}", "it"),
            format!("--add-l{}", "abel"),
            format!("--remove-l{}", "abel"),
        ];

        // Scan the entire app source tree (all slices + root modules), so this
        // file and any future root module are covered too.
        let src_root = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

        let mut offenders: Vec<String> = Vec::new();
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

    /// All `.rs` files under `dir`, recursively. Used by the gh-write governance
    /// scan to walk the app's sources.
    fn rs_files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                out.extend(rs_files_under(&path));
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
        out
    }
}
