//! Auto-trigger dispatch seam — the reusable orchestrator that turns dispatchable
//! [`Candidate`]s into running reviews.
//!
//! This is a horizontal composition module (a peer of `events`/`state`/`model`,
//! NOT a slice): it is the one place allowed to consume BOTH the `pr` slice
//! (gating output `Candidate`, the ledger, the cooldown clock) and the `review`
//! slice (the `CodexEngine` / `SessionRegistry`) public APIs and glue them.
//! Keeping it here keeps the `pr` slice review-agnostic — the scheduler only knows
//! the abstract [`crate::pr::scheduler::Dispatcher`] hook, whose real body is
//! [`auto_dispatch`].
//!
//! **Reused by multiple trigger sources.** The scheduler installs it today; a
//! future webhook trigger (#…) will call the same `auto_dispatch` with the
//! candidates a push event yields — that is why the trigger source is not baked in
//! here.
//!
//! **The app writes NO labels / comments.** Its only side effect is *starting*
//! reviews (and recording the dedup ledger). All GitHub writes — the `pm:` review
//! comment and the status-label transition — are done by the codex pr-review skill
//! the started turn runs, never by app code. The app's own `gh` surface stays
//! read-only (`gh pr list` / `gh auth status`); a Medium guard test in
//! `pr::gh` / `review` slice sources enforces that no write subcommand creeps in.

use tauri::Manager;

use crate::error::AppResult;
use crate::model::Candidate;
use crate::pr::commands::now_epoch;
use crate::pr::ledger::Ledger;
use crate::review::engine::ReviewEngine;
use crate::review::engines::codex::CodexEngine;
use crate::review::session::{SessionInfo, SessionStatus};
use crate::state::AppState;

/// The codex binary name (PATH-resolved). Mirrors `review::commands::CODEX_BIN`
/// (that const is slice-private; the dispatcher is composition glue and re-states
/// it rather than widening the review slice's API).
const CODEX_BIN: &str = "codex";

/// Auto-start reviews for a cycle's dispatchable candidates, concurrently and
/// unbounded, then land the dedup ledger for the ones that started.
///
/// The flow:
/// 1. Empty in → return (the scheduler already gates on non-empty, but this is the
///    public entry every trigger source funnels through, so guard here too).
/// 2. Load + validate config (skip the whole batch, logged, on error — never
///    panic: a bad config must not crash the poll loop).
/// 3. **Registry guard.** With no live gate (the fresh discovery list is the
///    source of truth), the in-memory [`SessionRegistry`] is the safety net against
///    starting a *second* review for a PR whose prior one is still in flight: drop
///    any candidate that already has an active (`Starting`/`Running`/`Interrupting`)
///    session for the same `(pr, kind)`. The cross-cycle dedup is the ledger; this
///    guards the within/overlapping-cycle race the ledger (written only after a
///    start) can't.
/// 4. **Unbounded concurrent start.** One task; each candidate gets a borrowing
///    `CodexEngine` and the starts are driven by `join_all` (the engines hold `&`
///    borrows of the non-`Clone` resident `CodexManager`/`SessionRegistry`, so
///    spawning would force `'static` owned handles — `join_all` keeps the borrows).
/// 5. **Batched ledger landing.** Only the candidates whose start returned `Ok`
///    are recorded, in ONE persist ([`Ledger::record_many`]); a failed start is left
///    unrecorded so it retries next cycle. Errors are logged, never propagated.
pub async fn auto_dispatch<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    candidates: Vec<Candidate>,
) {
    if candidates.is_empty() {
        return;
    }

    // Skip (logged) on a bad config rather than panic — a hand-edited / absent
    // config must not take the poll loop down. `load_validated` re-checks the
    // skill path before we attach it to a turn (same as the review command).
    let cfg = match crate::config::service::load_validated(&app) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("auto-dispatch 跳过本轮：配置无效（{}）", e.message);
            return;
        }
    };
    let skill_abs = skill_abs_path(&cfg.repo_root, &cfg.skill_rel_path);

    let state = app.state::<AppState>();

    // Registry guard: filter out candidates already covered by an active session.
    let candidates = dispatchable_after_guard(candidates, &state.sessions.list());
    if candidates.is_empty() {
        return;
    }

    // Unbounded concurrent start, all in this one task. Each future borrows the
    // shared resident handles + config; `join_all` awaits them together. Pair each
    // result back with its candidate so the ledger lands only the started ones.
    let starts = candidates.iter().map(|c| {
        let engine = CodexEngine {
            app: &app,
            codex: &state.codex,
            registry: &state.sessions,
            codex_bin: CODEX_BIN,
            repo: &cfg.repo,
            repo_root: &cfg.repo_root,
            skill_abs_path: &skill_abs,
        };
        async move { engine.start(c.number, &c.kind).await }
    });
    let results = futures::future::join_all(starts).await;

    // Batched ledger landing: record only the candidates whose start succeeded
    // (a failed start stays unrecorded → retried next cycle). One persist.
    let mut succeeded: Vec<Candidate> = Vec::new();
    for (cand, result) in candidates.into_iter().zip(results) {
        match result {
            Ok(_session_id) => succeeded.push(cand),
            Err(e) => eprintln!(
                "auto-dispatch 启动 review 失败（PR {} {}）：{}",
                cand.number, cand.kind, e.message
            ),
        }
    }
    if !succeeded.is_empty() {
        if let Err(e) = land_ledger(&app, &succeeded) {
            eprintln!("auto-dispatch 写入 ledger 失败：{}", e.message);
        }
    }
}

