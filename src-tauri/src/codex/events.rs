//! Maps codex app-server `ServerNotification`s into [`crate::events::ReviewEvent`]s.
//!
//! Implemented in PR6: aggregates `item/agentMessage/delta`,
//! `item/reasoning/textDelta`, `turn/completed`, `error`, etc. (decoding
//! `delta` / `deltaBase64`) and routes them per `threadId`.
