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
//! - `backend` — the `TerminalBackend` trait seam (now with TWO live impls).
//! - `iterm` — the iTerm `TerminalBackend` impl + the pure `map_notification`.
//! - `webpty` / `webpty_manager` — the #1372 Web PTY `TerminalBackend` impl + its resident
//!   `portable-pty` session pool (cross-platform: unix openpty + Windows ConPTY).
//! - `commands` — the Tauri command surface + the backend ROUTING hub (registered in `lib.rs`).
//!
//! **Preconditions for the live daemon** (a missing one surfaces as a structured handshake
//! error with an actionable Chinese message, never a silent failure): iTerm's
//! "Enable Python API" (Preferences → General → Magic), first-run API authorization, and
//! `pip install iterm2` for the interpreter `python3` resolves to.
//!
//! Remote Web Terminal exposure lives in the `remote` horizontal: `ListenerKind::Terminal`
//! binds a loopback-only HTTP router, tunnels expose it publicly, and terminal audit rows record
//! metadata only. The SECOND backend (`WebPty`, #1372) is now LIVE: the reserved seam is realized
//! as the sealed [`crate::model::TerminalBackendKind`] enum (the Hard carrier) driving an
//! exhaustive `match` in `commands::RoutedBackend` — adding a third backend without handling it
//! everywhere is a compile error. Both backends coexist in one merged session list; create-time
//! picks the backend; per-session ops route to the owner via `WebPtyManager::owns`.

pub mod backend;
pub mod codec;
pub mod commands;
pub mod iterm;
pub mod manager;
pub mod process;
pub mod protocol;
pub mod rpc;
pub mod webpty;
pub mod webpty_manager;
