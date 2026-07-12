//! Review-engine implementations of the [`super::engine::ReviewEngine`] port.
//!
//! Convention: one submodule per engine (`codex` = codex app-server MVP;
//! `claude` = `claude -p` headless, #718; `cursor` = Cursor ACP via `agent acp`).
//! Engines depend only on the port and emit [`crate::events::ReviewEvent`]s; the
//! rest of the slice never names a concrete engine.

pub mod claude;
pub mod codex;
pub mod cursor;
