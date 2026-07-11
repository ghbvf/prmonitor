//! Config slice: persisted application configuration and its Tauri commands.
//!
//! Self-contained: `commands` (handlers) → `service` (logic) → `model` (domain).
//! Persistence is the SQLite `config_blob` table (#70); tauri-plugin-store is retained
//! only for the one-time legacy import.

mod cli;
pub mod commands;
pub mod model;
pub mod service;
