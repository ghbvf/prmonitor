//! Cursor ACP adapter ([`crate::review::engine::ReviewEngine`] impl).
//!
//! Spawns `agent acp` and speaks JSON-RPC 2.0 NDJSON over stdio (**must** include
//! `"jsonrpc":"2.0"` — unlike the codex app-server, which omits it):
//! - `process` — spawn + manage the child, stdin/stdout pipes, stderr capture
//! - `codec` — NDJSON line framing with the `jsonrpc` field
//! - `rpc` — request/response demux (`id` → oneshot) + notification broadcast,
//!   plus auto-answering server→client permission / Cursor extension requests
//! - `protocol` — typed subset of the ACP + Cursor extensions we use
//! - `manager` — the resident connection handle held in `AppState`
//! - `engine` — the [`crate::review::engine::ReviewEngine`] impl (self-contained
//!   session pump, mirroring Claude's layout while reusing [`SessionRegistry`])

pub mod codec;
pub mod engine;
pub mod manager;
pub mod process;
pub mod protocol;
pub mod rpc;

pub use engine::CursorEngine;
pub use manager::CursorManager;
pub use process::CursorStatus;
