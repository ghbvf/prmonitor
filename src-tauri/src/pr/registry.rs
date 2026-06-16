//! Persisted PR-retention registry — the "ghost flicker" fix.
//!
//! Each poll round upserts the discovered PRs into a persisted set (`prs.json`
//! via `tauri-plugin-store`'s `StoreExt`, the same backend-owned store pattern as
//! [`super::ledger`] / the config slice). The emitted list is the *retained* set,
//! not the raw per-round discovery: a transient one-round `gh` miss no longer
//! drops a row — it just flips that row's [`crate::model::PrPresence`] from
//! `Current` to `Stale` once it ages past the presence grace window. PRs are never
//! auto-evicted (users archive inactive ones); [`TrackedPrs::prune`] is only the
//! unbounded-growth backstop.
//!
//! Slice boundary: presence is computed purely from `last_seen_epoch` vs the grace
//! window — the `pr` slice stays review-agnostic and never reads `state.sessions`.

use serde::{Deserialize, Serialize};
use tauri_plugin_store::StoreExt;

use crate::config::service as config_service;
use crate::error::{AppError, AppResult};
use crate::model::{PrPresence, PullRequestView, TrackedPrView};

/// Store file holding the persisted tracked-PR set.
const STORE_FILE: &str = "prs.json";
/// Key holding the list of tracked PRs.
const TRACKED_KEY: &str = "tracked";
/// Unbounded-growth cap. Beyond this, [`TrackedPrs::prune`] drops the least
/// recently seen records (never the recent working set) — see its doc.
const MAX_TRACKED: usize = 500;

/// One persisted PR. `first_seen_epoch` is set once on insert and preserved across
/// upserts; `last_seen_epoch` is bumped to `now` every round the PR is discovered
/// (the presence clock). `archived` is user-controlled and survives upserts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackedPr {
    pub number: u64,
    pub title: String,
    pub labels: Vec<String>,
    pub url: String,
    pub kind: String,
    pub skip_reason: Option<String>,
    pub first_seen_epoch: u64,
    pub last_seen_epoch: u64,
    pub archived: bool,
}

/// In-memory snapshot of the persisted tracked-PR set.
#[derive(Debug, Default)]
pub struct TrackedPrs {
    pub(crate) prs: Vec<TrackedPr>,
}

impl TrackedPrs {
    /// Loads the persisted set, defaulting to empty when nothing is stored or the
    /// value is corrupt (a corrupt registry must never block discovery — the worst
    /// case is the list rebuilds from the next round's discovery).
    pub fn load<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> AppResult<Self> {
        let store = app
            .store(STORE_FILE)
            .map_err(|e| AppError::new(format!("打开 PR 存储失败: {e}")))?;

        let prs = store
            .get(TRACKED_KEY)
            .and_then(|v| serde_json::from_value::<Vec<TrackedPr>>(v).ok())
            .unwrap_or_default();

        Ok(Self { prs })
    }

    /// Persists the tracked set.
    pub fn save<R: tauri::Runtime>(&self, app: &tauri::AppHandle<R>) -> AppResult<()> {
        let store = app
            .store(STORE_FILE)
            .map_err(|e| AppError::new(format!("打开 PR 存储失败: {e}")))?;
        store.set(
            TRACKED_KEY,
            serde_json::to_value(&self.prs).map_err(|e| AppError::new(e.to_string()))?,
        );
        store
            .save()
            .map_err(|e| AppError::new(format!("写入 PR 存储失败: {e}")))?;
        Ok(())
    }

    /// Upserts this round's discovered views into the tracked set. An existing PR
    /// (same `number`) has its display fields refreshed and `last_seen_epoch` bumped
    /// to `now`, KEEPING its original `first_seen_epoch` and `archived` flag; a new
    /// PR is inserted with `first_seen_epoch == last_seen_epoch == now` and
    /// `archived: false`. Prunes afterward so the set can't grow unbounded.
    pub fn upsert(&mut self, views: &[PullRequestView], now: u64) {
        for view in views {
            if let Some(existing) = self.prs.iter_mut().find(|p| p.number == view.number) {
                existing.title = view.title.clone();
                existing.labels = view.labels.clone();
                existing.url = view.url.clone();
                existing.kind = view.kind.clone();
                existing.skip_reason = view.skip_reason.clone();
                existing.last_seen_epoch = now;
                // first_seen_epoch and archived are preserved across upserts.
            } else {
                self.prs.push(TrackedPr {
                    number: view.number,
                    title: view.title.clone(),
                    labels: view.labels.clone(),
                    url: view.url.clone(),
                    kind: view.kind.clone(),
                    skip_reason: view.skip_reason.clone(),
                    first_seen_epoch: now,
                    last_seen_epoch: now,
                    archived: false,
                });
            }
        }
        self.prune();
    }

