//! Realtime stream bus (AB#1072 / #1373) — a single in-process broadcast hub for the unified
//! [`crate::events::StreamEvent`] envelope, plus the producer funnel [`emit`].
//!
//! **Why a bus.** Review deltas/status and action-outbox transitions were each pushed STRAIGHT to
//! the desktop frontend via `app.emit(...)` — one transport, no second subscriber. The Remote Web
//! Console needs the SAME events over HTTP (SSE). The bus is the in-process fan-out point that lets
//! a second consumer (the local-api SSE endpoint, `review::local_api`) subscribe to one typed
//! stream WITHOUT each producer learning about HTTP.
//!
//! **Composition horizontal, not a slice.** Like `remote` / `dispatch`, this names sibling
//! contract modules (`crate::events`, `crate::state`) directly; the slice-boundary test does not
//! scan it. It is deliberately NOT a trait seam — there is exactly one bus impl and the
//! producers/consumers are concrete, so a `StreamProvider` trait would be speculative abstraction
//! (mirrors the `remote::supervisor` no-speculative-seam precedent; the ai-robust charter's
//! trait-seam rule wants "new implementation, not changed callsite" — there is no second impl).
//!
//! **The single emission funnel ([`emit`]).** Each review/action event has ONE emission point: a
//! call to [`emit`], which (1) emits SYNCHRONOUSLY to the desktop on the existing per-domain Tauri
//! channel — so the desktop contract AND delivery reliability are unchanged (no lossy hop, no
//! background bridge task) — then (2) broadcasts the unified envelope on the bus for SSE. The bus
//! itself is best-effort/lossy (a lagging SSE client drops frames), which is why the latency- and
//! reliability-sensitive desktop path does NOT read the bus.

use tauri::{AppHandle, Emitter, Manager, Runtime};
use tokio::sync::broadcast;

use crate::events::{
    StreamEvent, OUTBOX_UPDATED_EVENT, PRS_UPDATED_EVENT, REVIEW_EVENT, TERMINAL_EVENT,
};
use crate::state::AppState;

/// Bus ring capacity. Generous so a brief consumer stall (an SSE client on a slow link, the
/// `BroadcastStream` poll cadence) does not drop events under normal load. An overflow only lags
/// the lossy-tolerant SSE consumer — the desktop path does not read the bus (see [`emit`]) — and a
/// dropped review delta is still recoverable from the persisted session history.
const STREAM_BUS_CAPACITY: usize = 1024;

/// In-process broadcast hub for [`StreamEvent`]. Lives in [`AppState`] (`Default`); `publish` /
/// `subscribe` take `&self`. A thin wrapper over `tokio::sync::broadcast` — the SOLE fan-out point
/// the SSE endpoint subscribes to.
pub struct StreamBus {
    tx: broadcast::Sender<StreamEvent>,
}

impl Default for StreamBus {
    fn default() -> Self {
        // Keep only the `Sender`; subscribers call `subscribe()` to get their own `Receiver`.
        let (tx, _rx) = broadcast::channel(STREAM_BUS_CAPACITY);
        Self { tx }
    }
}

impl StreamBus {
    /// Broadcast one event to all live subscribers. Best-effort: an `Err` (no subscribers, or a
    /// lagging receiver) is intentionally swallowed — the bus feeds only the lossy-tolerant SSE
    /// endpoint, and a dropped delta is recoverable from the persisted session history.
    pub fn publish(&self, event: StreamEvent) {
        let _ = self.tx.send(event);
    }

    /// Subscribe a new receiver (one per SSE connection). A subscriber sees only events sent AFTER
    /// it subscribes, so the SSE handler subscribes BEFORE its existence check to avoid a
    /// "subscribed too late" race (mirrors `SessionRegistry::subscribe_completion`'s ordering).
    pub fn subscribe(&self) -> broadcast::Receiver<StreamEvent> {
        self.tx.subscribe()
    }
}

