//! Remote Access listener runtime (AB#1225) — a composition horizontal that turns the
//! declarative AB#1064 `config.listeners[]` model into bound loopback listeners.
//!
//! NOT a vertical slice: it names sibling slices directly (mounting review/terminal routers on
//! loopback ports), the same way `dispatch.rs` glues `pr`→`review`. The slice-boundary test scans
//! vertical slice dirs; this module is intentionally outside them.

pub mod commands;
pub mod status;
pub mod supervisor;
pub mod terminal_http;
