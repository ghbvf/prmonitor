//! [`ClaudeManager`] — the `AppState` handle that owns each live `claude -p`
//! review's kill handle, keyed by session id (#718).
//!
//! Unlike codex's resident-connection manager, this holds NO long-lived process: a
//! `claude -p` review is one-shot, so the manager only needs to map an in-flight
//! session id to a way to terminate its child. We store the pump task's
//! [`tokio::task::AbortHandle`]; aborting the pump drops the future that owns the
//! [`super::process::ClaudeProcess`], and `kill_on_drop(true)` on that child means
//! the drop SIGKILLs the subprocess. One handle ⇒ both the streaming and the child
//! die together, no separate kill plumbing.
//!
//! `stop(session_id) -> bool` returns whether THIS manager owned the session (and so
//! killed it). Because a session id is globally unique, the manager holding the child
//! definitively owns the session — so the review command can use this boolean to
//! route a stop to the claude engine vs falling through to codex's interrupt path,
//! deterministically and WITHOUT adding an engine field to the persisted `SessionInfo`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::task::AbortHandle;

use crate::review::engine::SessionId;

/// Owns the abort handle of each in-flight claude review's pump task, keyed by
/// session id. `Clone` shares one `Arc<Mutex<…>>` so the command path, the engine,
/// and each pump's terminal cleanup see the same map; `Default` so it slots into
/// `AppState` (which derives `Default`).
#[derive(Default, Clone)]
pub struct ClaudeManager {
    inner: Arc<Mutex<HashMap<SessionId, AbortHandle>>>,
}

impl ClaudeManager {
    /// Record an in-flight session's pump-task abort handle so [`Self::stop`] /
    /// [`Self::shutdown`] can terminate it. Called by the engine right after the
    /// pump task is spawned. A re-register for the same (globally unique) id replaces
    /// the prior handle — not expected in practice, but harmless.
    pub fn register(&self, session_id: SessionId, handle: AbortHandle) {
        self.inner.lock().unwrap().insert(session_id, handle);
    }

    /// Deregister a session WITHOUT aborting it — the pump calls this once it reaches a
    /// terminal `result` and is finishing on its own, so the map doesn't retain a
    /// handle to an already-finished task. Idempotent (a missing id is a no-op).
    pub fn deregister(&self, session_id: &str) {
        self.inner.lock().unwrap().remove(session_id);
    }

    /// Stop the session `session_id`: remove + abort its pump task (which drops the
    /// `kill_on_drop` child, SIGKILLing `claude -p`). Returns `true` iff THIS manager
    /// owned the session — the signal the review command uses to decide a stop is a
    /// claude stop (vs falling through to codex's interrupt). Idempotent: a second
    /// stop, or a stop for an unknown / codex-owned id, returns `false`.
    pub fn stop(&self, session_id: &str) -> bool {
        let handle = self.inner.lock().unwrap().remove(session_id);
        match handle {
            Some(handle) => {
                handle.abort();
                true
            }
            None => false,
        }
    }

    /// Abort every in-flight review's pump task (killing each `claude -p` child via
    /// `kill_on_drop`) on app shutdown — wired next to `codex.shutdown()` in `lib.rs`
    /// so no review subprocess outlives the app. Idempotent.
    pub fn shutdown(&self) {
        let handles: Vec<AbortHandle> =
            self.inner.lock().unwrap().drain().map(|(_, h)| h).collect();
        for handle in handles {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dummy pump-task abort handle: spawn a task that parks forever, take its
    /// `AbortHandle`. Aborting it is observable via `is_finished()` after a yield.
    async fn dummy_handle() -> (AbortHandle, tokio::task::JoinHandle<()>) {
        let join = tokio::spawn(async {
            // Park until aborted.
            std::future::pending::<()>().await;
        });
        (join.abort_handle(), join)
    }

    #[tokio::test]
    async fn stop_returns_true_and_removes_for_registered_id() {
        let m = ClaudeManager::default();
        let (handle, join) = dummy_handle().await;
        m.register("sess-1".to_string(), handle);

        // Owns the session → true, and the pump task is aborted.
        assert!(m.stop("sess-1"));
        // A second stop is idempotent: the id is gone → false.
        assert!(!m.stop("sess-1"));

        // The abort actually took effect on the underlying task.
        let _ = join.await; // a cancelled task resolves to a JoinError
    }

    #[tokio::test]
    async fn stop_returns_false_for_unknown_id() {
        // An unknown / codex-owned id is NOT owned here → false (the command then
        // falls through to the codex interrupt path).
        let m = ClaudeManager::default();
        assert!(!m.stop("never-registered"));
    }

    #[tokio::test]
    async fn deregister_does_not_abort_and_leaves_stop_false() {
        let m = ClaudeManager::default();
        let (handle, join) = dummy_handle().await;
        m.register("sess-2".to_string(), handle);
        // The pump finishing normally deregisters itself; a later stop finds nothing.
        m.deregister("sess-2");
        assert!(!m.stop("sess-2"));
        // deregister did NOT abort — the task is still running until we drop it here.
        assert!(!join.is_finished());
        join.abort();
    }

    #[tokio::test]
    async fn shutdown_aborts_all_registered() {
        let m = ClaudeManager::default();
        let (h1, j1) = dummy_handle().await;
        let (h2, j2) = dummy_handle().await;
        m.register("a".to_string(), h1);
        m.register("b".to_string(), h2);
        m.shutdown();
        // Both removed → stop now returns false for each.
        assert!(!m.stop("a"));
        assert!(!m.stop("b"));
        // Both tasks were aborted.
        let _ = j1.await;
        let _ = j2.await;
    }
}