    /// Unbounded-growth backstop: when the set exceeds [`MAX_TRACKED`], keep the
    /// `MAX_TRACKED` most-recently-seen records (by `last_seen_epoch`) and drop the
    /// rest. This never evicts the recent working set — only the long tail of stale
    /// records a user never archived. No-op while within the cap.
    fn prune(&mut self) {
        if self.prs.len() > MAX_TRACKED {
            // Most-recently-seen first, then keep the cap's worth.
            self.prs
                .sort_by_key(|p| std::cmp::Reverse(p.last_seen_epoch));
            self.prs.truncate(MAX_TRACKED);
        }
    }

    /// Sets the `archived` flag on the PR with `number` (no-op if absent).
    pub fn set_archived(&mut self, number: u64, archived: bool) {
        if let Some(pr) = self.prs.iter_mut().find(|p| p.number == number) {
            pr.archived = archived;
        }
    }
}

/// Projects the tracked set into the frontend wire rows. Each record's presence is
/// `Current` when last seen within `grace_secs` of `now`, else `Stale`
/// (`saturating_sub` so a backwards clock reads age 0 → `Current`). Rows are sorted
/// by PR number DESC (newest first). Presence is purely `last_seen_epoch` vs grace
/// — no `state.sessions` read, keeping the `pr` slice review-agnostic.
pub fn to_view_list(tracked: &TrackedPrs, now: u64, grace_secs: u64) -> Vec<TrackedPrView> {
    let mut views: Vec<TrackedPrView> = tracked
        .prs
        .iter()
        .map(|p| TrackedPrView {
            pr: PullRequestView {
                number: p.number,
                title: p.title.clone(),
                labels: p.labels.clone(),
                url: p.url.clone(),
                kind: p.kind.clone(),
                skip_reason: p.skip_reason.clone(),
            },
            presence: if now.saturating_sub(p.last_seen_epoch) <= grace_secs {
                PrPresence::Current
            } else {
                PrPresence::Stale
            },
            archived: p.archived,
        })
        .collect();
    views.sort_by_key(|v| std::cmp::Reverse(v.pr.number));
    views
}