/// Load the ledger and batch-record the started candidates at one epoch (one
/// persist). Split out so [`auto_dispatch`] reads linearly and the `?` error
/// funnel stays local.
fn land_ledger<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    succeeded: &[Candidate],
) -> AppResult<()> {
    let mut ledger = Ledger::load(app)?;
    ledger.record_many(app, succeeded, now_epoch())
}

/// Drop candidates that already have an *active* session for the same `(pr, kind)`
/// — the registry guard. Active = `Starting | Running | Interrupting` (a `Done` /
/// `Failed` session is finished, so its PR is eligible to re-dispatch, gated only
/// by the ledger/cooldown which discovery already applied). Pure over the
/// candidate list + a session snapshot, so it is unit-tested without an app.
fn dispatchable_after_guard(
    candidates: Vec<Candidate>,
    active_sessions: &[SessionInfo],
) -> Vec<Candidate> {
    use std::collections::HashSet;

    let active: HashSet<(u64, &str)> = active_sessions
        .iter()
        .filter(|s| {
            matches!(
                s.status,
                SessionStatus::Starting | SessionStatus::Running | SessionStatus::Interrupting
            )
        })
        .map(|s| (s.pr_number, s.kind.as_str()))
        .collect();

    candidates
        .into_iter()
        .filter(|c| !active.contains(&(c.number, c.kind.as_str())))
        .collect()
}

