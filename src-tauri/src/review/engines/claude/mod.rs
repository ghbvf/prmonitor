//! `claude -p` headless review engine (#718) — the second
//! [`crate::review::engine::ReviewEngine`] impl, selectable per-project alongside
//! the codex MVP.
//!
//! Unlike codex (a resident `app-server` driven over JSON-RPC), `claude -p` is a
//! ONE-SHOT subprocess per review (Claude Code headless print mode). There is no
//! persistent connection: each review spawns `claude -p "<prompt>" --output-format
//! stream-json …`, parses its line-delimited `stream_event` envelope into the
//! existing [`crate::events::ReviewEvent`]s, and exits.
//!
//! - `process` — spawn the child + the PURE `stream-json` line parser (the key
//!   testable seam: parse-without-spawning) + the review prompt builder
//! - `manager` — [`ClaudeManager`], the `AppState` handle that owns each live
//!   session's kill handle so `stop` can terminate it by session id
//! - `engine` — the thin [`crate::review::engine::ReviewEngine`] adapter; the
//!   spawn + parse + pump + registry-driving orchestration lives in `engine`
//!   (mirroring how codex's lives in `review::session`, but self-contained).
//!
//! The slice REUSES the engine-agnostic primitives — [`crate::review::session::SessionRegistry`]
//! (dedup + lifecycle), [`crate::review::history_store`] (persistence), and
//! [`crate::events::ReviewEvent`] (no new event types) — so it never duplicates the
//! dedup or wire contract.

pub mod engine;
pub mod manager;
pub mod process;

pub use engine::ClaudeEngine;
pub use manager::ClaudeManager;