/// Presence grace window: `2 ×` the resolved poll period, so a single missed round
/// keeps a PR `Current` (it only flips `Stale` after the window). Single-sources
/// the period clamp via [`super::scheduler::resolve_period`] rather than
/// re-hardcoding the default — a config-read failure degrades to the default
/// period (×2).
pub fn presence_grace_secs<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> u64 {
    super::scheduler::resolve_period(config_service::load(app).map(|c| c.poll_interval_secs))
        .saturating_mul(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(number: u64, title: &str) -> PullRequestView {
        PullRequestView {
            number,
            title: title.to_string(),
            labels: vec!["review-label".to_string()],
            url: format!("https://x/{number}"),
            kind: "review".to_string(),
            skip_reason: None,
        }
    }

    fn tracked(number: u64, last_seen: u64) -> TrackedPr {
        TrackedPr {
            number,
            title: format!("PR {number}"),
            labels: vec![],
            url: format!("https://x/{number}"),
            kind: "review".to_string(),
            skip_reason: None,
            first_seen_epoch: 0,
            last_seen_epoch: last_seen,
            archived: false,
        }
    }

    // Wire-shape lock for the persisted `prs.json` records (Medium carrier per
    // ai-robust.md). A field rename would make `TrackedPrs::load` silently drop the
    // records (deserialize → `unwrap_or_default()`), wiping the retained set and
    // re-introducing the ghost flicker on restart; this round-trip guards against it.
    #[test]
    fn tracked_pr_wire_shape_is_camel_case_and_round_trips() {
        let pr = TrackedPr {
            number: 12,
            title: "Add feature".to_string(),
            labels: vec!["review-label".to_string()],
            url: "https://x/12".to_string(),
            kind: "review".to_string(),
            skip_reason: Some("draft PR".to_string()),
            first_seen_epoch: 1_700_000_000,
            last_seen_epoch: 1_700_000_500,
            archived: true,
        };
        let v = serde_json::to_value(&pr).expect("TrackedPr serializes");

        // camelCase keys present.
        assert!(v.get("number").is_some());
        assert!(v.get("title").is_some());
        assert!(v.get("labels").is_some());
        assert!(v.get("url").is_some());
        assert!(v.get("kind").is_some());
        assert!(v.get("skipReason").is_some());
        assert!(v.get("firstSeenEpoch").is_some());
        assert!(v.get("lastSeenEpoch").is_some());
        assert!(v.get("archived").is_some());

        // snake_case forms absent — a rename surfaces here.
        assert!(v.get("skip_reason").is_none());
        assert!(v.get("first_seen_epoch").is_none());
        assert!(v.get("last_seen_epoch").is_none());

        // Round-trips without zeroing the presence clock or the archived flag.
        let back: TrackedPr = serde_json::from_value(v).expect("round-trips");
        assert_eq!(back.first_seen_epoch, 1_700_000_000);
        assert_eq!(back.last_seen_epoch, 1_700_000_500);
        assert!(back.archived);
    }

    #[test]
    fn upsert_new_sets_first_and_last_seen() {
        let mut t = TrackedPrs::default();
        t.upsert(&[view(1, "PR one")], 1_000);

        assert_eq!(t.prs.len(), 1);
        let pr = &t.prs[0];
        assert_eq!(pr.number, 1);
        assert_eq!(pr.title, "PR one");
        assert_eq!(pr.first_seen_epoch, 1_000);
        assert_eq!(pr.last_seen_epoch, 1_000);
        assert!(!pr.archived);
    }

    #[test]
    fn upsert_existing_updates_fields_and_keeps_first_seen_and_archived() {
        let mut t = TrackedPrs::default();
        t.upsert(&[view(1, "old title")], 1_000);
        t.set_archived(1, true);

        // Second round: same number, fresh title, later epoch.
        t.upsert(&[view(1, "new title")], 2_000);

        assert_eq!(t.prs.len(), 1, "upsert must not duplicate by number");
        let pr = &t.prs[0];
        assert_eq!(pr.title, "new title", "display fields refresh");
        assert_eq!(pr.first_seen_epoch, 1_000, "first_seen_epoch preserved");
        assert_eq!(pr.last_seen_epoch, 2_000, "last_seen_epoch bumped");
        assert!(pr.archived, "archived flag preserved across upsert");
    }

    #[test]
    fn set_archived_toggles() {
        let mut t = TrackedPrs::default();
        t.upsert(&[view(1, "PR one")], 1_000);

        t.set_archived(1, true);
        assert!(t.prs[0].archived);
        t.set_archived(1, false);
        assert!(!t.prs[0].archived);

        // Unknown number is a no-op (must not panic or insert).
        t.set_archived(999, true);
        assert_eq!(t.prs.len(), 1);
    }

    #[test]
    fn to_view_list_current_within_grace_and_stale_beyond() {
        let t = TrackedPrs {
            prs: vec![tracked(1, 1_000), tracked(2, 500)],
        };
        // now=1_100, grace=120: PR1 seen 100s ago → Current; PR2 seen 600s ago → Stale.
        let views = to_view_list(&t, 1_100, 120);

        // Sorted DESC by number, so [0] is PR2, [1] is PR1.
        let pr2 = views.iter().find(|v| v.pr.number == 2).unwrap();
        let pr1 = views.iter().find(|v| v.pr.number == 1).unwrap();
        assert!(matches!(pr1.presence, PrPresence::Current));
        assert!(matches!(pr2.presence, PrPresence::Stale));
    }

    #[test]
    fn to_view_list_sorted_by_number_desc() {
        let t = TrackedPrs {
            prs: vec![tracked(1, 0), tracked(3, 0), tracked(2, 0)],
        };
        let views = to_view_list(&t, 0, 120);
        let numbers: Vec<u64> = views.iter().map(|v| v.pr.number).collect();
        assert_eq!(numbers, vec![3, 2, 1]);
    }

    #[test]
    fn prune_caps_at_max_tracked() {
        let mut t = TrackedPrs::default();
        // Insert MAX_TRACKED + 50 records, each with a distinct (increasing)
        // last_seen so the oldest are deterministically droppable.
        for i in 0..(MAX_TRACKED as u64 + 50) {
            t.prs.push(tracked(i, i)); // last_seen == i
        }
        t.prune();

        assert_eq!(t.prs.len(), MAX_TRACKED, "pruned down to the cap");
        // The kept records are the most-recently-seen MAX_TRACKED (highest last_seen):
        // the 50 oldest (last_seen 0..49) were dropped.
        let min_last_seen = t.prs.iter().map(|p| p.last_seen_epoch).min().unwrap();
        assert_eq!(min_last_seen, 50, "the 50 least-recently-seen were dropped");
    }
}
