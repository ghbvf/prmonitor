//! codex app-server adapter (the MVP [`crate::review::engine::ReviewEngine`] impl).
//!
//! Spawns `codex app-server --stdio` and speaks its newline-delimited JSON-RPC
//! (no `jsonrpc` field). Implemented in PR5:
//! - `process` — spawn + manage the child, stdin/stdout pipes, stderr capture
//! - `codec` — NDJSON line framing
//! - `rpc` — request/response demux (`id` → oneshot) + notification broadcast
//! - `protocol` — typed subset of the protocol we use
//! - `manager` — the resident connection handle held in `AppState`

pub mod codec;
pub mod manager;
pub mod process;
pub mod protocol;
pub mod rpc;

pub use manager::CodexManager;
pub use process::{CodexProcess, CodexStatus};
pub use rpc::{RpcClient, RpcError};
