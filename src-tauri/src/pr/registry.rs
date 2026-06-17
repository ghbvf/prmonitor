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

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri_plugin_store::StoreExt;

use crate::config::service as config_service;
use crate::error::{AppError, AppResult};
use crate::model::{PrPresence, PullRequestView, TrackedPrView};

/// Store file holding the persisted tracked-PR set.
const STORE_FILE: &str = "prs.json";
/// Key PREFIX holding the per-project list of tracked PRs (#35). The effective key
/// is `tracked:{project_id}` (see [`tracked_key`]); a single `prs.json` holds every
/// project's tracked set under its own key, so two projects' PRs never mingle in one
/// list.
const TRACKED_KEY_PREFIX: &str = "tracked";
/// Unbounded-growth cap, applied PER PROJECT (#35). Beyond this, [`TrackedPrs::prune`]
/// drops the least recently seen records (never the recent working set) — see its doc.
const MAX_TRACKED: usize = 500;

/// Store key for a project's tracked-PR set: `tracked:{project_id}` (#35).
/// Partitions the shared `prs.json` so each project's retained list is isolated.
fn tracked_key(project_id: &str) -> String {
    format!("{TRACKED_KEY_PREFIX}:{project_id}")
}

/// Serializes every read-modify-write of the persisted set (F1, PR #43). The two
/// writers — the poll cycle's upsert and the `set_pr_archived` command — each do a
/// load→mutate→save of the whole `prs.json`; without a shared critical section they
/// interleave and silently lose each other's write (an archive overwritten by a poll
/// that loaded the pre-archive snapshot, or vice versa). A process-global `Mutex<()>`
/// (the data lives in the store, not behind the lock) is the gate, and
/// [`mutate_tracked`] is its only acquirer. A module static — not an injected
/// `AppState` field — so the lock *identity* is fixed: a caller cannot accidentally
/// serialize on the wrong mutex, which closes the funnel downstream as well as up.
/// `std` (not `tokio`) `Mutex`: the guarded section is fully synchronous, so no
/// `.await` is ever held across the guard.
///
/// **Multi-project (#35):** the lock stays GLOBAL (not per-project) on purpose. Each
/// project's set lives under its own store key (`tracked:{project_id}`), but
/// `Store::save` rewrites the WHOLE `prs.json` — so two projects' parallel poll cycles
/// each doing a load→mutate→save would still clobber each other's just-written key. A
/// single global gate over the shared file is the correct granularity; a per-project
/// lock would reopen that cross-project lost-update race.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

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
    /// Loads `project_id`'s persisted set (#35), defaulting to empty when nothing is
    /// stored or the value is corrupt (a corrupt registry must never block discovery —
    /// the worst case is the list rebuilds from the next round's discovery). Reads only
    /// this project's key (`tracked:{project_id}`), so one project's retained list never
    /// shows another's PRs.
    pub fn load<R: tauri::Runtime>(app: &tauri::AppHandle<R>, project_id: &str) -> AppResult<Self> {
        let store = app
            .store(STORE_FILE)
            .map_err(|e| AppError::new(format!("打开 PR 存储失败: {e}")))?;

        let prs = store
            .get(tracked_key(project_id))
            .and_then(|v| serde_json::from_value::<Vec<TrackedPr>>(v).ok())
            .unwrap_or_default();

        Ok(Self { prs })
    }

    /// Persists the tracked set. **Module-private — the F1 funnel's upstream gate.**
    /// This is the only write path to `prs.json`, and it is reachable solely from
    /// [`mutate_tracked`] (same module), which holds [`WRITE_LOCK`] across the whole
    /// load→mutate→save. Keeping `save` private makes a lock-free read-modify-write
    /// *not expressible* outside this module: a new writer has no way to call `save`,
    /// so it must go through `mutate_tracked` and inherit the serialization. Making
    /// this `pub` reopens the lost-update race — do not.
    fn save<R: tauri::Runtime>(
        &self,
        app: &tauri::AppHandle<R>,
        project_id: &str,
    ) -> AppResult<()> {
        let store = app
            .store(STORE_FILE)
            .map_err(|e| AppError::new(format!("打开 PR 存储失败: {e}")))?;
        // tauri-plugin-store 2.x: `Store::set` is infallible and returns `()`.
        store.set(
            tracked_key(project_id),
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

    /// Unbounded-growth backstop. Archived records are **never** auto-pruned —
    /// archiving is how users retire rows, so dropping an archived PR would
    /// contradict that contract. Only the non-archived tail is capped: keep every
    /// `archived == true` record unconditionally, then keep the most-recently-seen
    /// (by `last_seen_epoch`) of the non-archived ones up to the cap's remaining
    /// budget (`MAX_TRACKED - archived_count`, saturating to 0 if archived alone
    /// already exceeds the cap). This never evicts the recent working set — only the
    /// long tail of stale, non-archived records. No-op while within the cap.
    fn prune(&mut self) {
        if self.prs.len() <= MAX_TRACKED {
            return;
        }
        let (mut archived, mut active): (Vec<TrackedPr>, Vec<TrackedPr>) =
            std::mem::take(&mut self.prs)
                .into_iter()
                .partition(|p| p.archived);
        // Cap only the non-archived records; archived ones are all retained.
        let active_budget = MAX_TRACKED.saturating_sub(archived.len());
        active.sort_by_key(|p| std::cmp::Reverse(p.last_seen_epoch));
        active.truncate(active_budget);
        archived.append(&mut active);
        self.prs = archived;
    }

    /// Sets the `archived` flag on the PR with `number`, returning `true` if it was
    /// found and set. Returns `false` (no change) for an unknown number, so the
    /// caller can skip a phantom persist + re-emit on a no-op.
    pub fn set_archived(&mut self, number: u64, archived: bool) -> bool {
        if let Some(pr) = self.prs.iter_mut().find(|p| p.number == number) {
            pr.archived = archived;
            true
        } else {
            false
        }
    }
}

/// The single serialized read-modify-write seam for the persisted set (F1). Holds
/// [`WRITE_LOCK`] across load → `mutate` → (conditional) save, so the poll cycle's
/// upsert and the `set_pr_archived` command can't interleave a load→mutate→save and
/// lose each other's write. `mutate` returns `(persist, out)`: `persist == false`
/// skips the store write entirely (e.g. an archive no-op on an unknown number — no
/// phantom write), and `out` is whatever the caller needs back (the re-emit
/// projection, or `()`). Load and save errors propagate as `Err` for the caller to
/// handle — the poll cycle folds them into a `PrEvent::Error` (it must not crash the
/// loop), the archive command returns them to the frontend.
///
/// **This is the ONLY persist path** ([`TrackedPrs::save`] is module-private), so a
/// lock-free write is not expressible outside this module — the closed funnel that
/// fixes F1 (upstream: `save` private; downstream: one fixed static [`WRITE_LOCK`]).
/// Reads (`get_prs`, the projection below) need no lock: a load is a single whole-value
/// store read, so a torn read can't happen and a stale-by-one-round snapshot self-heals.
///
/// `project_id` (#35) scopes the load + save to that project's key; the GLOBAL
/// [`WRITE_LOCK`] still guards the whole-file `prs.json` rewrite across projects.
pub fn mutate_tracked<R, T>(
    app: &tauri::AppHandle<R>,
    project_id: &str,
    mutate: impl FnOnce(&mut TrackedPrs) -> (bool, T),
) -> AppResult<T>
where
    R: tauri::Runtime,
{
    // `.unwrap()` matches the scheduler's std-Mutex convention. Poisoning can't leave
    // torn state here: the lock guards `()` (the data lives in the store, rewritten
    // wholesale by `save`), and the synchronous critical section below holds no
    // `.await` and no panic-prone step (load/save return `Result`, the closures are
    // pure), so the guard is never poisoned in practice.
    let _guard = WRITE_LOCK.lock().unwrap();
    let mut tracked = TrackedPrs::load(app, project_id)?;
    let (persist, out) = mutate(&mut tracked);
    if persist {
        tracked.save(app, project_id)?;
    }
    Ok(out)
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

/// Presence grace window for `project_id`: `2 ×` that project's resolved poll period
/// (#35), so a single missed round keeps a PR `Current` (it only flips `Stale` after
/// the window). The period is THAT project's `poll_interval_secs` (resolved via
/// [`config_service::project`]), clamped through [`super::scheduler::resolve_period`]
/// rather than re-hardcoding the default — a missing project or config-read failure
/// degrades to the default period (×2), matching the scheduler's per-project fallback.
pub fn presence_grace_secs<R: tauri::Runtime>(app: &tauri::AppHandle<R>, project_id: &str) -> u64 {
    super::scheduler::resolve_period(
        config_service::project(app, project_id).map(|p| p.poll_interval_secs),
    )
    .saturating_mul(2)
}

/// Projects `project_id`'s tracked set at *now* with that project's live grace window
/// — the common `to_view_list(tracked, now_epoch(), presence_grace_secs(app, pid))`
/// the command call sites (`get_prs`, `set_pr_archived`'s re-emit) share. The
/// scheduler keeps its inline `to_view_list` form because it already holds the cycle's
/// `now`. (#35: NOT the config-slice `config::service::project` — this is the registry
/// PROJECTION of tracked rows into wire views, distinct responsibility, same module.)
pub fn project<R: tauri::Runtime>(
    tracked: &TrackedPrs,
    app: &tauri::AppHandle<R>,
    project_id: &str,
) -> Vec<TrackedPrView> {
    to_view_list(
        tracked,
        super::ledger::now_epoch(),
        presence_grace_secs(app, project_id),
    )
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

        assert!(t.set_archived(1, true), "found+set returns true");
        assert!(t.prs[0].archived);
        assert!(t.set_archived(1, false));
        assert!(!t.prs[0].archived);

        // Unknown number is a no-op: returns false (so the caller skips persist/emit)
        // and must not panic or insert.
        assert!(!t.set_archived(999, true), "unknown number returns false");
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

    #[test]
    fn prune_keeps_archived_even_when_old() {
        let mut t = TrackedPrs::default();
        // One very-old ARCHIVED record (last_seen 0) that must survive pruning,
        // plus MAX_TRACKED + 50 non-archived records all seen more recently.
        let mut old_archived = tracked(9_999, 0);
        old_archived.archived = true;
        t.prs.push(old_archived);
        // 550 non-archived with last_seen 1..=550 (all newer than the archived one).
        for i in 0..(MAX_TRACKED as u64 + 50) {
            t.prs.push(tracked(i, i + 1));
        }
        t.prune();

        // The archived record survived despite being the least-recently-seen.
        assert!(
            t.prs.iter().any(|p| p.number == 9_999 && p.archived),
            "an old archived record must never be auto-pruned"
        );
        // Non-archived capped to the budget = MAX_TRACKED - 1 archived = 499.
        let active_count = t.prs.iter().filter(|p| !p.archived).count();
        assert_eq!(
            active_count,
            MAX_TRACKED - 1,
            "non-archived capped to MAX_TRACKED minus the archived count"
        );
        // The dropped non-archived ones are the oldest: 550 - 499 = 51 dropped
        // (last_seen 1..=51), so the surviving minimum last_seen is 52.
        let min_active = t
            .prs
            .iter()
            .filter(|p| !p.archived)
            .map(|p| p.last_seen_epoch)
            .min()
            .unwrap();
        assert_eq!(
            min_active, 52,
            "the 51 least-recently-seen non-archived were dropped"
        );
    }

    // The grace boundary is inclusive (`<=`): a record last seen exactly
    // `grace_secs` ago is still `Current`, not `Stale`.
    #[test]
    fn to_view_list_grace_boundary_is_inclusive() {
        let t = TrackedPrs {
            prs: vec![tracked(5, 1_000)],
        };
        // age == grace_secs (1120 - 1000 == 120) → Current.
        let views = to_view_list(&t, 1_120, 120);
        assert!(matches!(views[0].presence, PrPresence::Current));
    }
}
