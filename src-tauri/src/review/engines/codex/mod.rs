//! codex app-server adapter (the MVP [`crate::review::engine::ReviewEngine`] impl).
//!
//! Spawns `codex app-server --stdio` and speaks its newline-delimited JSON-RPC
//! (no `jsonrpc` field):
//! - `process` — spawn + manage the child, stdin/stdout pipes, stderr capture
//! - `codec` — NDJSON line framing
//! - `rpc` — request/response demux (`id` → oneshot) + notification broadcast,
//!   plus auto-answering server→client (reverse approval) requests
//! - `protocol` — typed subset of the protocol we use
//! - `manager` — the resident connection handle held in `AppState`
//! - `engine` — the [`crate::review::engine::ReviewEngine`] impl (PR6)

pub mod codec;
pub mod engine;
pub mod manager;
pub mod process;
pub mod protocol;
pub mod rpc;

pub use engine::CodexEngine;
pub use manager::CodexManager;
pub use process::{CodexProcess, CodexStatus};
pub use rpc::{RpcClient, RpcError};
