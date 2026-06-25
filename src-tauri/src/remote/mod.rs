//! Remote Access listener runtime (AB#1225) — a composition horizontal that turns the
//! declarative AB#1064 `config.listeners[]` model into bound loopback listeners.
//!
//! NOT a vertical slice: it names `crate::review::local_api` directly (mounting that router on the
//! port it binds), the same way `dispatch.rs` glues `pr`→`review`. The slice-boundary test scans
//! only `config`/`pr`/`review`/`inbox`/`outbox`, so this module is intentionally outside it.

pub mod commands;
pub mod status;
pub mod supervisor;
