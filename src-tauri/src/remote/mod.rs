//! Remote Access runtime (#1553) — a composition horizontal that turns
//! `remoteAccess.entrypoints[]` into bound entrypoint routers and optional tunnels.
//!
//! NOT a vertical slice: it names sibling slices directly (mounting review/terminal routers on
//! entrypoint ports), the same way `dispatch.rs` glues `pr`→`review`. The slice-boundary test scans
//! vertical slice dirs; this module is intentionally outside them.

pub mod commands;
pub mod status;
pub mod supervisor;
pub mod terminal_http;
