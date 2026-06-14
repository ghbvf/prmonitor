//! Review session state machine + session manager.
//!
//! Implemented in PR6: one `codex app-server` process, multiple sessions keyed
//! by `threadId` (Idle/Starting/Running/Interrupting/Done/Failed); `turn/start`
//! with the pr-review skill input; `turn/interrupt` to stop.