/// Resolve the absolute path to the pr-review skill file codex attaches to the
/// turn. `repo_root` is an absolute dir and `skill_rel_path` a relative path under
/// it (both config-validated), so the join is absolute and infallible. Mirrors
/// `review::commands::skill_abs_path` (that helper is slice-private).
fn skill_abs_path(repo_root: &str, skill_rel_path: &str) -> String {
    std::path::Path::new(repo_root)
        .join(skill_rel_path)
        .to_string_lossy()
        .into_owned()
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

    fn session(pr: u64, kind: &str, status: SessionStatus) -> SessionInfo {
        SessionInfo {
            thread_id: format!("t{pr}"),
            turn_id: format!("tn{pr}"),
            pr_number: pr,
            kind: kind.to_string(),
            status,
        }
    }

    #[test]
    fn guard_drops_candidate_with_active_same_kind_session() {
        let candidates = vec![cand(1, "review"), cand(2, "check")];
        // PR 1 review is already Running → dropped; PR 2 check has no session.
        let active = [session(1, "review", SessionStatus::Running)];
        let kept = dispatchable_after_guard(candidates, &active);
        let nums: Vec<u64> = kept.iter().map(|c| c.number).collect();
        assert_eq!(nums, vec![2]);
    }

    #[test]
    fn guard_keeps_candidate_when_session_is_different_kind() {
        // A Running `review` session must NOT block a `check` dispatch for the same
        // PR — the dedup key is `(pr, kind)`, not `pr`.
        let candidates = vec![cand(1, "check")];
        let active = [session(1, "review", SessionStatus::Running)];
        let kept = dispatchable_after_guard(candidates, &active);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].kind, "check");
    }

    #[test]
    fn guard_keeps_candidate_when_session_is_terminal() {
        // Done / Failed are finished: the PR is eligible to re-dispatch (the ledger
        // / cooldown — applied during discovery — is the cross-cycle gate, not this).
        let candidates = vec![cand(1, "review"), cand(2, "review")];
        let active = [
            session(1, "review", SessionStatus::Done),
            session(2, "review", SessionStatus::Failed),
        ];
        let kept = dispatchable_after_guard(candidates, &active);
        let nums: Vec<u64> = kept.iter().map(|c| c.number).collect();
        assert_eq!(nums, vec![1, 2]);
    }

    #[test]
    fn guard_drops_for_every_active_status() {
        // All three active statuses block a re-dispatch of the same (pr, kind).
        for status in [
            SessionStatus::Starting,
            SessionStatus::Running,
            SessionStatus::Interrupting,
        ] {
            let kept =
                dispatchable_after_guard(vec![cand(7, "review")], &[session(7, "review", status)]);
            assert!(kept.is_empty(), "active {status:?} must drop the candidate");
        }
    }

    #[test]
    fn guard_no_sessions_keeps_all() {
        let candidates = vec![cand(1, "review"), cand(2, "check")];
        let kept = dispatchable_after_guard(candidates, &[]);
        assert_eq!(kept.len(), 2);
    }

    // ── Governance: "the app writes NO labels/comments" (Medium carrier) ────────
    //
    // The acceptance invariant per `.claude/rules/prmonitor/ai-robust.md`: only the
    // codex pr-review skill (run by a started turn) writes to GitHub — the pm:
    // review comment and the status-label transition. App code's own `gh` surface
    // stays read-only (`gh pr list`, `gh auth status`). A `gh` *write* subcommand
    // creeping into the `pr` / `review` slice sources would silently make the app
    // double-write labels/comments.
    //
    // Carrier strength: **Medium** — a type-aware-ish scan over the slice `.rs`
    // sources at test time. (A Hard carrier would forbid the write at the type
    // level — e.g. a sealed gh-arg builder admitting only read subcommands — but
    // the `gh` args are plain `&str` slices, so the violation stays expressible;
    // this test is the machine check that catches it.) Funnel: the only gh
    // *callsites* are `pr::gh`'s `run_pr_list` / `gh_auth_status`; this scan closes
    // the downstream by failing CI if ANY slice source names a write subcommand,
    // not just those two callsites.
    //
    // The forbidden patterns are BUILT from fragments at runtime so the denylist
    // literals do not appear verbatim in this file — otherwise the scan would match
    // its own source.
    #[test]
    fn pr_and_review_slices_use_no_gh_write_subcommands() {
        // Built so e.g. "pr edit" never appears literally in this source file.
        let forbidden: Vec<String> = vec![
            format!("pr ed{}", "it"),
            format!("pr com{}", "ment"),
            format!("pr rev{}", "iew"),
            format!("issue com{}", "ment"),
            format!("issue ed{}", "it"),
            format!("--add-l{}", "abel"),
            format!("--remove-l{}", "abel"),
        ];

        let slice_dirs = [
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/pr"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/review"),
        ];

        let mut offenders: Vec<String> = Vec::new();
        for dir in slice_dirs {
            for path in rs_files_under(std::path::Path::new(dir)) {
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
        }

        assert!(
            offenders.is_empty(),
            "app-side gh WRITE detected (only the pr-review skill may write to GitHub):\n{}",
            offenders.join("\n")
        );
    }

    /// All `.rs` files under `dir`, recursively. Used by the gh-write governance
    /// scan to walk a slice's sources.
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
