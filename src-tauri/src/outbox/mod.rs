//! Action-outbox slice (AB#1066, epic AB#1078): the durable egress in front of every side
//! effect. A produced action (today the review-completion desktop notification) is PERSISTED
//! before it runs, so a pending action survives an app restart and a failed one retries to a
//! terminal dead-letter — the symmetric mirror of the [`crate::inbox`] ingress.
//!
//! **Decoupled from `review` / `pr` (mirrors the inbox's pr-decoupling).** This slice names NO
//! foreign-slice type. The actual side effect is performed by an OPAQUE, composition-root-injected
//! [`ActionExecutor`] closure (installed in `lib.rs`, the only place that knows both the outbox and
//! `review::notify`); the **Hard** exhaustive `match ActionKind` that routes a kind to its provider
//! lives inside THAT closure. The outbox holds only the neutral [`crate::model::ActionKind`] /
//! [`OutboxAction`] and the `dyn Fn` — so it stays a generic durable queue, blind to its producers
//! and consumers.
//!
//! `store` owns the `action_outbox` table's queries (reaching SQLite only through the horizontal
//! [`crate::db::Database`] handle, like `review::history_store` / `inbox::store`). `service` is the
//! enqueue + worker logic: `enqueue` persists a produced action; `run_due_once` claims the due
//! `pending` rows, runs each through the injected executor, and records the terminal/retry/dead
//! transition. `manager` holds the worker task + its wake/stop signals (a `tauri::State` field on
//! [`crate::state::AppState`], like the inbox's `InboxManager`). `commands` exposes the
//! `outbox_list` / `outbox_get_raw` / `outbox_retry` Tauri commands.
//!
//! The cross-slice contract types ([`crate::model::ActionStatus`] / [`crate::model::ActionKind`] /
//! [`crate::model::OutboxEntry`]) live in the root `model.rs`; the [`crate::events::OutboxEvent`]
//! tagged union is the `outbox:updated` payload.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::db::Database;
use crate::error::AppResult;
use crate::model::ActionKind;

pub mod commands;
pub mod manager;
pub mod service;
pub mod store;

/// The side-effect executor the composition root injects (AB#1066). Given the
/// [`tauri::AppHandle`] and one claimed [`OutboxAction`], it performs the action — today routing
/// `ActionKind::Notification` to `review::notify::deliver` via an exhaustive `match` in `lib.rs`.
/// OPAQUE on purpose: the outbox holds only this `dyn Fn` (the closure, defined in `lib.rs`, is the
/// sole place naming `review::notify`), so the outbox slice never names a `review`/`pr` type — the
/// mirror of [`crate::inbox::GithubRefeed`].
///
/// **Returns [`AppResult`] (AB#1066):** an executor FAILURE (e.g. the OS notification API errors,
/// or the closure can't deserialize the stored payload) propagates as `Err` so the worker bumps the
/// row's `attempt_count` and reschedules it — or, at the attempt cap, dead-letters it — rather than
/// falsely marking it `done`.
pub type ActionExecutor = Arc<
    dyn Fn(tauri::AppHandle, OutboxAction) -> Pin<Box<dyn Future<Output = AppResult<()>> + Send>>
        + Send
        + Sync,
>;

/// Composition-root-injected hook to release a row's AB#1204 review-execution claim once the row
/// TERMINALIZES (`done`/`dead`) — the same injection shape as [`ActionExecutor`] keeps this slice
/// blind to `review` (the closure, in `lib.rs`, is the only place naming `review::claim_store`). The
/// worker calls it after a terminal transition; it is a no-op for non-review rows (no claim exists)
/// and idempotent. Table hygiene only — correctness does NOT depend on it (a terminal row is never
/// re-claimed), so this never affects the dedup decision, only bounds `outbox_review_claim`.
pub type ClaimReleaser = Arc<dyn Fn(&Database, i64) + Send + Sync>;

/// One claimed outbox row, handed to the [`ActionExecutor`] (AB#1066). Carries the neutral
/// [`ActionKind`] + the opaque `payload` (a `model::Notification` JSON today; the executor
/// deserializes it) plus the routing `project_id` and the row `id` for diagnostics. `attempt_count`
/// is the number of attempts BEFORE this run (the worker uses it to decide retry-vs-dead-letter; the
/// executor ignores it). A plain struct (no serde): the store builds it from columns and the worker
/// consumes it in-process — it never crosses a wire.
#[derive(Debug, Clone)]
pub struct OutboxAction {
    /// The `action_outbox` row id.
    pub id: i64,
    /// Routing key (#35): which project this action belongs to.
    pub project_id: String,
    /// The side effect's kind (the executor routes on it).
    pub kind: ActionKind,
    /// The serialized action body (a `model::Notification` JSON today).
    pub payload: String,
    /// Attempts that have already run for this row (0 on the first claim).
    pub attempt_count: u32,
    /// When the action was enqueued (epoch seconds) (AB#1182). The worker compares this against
    /// the kind's staleness TTL to dead-letter a row that has sat `pending` too long (e.g. a
    /// notification persisted before a restart) instead of firing it as a ghost.
    pub created_at: u64,
}
