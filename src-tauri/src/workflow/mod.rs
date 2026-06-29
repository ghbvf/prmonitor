//! Durable workflow / saga orchestration (#1370).
//!
//! The workflow slice owns persistence, lifecycle, and frontend commands for multi-step flows. It
//! does not name sibling slices (`review`, `outbox`, `notification`): composition-root-injected
//! actions in [`manager`] perform cross-slice work.

pub mod commands;
pub mod manager;
pub mod service;
pub mod store;
