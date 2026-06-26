//! `terminal` slice (#1383) — an iTerm-backed terminal panel for the desktop app.
//!
//! PR1 (this slice) is the desktop end-to-end: the frontend xterm panel ↔ a long-resident
//! `python3 iterm_daemon.py` daemon spoken over NDJSON JSON-RPC (the codex-transport
//! pattern), driving the iTerm2 Python API. The slice is self-contained: it shares only the
//! horizontals (`crate::model` for the `TerminalSession` / `CreateSessionOpts` contract,
//! `crate::error`, `crate::events` for `TerminalEvent`, `crate::stream` for the emit funnel,
//! `crate::state` for the resident manager handle), NEVER a sibling slice.
//!
//! Layers (mirroring `review::engines::codex`):
//! - `protocol` — typed daemon JSON-RPC subset (method consts, request params, results, the
//!   total `ServerNotification` classifier).
//! - `codec` / `rpc` — the minimal NDJSON request/response demux + notification broadcast
//!   (an independent copy of the codex transport; the slice boundary forbids `crate::review::`).
//! - `process` — spawn + handshake the `python3` child; `TerminalDaemonStatus`.
//! - `manager` — the resident connection handle held in `AppState`, plus the per-connection
//!   notification pump.
//! - `backend` — the `TerminalBackend` trait seam (the future-WebPty extension point).
//! - `iterm` — the iTerm `TerminalBackend` impl + the pure `map_notification`.
//! - `commands` — the Tauri command surface (registered in `lib.rs`).
//!
//! **Preconditions for the live daemon** (a missing one surfaces as a structured handshake
//! error with an actionable Chinese message, never a silent failure): iTerm's
//! "Enable Python API" (Preferences → General → Magic), first-run API authorization, and
//! `pip install iterm2` for the interpreter `python3` resolves to.
//!
//! **PR2 forward-compat (NOT this PR):** exposing the terminal over the Remote Web Console is
//! a `remote::supervisor` `ListenerKind::Terminal` binder + tunnel exposure + an audit trail;
//! the `StreamEvent::Terminal` bus envelope already feeds an SSE consumer with no producer
//! change. A SECOND backend (e.g. WebPty) is added by a sealed `TerminalBackendKind` enum + an
//! exhaustive `match` at the command layer (the Hard carrier) — NOT now (single iTerm backend).

pub mod backend;
pub mod codec;
pub mod commands;
pub mod iterm;
pub mod manager;
pub mod process;
pub mod protocol;
pub mod rpc;
