//! [`ClaudeManager`] — the `AppState` handle that owns each live `claude -p`
//! review's cancel signal, keyed by session id (#718).
//!
//! Unlike codex's resident-connection manager, this holds NO long-lived process: a
//! `claude -p` review is one-shot, so the manager only needs to map an in-flight
//! session id to a way to STOP it gracefully. We store a [`tokio::sync::watch::Sender<bool>`]
//! cancel channel — NOT a task abort handle. Stopping does NOT abort the pump task
//! (that would drop the pump mid-flight and emit no terminal event, leaving the session
//! stuck `Running` and the UI hung). Instead `stop` SIGNALS cancel; the pump observes
//! the signal in its `select!` loop and converges through its `finish` path, which
//! SIGKILLs the child, emits the terminal `TurnCompleted{"interrupted"}`, sets the
//! terminal status, and deregisters. So a stopped review ends cleanly, never left
//! `Running`.
//!
//! `stop(session_id) -> bool` returns whether THIS manager owned the session (and so
//! signalled it). Because a session id is globally unique, the manager holding the
//! cancel channel definitively owns the session — so the review command can use this
//! boolean to route a stop to the claude engine vs falling through to codex's interrupt
//! path, deterministically and WITHOUT adding an engine field to the persisted
//! `SessionInfo`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use crate::review::engine::SessionId;

/// Owns the cancel-signal sender of each in-flight claude review, keyed by session id.
/// `Clone` shares one `Arc<Mutex<…>>` so the command path, the engine, and each pump's
/// terminal cleanup see the same map; `Default` so it slots into `AppState` (which
/// derives `Default`).
#[derive(Default, Clone)]
pub struct ClaudeManager {
    inner: Arc<Mutex<HashMap<SessionId, watch::Sender<bool>>>>,
}

impl ClaudeManager {
    /// Record an in-flight session's cancel-signal sender so [`Self::stop`] /
    /// [`Self::shutdown`] can signal it. Called by the engine BEFORE the pump task is
    /// spawned (the channel exists before the pump runs, closing the
    /// register-after-spawn race where a `stop` between spawn and register would miss
    /// the task). A re-register for the same (globally unique) id replaces the prior
    /// sender — not expected in practice, but harmless.
    pub fn register(&self, session_id: SessionId, tx: watch::Sender<bool>) {
        self.inner.lock().unwrap().insert(session_id, tx);
    }

    /// Deregister a session WITHOUT signalling it — the pump calls this once it reaches a
    /// terminal `result` and is finishing on its own, so the map doesn't retain a sender
    /// to an already-finished review. Idempotent (a missing id is a no-op).
    pub fn deregister(&self, session_id: &str) {
        self.inner.lock().unwrap().remove(session_id);
    }

    /// Stop the session `session_id`: remove its cancel sender and SIGNAL cancel
    /// (`send(true)`). This does NOT abort the pump task — the pump observes the signal,
    /// kills its child, and runs `finish` (emits the terminal `TurnCompleted{"interrupted"}`,
    /// sets the terminal status, deregisters), so the review unsticks rather than being
    /// left `Running`. Returns `true` iff THIS manager owned the session — the signal the
    /// review command uses to decide a stop is a claude stop (vs falling through to
    /// codex's interrupt). Idempotent: a second stop, or a stop for an unknown /
    /// codex-owned id, returns `false`. A `send` error (the pump's receiver already
    /// dropped, i.e. it just finished) is ignored — there is nothing left to stop.
    pub fn stop(&self, session_id: &str) -> bool {
        let tx = self.inner.lock().unwrap().remove(session_id);
        match tx {
            Some(tx) => {
                let _ = tx.send(true);
                true
            }
            None => false,
        }
    }

    /// Signal cancel to every in-flight review (each pump then kills its `claude -p`
    /// child and converges through `finish`) on app shutdown — wired next to
    /// `codex.shutdown()` in `lib.rs` so no review subprocess outlives the app.
    /// Idempotent. `kill_on_drop(true)` on each child is the backstop if a pump can't
    /// observe the signal before the runtime tears down.
    pub fn shutdown(&self) {
        let senders: Vec<watch::Sender<bool>> = self
            .inner
            .lock()
            .unwrap()
            .drain()
            .map(|(_, tx)| tx)
            .collect();
        for tx in senders {
            let _ = tx.send(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cancel channel whose receiver is held so the sender stays live (a watch
    /// `send` errors only when ALL receivers have dropped). Mirrors how the engine
    /// hands the receiver to the pump.
    fn cancel_pair() -> (watch::Sender<bool>, watch::Receiver<bool>) {
        watch::channel(false)
    }

    #[tokio::test]
    async fn stop_signals_true_and_removes_for_registered_id() {
        let m = ClaudeManager::default();
        let (tx, mut rx) = cancel_pair();
        m.register("sess-1".to_string(), tx);

        // Owns the session → true, and the cancel signal is delivered to the receiver.
        assert!(m.stop("sess-1"));
        assert!(rx.changed().await.is_ok(), "receiver observes a change");
        assert!(
            *rx.borrow_and_update(),
            "the signalled value is `true` (cancel)"
        );

        // A second stop is idempotent: the id is gone → false (no double-signal).
        assert!(!m.stop("sess-1"));
    }

    #[tokio::test]
    async fn stop_returns_false_for_unknown_id() {
        // An unknown / codex-owned id is NOT owned here → false (the command then
        // falls through to the codex interrupt path).
        let m = ClaudeManager::default();
        assert!(!m.stop("never-registered"));
    }

    #[tokio::test]
    async fn deregister_does_not_signal_and_leaves_stop_false() {
        let m = ClaudeManager::default();
        let (tx, mut rx) = cancel_pair();
        m.register("sess-2".to_string(), tx);
        // The pump finishing normally deregisters itself; a later stop finds nothing.
        m.deregister("sess-2");
        assert!(!m.stop("sess-2"));
        // deregister did NOT signal — the receiver still sees the initial `false`.
        assert!(!*rx.borrow_and_update(), "no cancel was signalled");
    }

    #[tokio::test]
    async fn shutdown_signals_all_registered() {
        let m = ClaudeManager::default();
        let (tx_a, mut rx_a) = cancel_pair();
        let (tx_b, mut rx_b) = cancel_pair();
        m.register("a".to_string(), tx_a);
        m.register("b".to_string(), tx_b);
        m.shutdown();
        // Both removed → stop now returns false for each.
        assert!(!m.stop("a"));
        assert!(!m.stop("b"));
        // Both receivers observed the cancel signal.
        assert!(rx_a.changed().await.is_ok() && *rx_a.borrow_and_update());
        assert!(rx_b.changed().await.is_ok() && *rx_b.borrow_and_update());
    }
}
