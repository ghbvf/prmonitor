//! Config slice: persisted application configuration and its Tauri commands.
//!
//! Self-contained: `commands` (handlers) → `service` (logic) → `model` (domain).
//! PR2 wires `tauri-plugin-store` persistence into `service`.

pub mod commands;
pub mod model;
pub mod service;
