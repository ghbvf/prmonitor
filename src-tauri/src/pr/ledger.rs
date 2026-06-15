//! Dispatch de-duplication ledger + cooldown source.
//!
//! Port of `router.py`'s two state files (`dispatched` keys + `dispatch-events`
//! epochs), unified into one `ledger.json` persisted via `tauri-plugin-store`'s
//! `StoreExt` — the same backend-owned store pattern as the config slice.
//!
//! PR3 uses the **read** path (`has_dispatched` / `last_dispatch_at`) to annotate
//! the PR list with "already dispatched" / cooldown skip reasons. The **write**
//! path (`record_many`) is invoked by the auto-trigger dispatcher
//! ([`crate::dispatch`]) once review turns actually start; recording it here keeps
//! the dedup machinery complete. The write is batched (one persist for the whole
//! cycle's started candidates) so unbounded concurrent starts can't race the store.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use tauri_plugin_store::StoreExt;

use crate::error::{AppError, AppResult};
use crate::model::Candidate;

/// Store file holding the persisted ledger.
const STORE_FILE: &str = "ledger.json";
/// Key holding the set of dispatched dedup keys.
const DISPATCHED_KEY: &str = "dispatched";
/// Key holding the dispatch-event log (for cooldown).
const EVENTS_KEY: &str = "events";

/// One recorded dispatch — the cooldown source (mirrors `router.py`
/// dispatch-events: `(pr, kind, dispatchedAtEpoch)`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchEvent {
    pub pr: u64,
    pub kind: String,
    pub head_sha: String,
    pub key: String,
    pub dispatched_at_epoch: u64,
}

/// In-memory snapshot of the dedup ledger + cooldown events.
#[derive(Debug, Default)]
pub struct Ledger {
    pub(crate) dispatched: HashSet<String>,
    pub(crate) events: Vec<DispatchEvent>,
}

/// `{number}@{head_sha}:{kind}` — the per-(pr, head, kind) dedup key
/// (`router.py` `Candidate.key`). Re-dispatch is suppressed once this key is in
/// the ledger, so a force-push (new head_sha) is a fresh key and *can* dispatch.
pub fn dispatch_key(number: u64, head_sha: &str, kind: &str) -> String {
    format!("{number}@{head_sha}:{kind}")
}

/// Remaining cooldown seconds when `last` is within `secs` of `now`, else `None`
/// (cooldown elapsed). `saturating_sub` so a backwards clock (`last > now`) reads
/// as age 0 rather than underflowing.
pub fn cooldown_remaining(now: u64, last: u64, secs: u64) -> Option<u64> {
    let age = now.saturating_sub(last);
    if age < secs {
        Some(secs - age)
    } else {
        None
    }
}

impl Ledger {
    /// Loads the persisted ledger, defaulting to empty when nothing is stored or
    /// a value is corrupt (a corrupt ledger must never block discovery — the
    /// worst case is a duplicate dispatch, which the live gate then re-checks).
    pub fn load<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> AppResult<Self> {
        let store = app
            .store(STORE_FILE)
            .map_err(|e| AppError::new(format!("打开 ledger 存储失败: {e}")))?;

        let dispatched = store
            .get(DISPATCHED_KEY)
            .and_then(|v| serde_json::from_value::<HashSet<String>>(v).ok())
            .unwrap_or_default();
        let events = store
            .get(EVENTS_KEY)
            .and_then(|v| serde_json::from_value::<Vec<DispatchEvent>>(v).ok())
            .unwrap_or_default();

        Ok(Self { dispatched, events })
    }

    /// Whether `key` has already been dispatched.
    pub fn has_dispatched(&self, key: &str) -> bool {
        self.dispatched.contains(key)
    }

    /// Most-recent dispatch epoch for `(pr, kind)`, or `None`. `router.py` scans
    /// dispatch-events in reverse and takes the first match (= most recent); the
    /// `max` here is order-independent and equivalent.
    pub fn last_dispatch_at(&self, pr: u64, kind: &str) -> Option<u64> {
        self.events
            .iter()
            .filter(|e| e.pr == pr && e.kind == kind)
            .map(|e| e.dispatched_at_epoch)
            .max()
    }

