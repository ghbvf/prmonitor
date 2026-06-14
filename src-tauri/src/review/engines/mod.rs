//! Review-engine implementations of the [`super::engine::ReviewEngine`] port.
//!
//! Convention: one submodule per engine (`codex` = codex app-server MVP; a
//! future `claude` engine plugs in here). Engines depend only on the port and
//! emit [`crate::events::ReviewEvent`]s; the rest of the slice never names a
//! concrete engine.

pub mod codex;