/// The single producer funnel for review/action realtime events — replaces the former direct
/// `app.emit(REVIEW_EVENT / OUTBOX_UPDATED_EVENT, …)` call sites so each event has ONE emission
/// point (no dual-publish drift). It (1) emits SYNCHRONOUSLY to the desktop frontend on the
/// existing per-domain Tauri channel — preserving the current contract + delivery reliability —
/// then (2) broadcasts the unified envelope on the bus for the local-api SSE endpoint.
///
/// **Funnel rating (charter `.claude/rules/prmonitor/ai-robust.md`).** This is the SOLE emission
/// funnel for realtime review/action events, so both ends are closed:
/// - **DOWNSTREAM = Hard**: the `match` over the sealed [`StreamEvent`] is a compile error if a new
///   domain has no arm, forcing it to declare its desktop channel here.
/// - **UPSTREAM = Medium**: a producer could otherwise BYPASS the bus with a direct
///   `app.emit(REVIEW_EVENT / OUTBOX_UPDATED_EVENT / TERMINAL_EVENT, …)` (the channel-name consts
///   are `pub`), silently starving SSE. The scan test [`tests::no_direct_emit_of_funnelled_channels_outside_stream`]
///   closes that open end: a reference to either channel const ANYWHERE outside `stream.rs` /
///   `events.rs` is a CI-caught violation. (Was Soft — a reviewer P2 — until that guard landed.)
pub fn emit<R: Runtime>(app: &AppHandle<R>, event: StreamEvent) {
    match &event {
        StreamEvent::Pr(e) => {
            let _ = app.emit(PRS_UPDATED_EVENT, e);
        }
        StreamEvent::Review(e) => {
            // Best-effort, same as the prior direct emit: a gone window is not worth propagating.
            let _ = app.emit(REVIEW_EVENT, e);
        }
        StreamEvent::Action(e) => {
            let _ = app.emit(OUTBOX_UPDATED_EVENT, e);
        }
        StreamEvent::Terminal(e) => {
            let _ = app.emit(TERMINAL_EVENT, e);
        }
    }
    app.state::<AppState>().stream.publish(event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ReviewEvent;

    fn sample() -> StreamEvent {
        StreamEvent::Review(ReviewEvent::MessageDelta {
            project_id: "p1".to_string(),
            thread_id: "t1".to_string(),
            item_id: "i1".to_string(),
            text: "x".to_string(),
        })
    }

    #[test]
    fn publish_fans_out_to_every_subscriber() {
        let bus = StreamBus::default();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        bus.publish(sample());
        // Both receivers observe the same event (broadcast fan-out is what lets the desktop bridge
        // and an SSE client read the one stream independently).
        assert!(matches!(a.try_recv(), Ok(StreamEvent::Review(_))));
        assert!(matches!(b.try_recv(), Ok(StreamEvent::Review(_))));
    }

    #[test]
    fn publish_with_no_subscribers_is_swallowed() {
        // Best-effort: a send with zero receivers must not panic — `publish` swallows the Err.
        let bus = StreamBus::default();
        bus.publish(sample());
    }

    #[test]
    fn late_subscriber_misses_prior_events() {
        // Documents the race the SSE handler avoids by subscribing BEFORE its existence check: a
        // subscriber created after a publish does NOT replay it.
        let bus = StreamBus::default();
        bus.publish(sample());
        let mut late = bus.subscribe();
        assert!(matches!(
            late.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    /// Upstream funnel guard (**Medium** carrier per ai-robust.md, mirroring `slice_boundary_test`):
    /// every review/action realtime event MUST flow through [`emit`] (which both desktop-emits AND
    /// bus-broadcasts), never via a direct `app.emit(REVIEW_EVENT / OUTBOX_UPDATED_EVENT, …)`. The
    /// channel-name consts are `pub`, so without this scan a producer could bypass the bus and
    /// silently starve SSE (a non-closed funnel the charter forbids). Only `events.rs` (which defines
    /// the consts and their goldens) and `stream.rs` (THE funnel) may name them; any other reference
    /// is a CI-caught violation. The funnel's downstream is Hard (the exhaustive `match` in [`emit`]),
    /// so this scan closes the open upstream end.
    ///
    /// The scan matches BOTH the channel-name consts AND their raw wire string literals
    /// (`"review:event"` / `"outbox:updated"`): a bypass could `app.emit("review:event", …)` with
    /// the literal instead of the const, which a name-only scan would miss (reviewer P2 F1).
    #[test]
    fn no_direct_emit_of_funnelled_channels_outside_stream() {
        use std::fs;
        use std::path::Path;

        // `events.rs` defines + golden-tests the consts (and their wire literals); `stream.rs` is
        // the funnel that emits them. Every other file naming the const OR the literal is a bypass.
        const ALLOWED: [&str; 2] = ["events.rs", "stream.rs"];
        const FUNNELLED: [&str; 8] = [
            "PRS_UPDATED_EVENT",
            "REVIEW_EVENT",
            "OUTBOX_UPDATED_EVENT",
            "TERMINAL_EVENT",
            "\"prs:updated\"",
            "\"review:event\"",
            "\"outbox:updated\"",
            "\"terminal:event\"",
        ];

        fn scan(dir: &Path, violations: &mut Vec<String>, scanned: &mut usize) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    scan(&path, violations, scanned);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if ALLOWED.contains(&name) {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                *scanned += 1;
                for (i, line) in text.lines().enumerate() {
                    // Strip a trailing `//` comment so a doc/inline mention is not a false positive.
                    let code = line.split("//").next().unwrap_or(line);
                    for chan in FUNNELLED {
                        if code.contains(chan) {
                            violations.push(format!(
                                "{}:{} references the funnelled channel `{chan}` — emit review/action \
                                 events through crate::stream::emit, never a direct app.emit",
                                path.display(),
                                i + 1
                            ));
                        }
                    }
                }
            }
        }

        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut violations = Vec::new();
        let mut scanned = 0usize;
        scan(&src, &mut violations, &mut scanned);
        // Guard against a moved src dir making the assertion vacuously pass (mirrors the boundary test).
        assert!(
            scanned > 0,
            "funnel scan found no .rs files outside the allowlist — src layout changed?"
        );
        assert!(
            violations.is_empty(),
            "realtime review/action events must be emitted through crate::stream::emit:\n{}",
            violations.join("\n")
        );
    }
}
