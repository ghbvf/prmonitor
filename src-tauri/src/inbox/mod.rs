//! Event-inbox slice (AB#1065, epic AB#1078): the durable ingress in front of the webhook
//! path. Every inbound delivery is persisted, deduped by its delivery identity, and kept with
//! its raw payload so it can be listed, inspected, and REPLAYED.
//!
//! **Funnel (upstream Hard, downstream durable).** The inbox's own enforcement is the
//! `UNIQUE(dedupe_key)` constraint on `inbox_event` (the **Hard** ingress-idempotency carrier,
//! see [`crate::db`]'s `SCHEMA_V4`): a re-delivered webhook (a GitHub retry, a tunnel
//! re-delivery) is UNEXPRESSIBLE as a second row, so each delivery is processed at most once.
//! That is the UPSTREAM of the funnel; the DOWNSTREAM review/check dedup is now the outbox pending
//! `dedupe_key` plus the review executor's durable claim. The inbox does NOT execute reviews; it
//! stores replayable inputs in front of the vetted producer/executor path.
//!
//! `store` owns the `inbox_event` table's queries (the slice reaches SQLite only through the
//! horizontal [`crate::db::Database`] handle — not a cross-slice import). `service` is the
//! ingress/replay logic: it works ONLY on the neutral [`crate::model::Event`] envelope and two
//! OPAQUE composition-root-injected closures ([`GithubRefeed`] / [`AzureRefresh`] /
//! [`RuleProcessor`]) — so the inbox slice names NO `pr`/`outbox` internal type. The composition
//! root (`lib.rs`) is the only place that knows both `pr` and `inbox`: it normalizes a
//! `pr::webhook::WebhookEvent` into a `model::Event` (via `pr::webhook::event_from_webhook`) at the
//! webhook seam, and installs the closures that re-feed a GitHub delivery through
//! `pr::commands::ingest_webhook`, re-run the `az` discovery, or process a candidate-backed
//! rule event. `commands` exposes the `inbox_list` / `inbox_get_raw` / `inbox_replay` Tauri
//! commands.
//!
//! The cross-slice contract types ([`crate::model::InboxStatus`] / [`crate::model::InboxEntry`])
//! live in the root `model.rs`; the [`crate::events::InboxEvent`] tagged union is the
//! `inbox:updated` payload.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::AppResult;
use crate::model::{Candidate, EventEnvelope};

pub mod commands;
pub mod manager;
pub mod service;
pub mod store;

/// The GitHub re-feed hook the composition root injects (AB#1065). Given the [`tauri::AppHandle`]
/// and the stored parsed-`WebhookEvent` JSON, it re-feeds that delivery through the vetted
/// `pr::commands::ingest_webhook` dispatch path. OPAQUE on purpose: the inbox holds only this
/// `dyn Fn` (the closure captures the composition-root refeed), so the inbox slice never
/// names `pr::webhook::WebhookEvent`. The Azure analogue is
/// [`AzureRefresh`]; making GitHub symmetric is what fully decouples the inbox from `pr`.
///
/// **Returns [`AppResult`] (AB#1065 F2):** a refeed FAILURE (e.g. the `lib.rs` closure can't
/// deserialize the stored `WebhookEvent` JSON) propagates as `Err` so the inbox marks the row
/// `Failed` rather than falsely `Processed`. This drives the inbox ROW STATUS, NOT the HTTP ACK
/// (which ties acknowledgement to durable persist only; the single worker processes it later).
pub type GithubRefeed = Arc<
    dyn Fn(
            tauri::AppHandle,
            String,
        ) -> Pin<Box<dyn Future<Output = AppResult<Option<Candidate>>> + Send>>
        + Send
        + Sync,
>;

/// The Azure re-discovery hook the composition root injects (AB#1065 / AB#822): re-run `az`
/// discovery for a `project_id`. OPAQUE (the closure wraps `SchedulerSet::discover_once` in
/// `lib.rs`), so the inbox never names a `pr` type. Mirror of [`GithubRefeed`]. Returns
/// [`AppResult`] (AB#1065 F2): a refresh failure marks the audit row `Failed`, not `Processed`.
pub type AzureRefresh =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = AppResult<()>> + Send>> + Send + Sync>;

/// The rule processor hook installed by the composition root. It consumes a persisted inbox row
/// plus the neutral event and an optional PR candidate that already passed source-specific gates.
pub type RuleProcessor = Arc<
    dyn Fn(
            tauri::AppHandle,
            i64,
            EventEnvelope,
            Option<Candidate>,
        ) -> Pin<Box<dyn Future<Output = AppResult<()>> + Send>>
        + Send
        + Sync,
>;