    /// Records a batch of dispatches (key + event per candidate) and persists
    /// **once**. Invoked by the auto-trigger dispatcher ([`crate::dispatch`]) after
    /// a poll cycle's reviews have started; PR discovery itself never dispatches.
    ///
    /// The single-persist shape matters under unbounded concurrent starts: staging
    /// every candidate's key/event in memory and saving the store one time avoids
    /// the interleaved store writes (and redundant saves) that per-candidate
    /// `record` calls would produce. An empty `cands` slice still touches the store
    /// (a harmless no-op save) — callers gate on non-empty before calling.
    pub fn record_many<R: tauri::Runtime>(
        &mut self,
        app: &tauri::AppHandle<R>,
        cands: &[Candidate],
        epoch: u64,
    ) -> AppResult<()> {
        self.stage_all(cands, epoch);

        let store = app
            .store(STORE_FILE)
            .map_err(|e| AppError::new(format!("打开 ledger 存储失败: {e}")))?;
        store.set(
            DISPATCHED_KEY,
            serde_json::to_value(&self.dispatched).map_err(|e| AppError::new(e.to_string()))?,
        );
        store.set(
            EVENTS_KEY,
            serde_json::to_value(&self.events).map_err(|e| AppError::new(e.to_string()))?,
        );
        store
            .save()
            .map_err(|e| AppError::new(format!("写入 ledger 存储失败: {e}")))?;
        Ok(())
    }

