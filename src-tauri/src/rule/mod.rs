//! Rule-engine slice (#1371): match normalized inbox events to durable outbox actions.
//!
//! The slice consumes only cross-slice contracts (`Event`, `Candidate`, `ActionKind`) and
//! public config service types. It never executes side effects itself; it only enqueues
//! outbox rows and records the inbox→rule→outbox trace.

pub mod commands;
pub mod service;
pub mod store;