    /// Stages a batch into the in-memory ledger (the dedup key set + cooldown event
    /// log) without persisting. Split out so the staging — what `record_many`
    /// actually writes to the store — is unit-testable without a Tauri app /
    /// `tauri-plugin-store` (the persistence itself is a thin `Store::save`).
    fn stage_all(&mut self, cands: &[Candidate], epoch: u64) {
        for cand in cands {
            let key = dispatch_key(cand.number, &cand.head_sha, &cand.kind);
            self.dispatched.insert(key.clone());
            self.events.push(DispatchEvent {
                pr: cand.number,
                kind: cand.kind.clone(),
                head_sha: cand.head_sha.clone(),
                key,
                dispatched_at_epoch: epoch,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(pr: u64, kind: &str, epoch: u64) -> DispatchEvent {
        DispatchEvent {
            pr,
            kind: kind.to_string(),
            head_sha: "sha".to_string(),
            key: dispatch_key(pr, "sha", kind),
            dispatched_at_epoch: epoch,
        }
    }

    fn cand(pr: u64, kind: &str) -> Candidate {
        Candidate {
            number: pr,
            head_sha: "sha".to_string(),
            head_ref: "ref".to_string(),
            author: "octocat".to_string(),
            is_cross_repository: false,
            is_draft: false,
            kind: kind.to_string(),
        }
    }

    // `record_many` persists via `Store::save`, which needs a Tauri app; the
    // batch's data effect is `stage_all`, which is what gets serialized. This
    // round-trip asserts staging a batch records every candidate's dedup key and
    // a cooldown event per candidate at the shared epoch (the persisted shape).
    #[test]
    fn record_many_stages_every_candidate_key_and_event() {
        let mut ledger = Ledger::default();
        let cands = [cand(12, "review"), cand(12, "check"), cand(13, "review")];
        ledger.stage_all(&cands, 1_700_000_000);

        // One dedup key per candidate (distinct (pr, head, kind) tuples).
        assert!(ledger.has_dispatched(&dispatch_key(12, "sha", "review")));
        assert!(ledger.has_dispatched(&dispatch_key(12, "sha", "check")));
        assert!(ledger.has_dispatched(&dispatch_key(13, "sha", "review")));
        assert_eq!(ledger.dispatched.len(), 3);

        // One cooldown event per candidate, all at the shared epoch.
        assert_eq!(ledger.events.len(), 3);
        assert_eq!(ledger.last_dispatch_at(12, "review"), Some(1_700_000_000));
        assert_eq!(ledger.last_dispatch_at(12, "check"), Some(1_700_000_000));
        assert_eq!(ledger.last_dispatch_at(13, "review"), Some(1_700_000_000));
    }

    #[test]
    fn record_many_empty_batch_is_a_noop_stage() {
        let mut ledger = Ledger::default();
        ledger.stage_all(&[], 1_000);
        assert!(ledger.dispatched.is_empty());
        assert!(ledger.events.is_empty());
    }

    #[test]
    fn dispatch_key_format() {
        assert_eq!(dispatch_key(42, "abc123", "review"), "42@abc123:review");
        assert_eq!(dispatch_key(7, "deadbeef", "check"), "7@deadbeef:check");
    }

    #[test]
    fn cooldown_remaining_within_window() {
        // 100s cooldown, dispatched 30s ago → 70s remaining.
        assert_eq!(cooldown_remaining(1_000, 970, 100), Some(70));
    }

    #[test]
    fn cooldown_remaining_elapsed() {
        // dispatched exactly `secs` ago (boundary) and beyond → elapsed.
        assert_eq!(cooldown_remaining(1_000, 900, 100), None);
        assert_eq!(cooldown_remaining(1_000, 800, 100), None);
    }

    #[test]
    fn cooldown_remaining_clock_skew_is_saturating() {
        // last in the future → age saturates to 0 → full window remaining.
        assert_eq!(cooldown_remaining(900, 1_000, 100), Some(100));
    }

    #[test]
    fn has_dispatched_matches_recorded_key() {
        let ledger = Ledger {
            dispatched: HashSet::from(["12@abc:review".to_string()]),
            events: vec![],
        };
        assert!(ledger.has_dispatched("12@abc:review"));
        assert!(!ledger.has_dispatched("12@abc:check"));
        assert!(!ledger.has_dispatched("13@abc:review"));
    }

    #[test]
    fn last_dispatch_at_takes_most_recent_matching() {
        let ledger = Ledger {
            dispatched: HashSet::new(),
            events: vec![
                event(12, "review", 100),
                event(12, "review", 300), // most recent for (12, review)
                event(12, "check", 200),
                event(13, "review", 500),
            ],
        };
        assert_eq!(ledger.last_dispatch_at(12, "review"), Some(300));
        assert_eq!(ledger.last_dispatch_at(12, "check"), Some(200));
        assert_eq!(ledger.last_dispatch_at(13, "review"), Some(500));
        assert_eq!(ledger.last_dispatch_at(99, "review"), None);
    }

    // Wire-shape lock for the persisted `ledger.json` events (Medium carrier per
    // ai-robust.md). A field rename would make `Ledger::load` silently drop the
    // events (deserialize → `unwrap_or_default()`), wiping cooldown state and
    // re-dispatching; this round-trip guards against that.
    #[test]
    fn dispatch_event_wire_shape_is_camel_case_and_round_trips() {
        let e = event(12, "review", 1_700_000_000);
        let v = serde_json::to_value(&e).expect("DispatchEvent serializes");

        // camelCase keys present.
        assert!(v.get("pr").is_some());
        assert!(v.get("kind").is_some());
        assert!(v.get("headSha").is_some());
        assert!(v.get("key").is_some());
        assert!(v.get("dispatchedAtEpoch").is_some());

        // snake_case forms absent — a rename surfaces here.
        assert!(v.get("head_sha").is_none());
        assert!(v.get("dispatched_at_epoch").is_none());

        // Round-trips without zeroing the cooldown epoch.
        let back: DispatchEvent = serde_json::from_value(v).expect("round-trips");
        assert_eq!(back.dispatched_at_epoch, 1_700_000_000);
        assert_eq!(back.head_sha, "sha");
    }
}
