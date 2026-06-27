//! Resident Web PTY session registry — a long-lived handle held in `AppState` (#1372).
//!
//! The SECOND terminal backend's owned state (the first is the iTerm daemon in
//! [`super::manager::ITermDaemonManager`]). Each session is a `portable-pty`-spawned shell
//! child plus a dedicated blocking reader thread that pushes raw output into a per-session
//! scrollback ring AND, while a panel is subscribed, forwards each chunk to the frontend as a
//! base64 [`TerminalEvent::Output`] through the [`crate::stream::emit`] funnel (NEVER a direct
//! `app.emit` — the funnel scan forbids it).
//!
//! Cross-platform: `portable-pty` wraps unix `openpty` + Windows ConPTY, so the same registry
//! drives mac + Windows. Killed on app shutdown ([`Self::shutdown`], wired to `RunEvent` in
//! `lib.rs`) — the shell children never outlive the app.
//!
//! **Concurrency contract.** A single per-session lock over the scrollback ring serializes the
//! reader's `push`+live-emit against [`Self::subscribe`]'s snapshot-replay+flip, so an attaching
//! panel sees the backlog exactly once with no gap or duplicate vs the live stream. Teardown
//! (natural child exit OR [`Self::close`]) funnels through [`finish_session`], guarded by a
//! run-once `torn_down` flag so deregister + `SessionEnded` happen exactly once. `kill` (via an
//! independent [`ChildKiller`]) and `wait` (the reaper) use SEPARATE locks, so killing a child
//! never blocks behind a reaper waiting on it.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use base64::Engine;
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tauri::{AppHandle, Runtime};

use super::webpty::default_shell_program;
use crate::error::{AppError, AppResult};
use crate::events::{StreamEvent, TerminalEvent};
use crate::model::{CreateSessionOpts, TerminalBackendKind, TerminalSession};

/// Default grid for a freshly spawned shell (the panel resizes to fit after attach).
/// `CreateSessionOpts` carries no rows/cols, so spawn always starts at this size.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Read buffer for the per-session reader thread (bounds a single `read` syscall's copy).
const READ_BUF: usize = 32 * 1024;

/// Per-session scrollback retained for replay on (re)attach. 256 KiB is generous for a screen+
/// of backlog without unbounded growth; the ring drops the oldest bytes past this.
/// mirrors RAW_REPLAY_MAX_BYTES in src/terminal/useTerminalStore.ts
const SCROLLBACK_CAP: usize = 256 * 1024;

/// Max concurrent Web PTY sessions (SEC-2). Each session is a live shell child + a reader thread,
/// so an unbounded `create` loop over the loopback HTTP listener could exhaust fds / threads; this
/// caps it. A configurable per-listener cap is a possible follow-up (#1372).
const MAX_CONCURRENT_SESSIONS: usize = 16;

/// Monotonic session-id source. The `webpty-<n>` id prefix is COSMETIC — routing is ALWAYS via
/// [`WebPtyManager::owns`], never id-parsing — so a process-unique counter (not a uuid dep) is
/// enough. Module-global so two `WebPtyManager`s (e.g. a test one + `AppState`'s) never collide.
static NEXT_PTY_ID: AtomicU64 = AtomicU64::new(0);

/// A byte ring buffer holding the most recent PTY output for replay on attach. Drops the oldest
/// bytes once it exceeds `cap`, preserving the tail (the most recent screen) in order.
struct ScrollbackRing {
    buf: VecDeque<u8>,
    cap: usize,
}

impl ScrollbackRing {
    fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap,
        }
    }

    /// Append `bytes`, dropping from the front until at most `cap` bytes remain (keeps the tail).
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes.iter().copied());
        while self.buf.len() > self.cap {
            self.buf.pop_front();
        }
    }

    /// A contiguous copy of the retained bytes, oldest-first.
    fn snapshot(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }
}

/// The per-session state shared between the reader thread and the manager's subscribe path: the
/// scrollback ring and the subscribed flag. The single `ring` lock is the serialization point
/// that makes subscribe-replay vs live-append race-free.
struct SessionShared {
    ring: Mutex<ScrollbackRing>,
    /// Whether a panel is attached (live `Output` events are emitted only while true). Flipped
    /// to `true` UNDER the `ring` lock in [`WebPtyManager::subscribe`] so the backlog replay and
    /// the first live frame can't interleave.
    subscribed: AtomicBool,
}

/// One live Web PTY session: the master pty (for resize), the writer (keystrokes), the child (for
/// reaping) + an independent killer (so kill never contends the reaper's child lock), the live
/// metadata row, the shared ring/subscribed state, and the reader thread handle.
struct PtySession {
    /// The registry key + the id echoed in every event. Immutable (no lock needed).
    id: String,
    /// `Option` so [`WebPtyManager::shutdown`] can `take()` + drop the master to close the pty fd
    /// and unblock the blocking reader. `None` once closing.
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    writer: Mutex<Box<dyn Write + Send>>,
    /// Held only by the reaper (`wait`); kept separate from `killer` so a `kill` never blocks
    /// behind a `wait` on a not-yet-dead child.
    child: Mutex<Box<dyn Child + Send + Sync>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// Live metadata (grid mutates on resize). `Mutex` rather than the spec's bare
    /// `TerminalSession` so `resize` updates the single source `list` reads — no duplicated grid.
    meta: Mutex<TerminalSession>,
    shared: Arc<SessionShared>,
    /// The reader thread's handle, retained for ownership; NEVER joined (teardown detaches — app
    /// exit must not hang on a blocking read). `OnceLock` since it's written EXACTLY once
    /// (post-spawn) and never read back (DX-2).
    reader: OnceLock<JoinHandle<()>>,
    /// Set by [`WebPtyManager::close`] before the kill, so teardown reports `reason: "killed"`
    /// (vs `"exited"` for a natural child exit).
    closing: AtomicBool,
    /// Run-once teardown guard: the first of {reader-loop EOF, `close`, `shutdown`} to flip it
    /// owns deregister + `SessionEnded`; the rest are no-ops. (A dedicated flag, NOT `closing` —
    /// a natural exit has `closing == false` but must still deregister exactly once.)
    torn_down: AtomicBool,
}

/// The resident Web PTY session registry held in `AppState`. `&self` methods + interior
/// mutability so it stays `Default` alongside the other `AppState` managers.
#[derive(Default)]
pub struct WebPtyManager {
    sessions: Arc<Mutex<HashMap<String, Arc<PtySession>>>>,
    /// In-flight `spawn_program` reservations (F3): the cap bounds `sessions.len() + reserved`, so
    /// concurrent creates can't each pass the `len()` check before any `insert` and bypass the cap.
    /// Incremented under the `sessions` lock at reserve time; released by the spawn cleanup guard.
    reserved: AtomicUsize,
}

/// RAII cleanup for an in-flight `spawn_program` (F2/F3): releases the cap RESERVATION on every drop
/// (balanced with the `fetch_add` at reserve time), and — while it still holds the freshly-spawned
/// `child` (the window between `spawn_command` and registering the session) — kills + detached-reaps
/// that child so a `try_clone_reader` / `take_writer` failure can't LEAK it (F2). `take_child`
/// disarms the child cleanup once the child is handed to the registered `PtySession`.
struct SpawnCleanup<'a> {
    reserved: &'a AtomicUsize,
    child: Option<Box<dyn Child + Send + Sync>>,
}

impl<'a> SpawnCleanup<'a> {
    /// Arm the reservation release (call right after `reserved.fetch_add`).
    fn new(reserved: &'a AtomicUsize) -> Self {
        Self {
            reserved,
            child: None,
        }
    }

    /// Hand the freshly-spawned child to the guard so an early return kills + reaps it.
    fn arm_child(&mut self, child: Box<dyn Child + Send + Sync>) {
        self.child = Some(child);
    }

    /// Reclaim the child (disarm child cleanup) as it is moved into the registered session.
    fn take_child(&mut self) -> Box<dyn Child + Send + Sync> {
        self.child
            .take()
            .expect("child was armed before take_child")
    }
}

impl Drop for SpawnCleanup<'_> {
    fn drop(&mut self) {
        // Always release the reservation slot (so a failed create never leaks one).
        self.reserved.fetch_sub(1, Ordering::AcqRel);
        // Still holding the child ⇒ the create failed before registration: SIGKILL + detached-reap
        // so the orphaned shell can't linger as a zombie.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

impl WebPtyManager {
    /// Spawn a new PTY shell session (the default shell), returning its row. The grid starts at
    /// 80×24; the panel resizes to fit after attach. Generic over the runtime so the reader pump
    /// can capture an owned `AppHandle<R>` for its emit calls (mirrors `ITermDaemonManager`).
    pub fn spawn<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        opts: &CreateSessionOpts,
    ) -> AppResult<TerminalSession> {
        self.spawn_program(app, opts, &default_shell_program())
    }

    /// Spawn body taking an explicit shell program — the seam the unit tests drive with `/bin/sh`
    /// for determinism (so a test never depends on the host `$SHELL`).
    fn spawn_program<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        _opts: &CreateSessionOpts,
        program: &str,
    ) -> AppResult<TerminalSession> {
        // SEC-2 + F3: RESERVE a slot UNDER the registry lock BEFORE opening a PTY. The cap bounds
        // `sessions.len() + reserved`, so concurrent creates can't each pass a `len()`-only check
        // before any `insert` and bypass the cap (the check + reserve are atomic under the lock).
        {
            let map = self.sessions.lock().unwrap();
            check_session_cap(map.len() + self.reserved.load(Ordering::Acquire))?;
            self.reserved.fetch_add(1, Ordering::AcqRel);
        }
        // From here EVERY early return must release the reservation (F3) — and a failure after the
        // child spawns must kill+reap it (F2). `cleanup` (RAII) guarantees both: it releases the
        // reservation on drop, and (while armed) kills+reaps the child it holds.
        let mut cleanup = SpawnCleanup::new(&self.reserved);

        // `_opts` (windowId / profile / backend) is intentionally unused (DX-4): a WebPTY shell
        // takes neither an iTerm window nor a profile, and the backend was already matched by the
        // caller. Selecting the shell / env / command from `opts` is a documented follow-up
        // (#1372 command-session, deferred).
        let cols = DEFAULT_COLS;
        let rows = DEFAULT_ROWS;
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| AppError::new(format!("无法打开 PTY: {e}")))?;

        let cmd = CommandBuilder::new(program);
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| AppError::new(format!("无法启动终端进程 {program}: {e}")))?;
        // Drop OUR slave handle so only the child holds the slave end — the master reader then
        // EOFs when the child exits (the natural-teardown trigger).
        drop(pair.slave);
        // Hand the child to the cleanup guard: a `try_clone_reader` / `take_writer` failure below
        // now kills + reaps it instead of leaking it (F2).
        cleanup.arm_child(child);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| AppError::new(format!("无法获取 PTY 读取端: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| AppError::new(format!("无法获取 PTY 写入端: {e}")))?;
        // Reclaim the child (disarm the guard's child cleanup) — it is now owned by the session
        // below. The guard still releases the reservation on drop.
        let child = cleanup.take_child();
        let killer = child.clone_killer();

        let id = format!("webpty-{}", NEXT_PTY_ID.fetch_add(1, Ordering::Relaxed));
        let meta = TerminalSession {
            session_id: id.clone(),
            window_id: "webpty".to_string(),
            tab_id: "webpty".to_string(),
            title: shell_basename(program),
            is_active: true,
            rows,
            cols,
            backend: TerminalBackendKind::WebPty,
        };
        let shared = Arc::new(SessionShared {
            ring: Mutex::new(ScrollbackRing::new(SCROLLBACK_CAP)),
            subscribed: AtomicBool::new(false),
        });
        let session = Arc::new(PtySession {
            id: id.clone(),
            master: Mutex::new(Some(pair.master)),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            killer: Mutex::new(killer),
            meta: Mutex::new(meta.clone()),
            shared,
            reader: OnceLock::new(),
            closing: AtomicBool::new(false),
            torn_down: AtomicBool::new(false),
        });

        // Register BEFORE spawning the pump so `owns`/`list` (and the pump's teardown lookup) see
        // it immediately.
        self.sessions
            .lock()
            .unwrap()
            .insert(id.clone(), session.clone());

        // One blocking reader thread per session — `portable-pty`'s reader is sync `io::Read`, so
        // it can't share the async runtime. It captures an owned `AppHandle<R>` for emit.
        let app_for_reader = app.clone();
        let sessions_for_reader = self.sessions.clone();
        let session_for_reader = session.clone();
        match std::thread::Builder::new()
            .name(format!("webpty-reader-{id}"))
            .spawn(move || {
                reader_loop(
                    app_for_reader,
                    sessions_for_reader,
                    session_for_reader,
                    reader,
                )
            }) {
            Ok(handle) => {
                // Written exactly once; `Err` (already set) is impossible here.
                let _ = session.reader.set(handle);
            }
            Err(e) => {
                // Could not start the pump — deregister, kill, AND reap on a detached thread (REL-2:
                // symmetric with `finish_session`, so a killed-but-unwaited child can't linger as a
                // zombie — `Child` Drop is not a reap guarantee).
                self.sessions.lock().unwrap().remove(&id);
                let _ = session.killer.lock().unwrap().kill();
                let reaper = session.clone();
                std::thread::spawn(move || {
                    let _ = reaper.child.lock().unwrap().wait();
                });
                return Err(AppError::new(format!("无法启动 PTY 读取线程: {e}")));
            }
        }

        Ok(meta)
    }

    /// Attach a panel: emit a one-shot `Attached` (seeds the xterm grid), replay the scrollback
    /// ring as a single `Output`, and flip `subscribed = true` — replay + flip UNDER the ring
    /// lock so a concurrent reader append can't interleave (no gap, no duplicate vs live frames).
    pub fn subscribe<R: Runtime>(&self, app: &AppHandle<R>, id: &str) -> AppResult<()> {
        let session = self.get(id)?;
        let (cols, rows) = {
            let meta = session.meta.lock().unwrap();
            (meta.cols, meta.rows)
        };
        // REL-1: catch an emit panic so it can't unwind through the ring lock below (poisoning it)
        // or abort the command mid-way; surface it as an `AppError` instead.
        if !emit_terminal(
            app,
            TerminalEvent::Attached {
                session_id: id.to_string(),
                cols,
                rows,
            },
        ) {
            return Err(AppError::new("终端事件发送失败（attached）".to_string()));
        }
        // Single critical section: snapshot the backlog, replay it, THEN flip subscribed — so the
        // reader's `push`+live-emit (which locks the same ring) is strictly before or after this.
        let ring = session.shared.ring.lock().unwrap();
        let snapshot = ring.snapshot();
        if !snapshot.is_empty()
            && !emit_terminal(
                app,
                TerminalEvent::Output {
                    session_id: id.to_string(),
                    data: encode_base64(&snapshot),
                },
            )
        {
            // The replay emit panicked (caught INSIDE the ring lock → the guard drops un-poisoned,
            // preserving the no-gap invariant); abort WITHOUT flipping subscribed so the session
            // stays un-attached + consistent.
            drop(ring);
            return Err(AppError::new(
                "终端事件发送失败（output replay）".to_string(),
            ));
        }
        session.shared.subscribed.store(true, Ordering::Release);
        drop(ring);
        Ok(())
    }

    /// Detach a panel: stop emitting live `Output` (the session keeps running, the ring keeps
    /// filling for the next attach). A stray in-flight frame after this is harmless.
    pub fn unsubscribe(&self, id: &str) -> AppResult<()> {
        let session = self.get(id)?;
        session.shared.subscribed.store(false, Ordering::Release);
        Ok(())
    }

    /// Write keystrokes / pasted bytes to the session's pty.
    pub fn write(&self, id: &str, bytes: &[u8]) -> AppResult<()> {
        let session = self.get(id)?;
        let mut writer = session.writer.lock().unwrap();
        writer
            .write_all(bytes)
            .map_err(|e| AppError::new(format!("写入终端失败: {e}")))?;
        writer
            .flush()
            .map_err(|e| AppError::new(format!("刷新终端失败: {e}")))?;
        Ok(())
    }

    /// Resize the session's pty grid and update the stored metadata (so `list` reflects it).
    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> AppResult<()> {
        let session = self.get(id)?;
        {
            let guard = session.master.lock().unwrap();
            let Some(master) = guard.as_ref() else {
                return Err(AppError::new("终端会话正在关闭".to_string()));
            };
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| AppError::new(format!("调整终端尺寸失败: {e}")))?;
        }
        let mut meta = session.meta.lock().unwrap();
        meta.cols = cols;
        meta.rows = rows;
        Ok(())
    }

    /// Stop a session: mark it closing (so teardown reports `"killed"`), SIGKILL the child, and
    /// run teardown (deregister + reap + `SessionEnded`) synchronously, so `owns` is false the
    /// moment this returns. The blocked reader then EOFs and finds teardown already done.
    pub fn close<R: Runtime>(&self, app: &AppHandle<R>, id: &str) -> AppResult<()> {
        let session = self.get(id)?;
        session.closing.store(true, Ordering::Release);
        let _ = session.killer.lock().unwrap().kill();
        finish_session(app, &self.sessions, &session);
        Ok(())
    }

    /// All live sessions' metadata rows (the merged-list contribution; HashMap order).
    ///
    /// Lock ordering (DX-5): `sessions` → `meta` (acquire the registry, then each session's `meta`);
    /// never the inverse — no path holds `meta` then acquires `sessions`, so this nesting can't
    /// invert into a deadlock.
    pub fn list(&self) -> Vec<TerminalSession> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .map(|s| s.meta.lock().unwrap().clone())
            .collect()
    }

    /// THE routing predicate: whether this backend owns `id`. The command layer routes per-session
    /// ops by this (never by parsing the id), so the `webpty-` prefix stays cosmetic.
    pub fn owns(&self, id: &str) -> bool {
        self.sessions.lock().unwrap().contains_key(id)
    }

    /// Kill every session's child on app shutdown (sync `RunEvent` handler): mark each torn down
    /// (so its reader's EOF teardown is a silent no-op), SIGKILL, drop the master to close the pty
    /// fd (unblocking the blocking reader), and reap on a DETACHED thread — NEVER join, so app exit
    /// can't hang on a blocking read. No `SessionEnded` emits (the app is going away).
    pub fn shutdown(&self) {
        // REL-4: this runs in `RunEvent::Exit`; a poisoned mutex `.unwrap()` here would double-panic
        // → abort. `unwrap_or_else(into_inner)` makes shutdown poison-tolerant so app exit always
        // completes the kill/reap.
        let sessions: Vec<Arc<PtySession>> = {
            let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            map.drain().map(|(_, v)| v).collect()
        };
        for session in sessions {
            session.torn_down.store(true, Ordering::Release);
            let _ = session
                .killer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .kill();
            // Drop the master → pty fd closes → reader EOFs.
            session
                .master
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            let reaper = session.clone();
            std::thread::spawn(move || {
                let _ = reaper
                    .child
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .wait();
            });
        }
    }

    /// Look up a session or return an actionable not-found error.
    fn get(&self, id: &str) -> AppResult<Arc<PtySession>> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| AppError::new(format!("终端会话不存在: {id}")))
    }

    /// Test-only: observe a session's scrollback WITHOUT a Tauri emit sink, so PTY round-trip
    /// tests can assert output landed in the ring without subscribing (which would emit).
    #[cfg(test)]
    pub(crate) fn scrollback_snapshot(&self, id: &str) -> Option<Vec<u8>> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .map(|s| s.shared.ring.lock().unwrap().snapshot())
    }

    /// Test-only: the stored grid `(cols, rows)`, to assert `resize` updated it.
    #[cfg(test)]
    pub(crate) fn session_grid(&self, id: &str) -> Option<(u16, u16)> {
        self.sessions.lock().unwrap().get(id).map(|s| {
            let meta = s.meta.lock().unwrap();
            (meta.cols, meta.rows)
        })
    }

    /// Test-only: the `subscribed` flag, to assert subscribe/unsubscribe flip it.
    #[cfg(test)]
    pub(crate) fn is_subscribed(&self, id: &str) -> Option<bool> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .map(|s| s.shared.subscribed.load(Ordering::Acquire))
    }

    /// Test-only: the in-flight reservation count, to assert a create (success or failure) leaks
    /// none (F3).
    #[cfg(test)]
    pub(crate) fn reserved_count(&self) -> usize {
        self.reserved.load(Ordering::Acquire)
    }
}

/// Base64-encode raw PTY bytes for the `Output` wire field (byte-lossless — a PTY stream is not
/// guaranteed valid UTF-8, so it is NOT `from_utf8_lossy`d).
fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Reject a new session once the concurrent-session cap is reached (SEC-2/F3). Pure — takes the
/// current `active + reserved` count, so it's unit-testable without spawning shells.
fn check_session_cap(current: usize) -> AppResult<()> {
    if current >= MAX_CONCURRENT_SESSIONS {
        return Err(AppError::new(format!(
            "已达 PTY 会话上限（{MAX_CONCURRENT_SESSIONS} 个）：请先关闭已有会话再新建"
        )));
    }
    Ok(())
}

/// Emit a terminal event through the [`crate::stream::emit`] funnel, catching a panic from the emit
/// so it can't unwind through a held lock (REL-1): an emit panic (e.g. an OOM in the broadcast /
/// serialize path) would otherwise poison the ring mutex AND kill the reader thread with no
/// `SessionEnded`. Returns `false` if the emit panicked (the caller logs + tears down, or surfaces
/// an error). `AssertUnwindSafe` because `AppHandle` isn't `UnwindSafe` and we are deliberately
/// recovering rather than propagating.
fn emit_terminal<R: Runtime>(app: &AppHandle<R>, event: TerminalEvent) -> bool {
    catch_unwind(AssertUnwindSafe(|| {
        crate::stream::emit(app, StreamEvent::Terminal(event));
    }))
    .is_ok()
}

/// Whether a reader `read` error is a NORMAL pty-close EOF (vs an unexpected I/O fault worth a log
/// line before teardown, REL-3). A closed pty master surfaces as `EIO` (5) on unix, or one of the
/// broken-pipe error kinds; anything else is unexpected.
fn is_normal_pty_eof(err: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    if matches!(
        err.kind(),
        ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof
    ) {
        return true;
    }
    #[cfg(unix)]
    if err.raw_os_error() == Some(5) {
        return true; // EIO — the expected result of reading a closed pty master.
    }
    false
}

/// The shell's basename, used as the session title (e.g. `/bin/zsh` → `zsh`).
fn shell_basename(program: &str) -> String {
    std::path::Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program)
        .to_string()
}

/// The per-session blocking reader pump: drain the master, push every chunk into the scrollback
/// ring, and — while subscribed — forward it as a base64 `Output` through the [`crate::stream::emit`]
/// funnel. The ring lock is held across the subscribed check + emit so a concurrent
/// [`WebPtyManager::subscribe`] replay can't interleave. On EOF / read error (the child exited or
/// the master was dropped) it runs [`finish_session`] once.
fn reader_loop<R: Runtime>(
    app: AppHandle<R>,
    sessions: Arc<Mutex<HashMap<String, Arc<PtySession>>>>,
    session: Arc<PtySession>,
    mut reader: Box<dyn Read + Send>,
) {
    let mut buf = [0u8; READ_BUF];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF: the child exited or the master was dropped.
            Err(e) => {
                // REL-3: a closed pty master is a NORMAL EOF (EIO / broken-pipe); anything else is
                // an unexpected fault worth a log line before we tear down. Either way we stop.
                if !is_normal_pty_eof(&e) {
                    eprintln!(
                        "webpty reader unexpected read error (session {}): {e}",
                        session.id
                    );
                }
                break;
            }
            Ok(n) => {
                let chunk = &buf[..n];
                // Single critical section over the ring: push, then (if subscribed) emit — held
                // across the emit so subscribe's snapshot+flip is atomic relative to this append.
                let mut ring = session.shared.ring.lock().unwrap();
                ring.push(chunk);
                if session.shared.subscribed.load(Ordering::Acquire)
                    && !emit_terminal(
                        &app,
                        TerminalEvent::Output {
                            session_id: session.id.clone(),
                            data: encode_base64(chunk),
                        },
                    )
                {
                    // REL-1: the emit panicked, but `catch_unwind` (inside `emit_terminal`) caught it
                    // INSIDE the ring lock — so the guard `break` drops un-poisoned and the no-gap
                    // invariant holds. Tear down normally rather than unwinding through the held lock
                    // (which would poison the ring + skip `SessionEnded`).
                    eprintln!(
                        "webpty reader emit panicked (session {}); tearing down",
                        session.id
                    );
                    break;
                }
            }
        }
    }
    finish_session(&app, &sessions, &session);
}

/// Tear a session down EXACTLY once (run-once `torn_down` guard): deregister it, SIGKILL + reap
/// the child on a detached thread (mirrors `process.rs::kill_and_reap` — never block the caller),
/// and emit `SessionEnded` (`"killed"` if [`WebPtyManager::close`] set `closing`, else `"exited"`).
/// Called from both the reader-loop EOF and `close`; whichever arrives first wins, the other is a
/// no-op.
fn finish_session<R: Runtime>(
    app: &AppHandle<R>,
    sessions: &Mutex<HashMap<String, Arc<PtySession>>>,
    session: &Arc<PtySession>,
) {
    if session.torn_down.swap(true, Ordering::AcqRel) {
        return; // already torn down by the other path.
    }
    sessions.lock().unwrap().remove(&session.id);
    let reason = if session.closing.load(Ordering::Acquire) {
        "killed"
    } else {
        "exited"
    };
    // SIGKILL (idempotent — a no-op on an already-dead child) then reap on a detached thread so the
    // caller (reader thread or `close`) never blocks on `wait`.
    let _ = session.killer.lock().unwrap().kill();
    let reaper = session.clone();
    std::thread::spawn(move || {
        let _ = reaper.child.lock().unwrap().wait();
    });
    crate::stream::emit(
        app,
        StreamEvent::Terminal(TerminalEvent::SessionEnded {
            session_id: session.id.clone(),
            reason: reason.to_string(),
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    // `Manager::manage` installs the `AppState` the emit funnel reads; only the unix real-PTY
    // tests (which trigger emits via subscribe/close/self-exit teardown) need it.
    #[cfg(unix)]
    use tauri::Manager;

    // ---- Pure tests (platform-agnostic; compile + run on Windows too) ----

    #[test]
    fn scrollback_ring_drops_front_past_cap_preserving_tail() {
        let mut ring = ScrollbackRing::new(4);
        ring.push(b"abc");
        assert_eq!(ring.snapshot(), b"abc");
        ring.push(b"def");
        // "abcdef" trimmed to the last 4 bytes — front dropped, tail + order preserved.
        assert_eq!(ring.snapshot(), b"cdef");
        // A single push larger than `cap` retains only its tail.
        ring.push(b"0123456789");
        assert_eq!(ring.snapshot(), b"6789");
    }

    #[test]
    fn scrollback_ring_under_cap_keeps_everything() {
        let mut ring = ScrollbackRing::new(64);
        ring.push(b"hello ");
        ring.push(b"world");
        assert_eq!(ring.snapshot(), b"hello world");
    }

    #[test]
    fn owns_and_list_are_empty_on_fresh_manager() {
        let m = WebPtyManager::default();
        assert!(!m.owns("webpty-0"));
        assert!(m.list().is_empty());
    }

    #[test]
    fn shell_basename_strips_directory() {
        assert_eq!(shell_basename("/bin/zsh"), "zsh");
        assert_eq!(shell_basename("/usr/bin/fish"), "fish");
        assert_eq!(shell_basename("powershell.exe"), "powershell.exe");
    }

    #[test]
    fn encode_base64_is_byte_lossless_for_non_utf8() {
        // 0xFF 0xFE is not valid UTF-8 — base64 must still round-trip it (not lose bytes).
        let raw = [0xFFu8, 0xFE, b'x'];
        let encoded = encode_base64(&raw);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("round-trips");
        assert_eq!(decoded, raw);
    }

    #[test]
    fn check_session_cap_rejects_at_limit() {
        assert!(check_session_cap(0).is_ok());
        assert!(check_session_cap(MAX_CONCURRENT_SESSIONS - 1).is_ok());
        let err = check_session_cap(MAX_CONCURRENT_SESSIONS).expect_err("at cap rejects");
        assert!(err.message.contains("上限"), "actionable: {}", err.message);
        assert!(check_session_cap(MAX_CONCURRENT_SESSIONS + 1).is_err());
    }

    #[test]
    fn normal_pty_eof_classifies_broken_pipe_and_eio() {
        use std::io::{Error, ErrorKind};
        assert!(is_normal_pty_eof(&Error::from(ErrorKind::BrokenPipe)));
        assert!(is_normal_pty_eof(&Error::from(ErrorKind::ConnectionReset)));
        assert!(is_normal_pty_eof(&Error::from(ErrorKind::UnexpectedEof)));
        #[cfg(unix)]
        assert!(
            is_normal_pty_eof(&Error::from_raw_os_error(5)),
            "EIO is a normal pty close"
        );
        // An unexpected fault is NOT a normal EOF → it gets logged before teardown.
        assert!(!is_normal_pty_eof(&Error::from(
            ErrorKind::PermissionDenied
        )));
    }

    // ---- Real-PTY round-trips (unix only; explicit /bin/sh for determinism) ----
    //
    // These observe output via `scrollback_snapshot` (no Tauri emit sink needed). They manage an
    // `AppState` on the mock app because the teardown / subscribe paths emit through
    // `crate::stream::emit` (which reads `app.state::<AppState>().stream`); the managed state's bus
    // has no subscribers, so the emit is a swallowed no-op.

    #[cfg(unix)]
    fn poll_until<F: Fn() -> bool>(predicate: F) -> bool {
        for _ in 0..250 {
            if predicate() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[cfg(unix)]
    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[cfg(unix)]
    #[test]
    fn pty_spawn_then_write_streams_output_into_ring() {
        let app = tauri::test::mock_app();
        app.manage(crate::state::AppState::default());
        let m = WebPtyManager::default();
        let session = m
            .spawn_program(app.handle(), &CreateSessionOpts::default(), "/bin/sh")
            .expect("spawn /bin/sh");
        let id = session.session_id.clone();
        assert!(m.owns(&id), "spawn registers the session");
        assert_eq!(session.backend, TerminalBackendKind::WebPty);
        assert_eq!(session.title, "sh");
        // F3: a successful create balances its reservation (the session is now counted in `list`).
        assert_eq!(m.reserved_count(), 0, "success releases the reservation");

        m.write(&id, b"echo hi\n").expect("write");
        let saw = poll_until(|| {
            m.scrollback_snapshot(&id)
                .is_some_and(|b| contains_subslice(&b, b"hi"))
        });
        assert!(
            saw,
            "the shell's output 'hi' should reach the scrollback ring"
        );
        m.shutdown();
    }

    // F3: a create that fails AFTER reserving (here: `spawn_command` rejects a bogus program) must
    // release its reservation and register nothing — the RAII `SpawnCleanup` guard guarantees it.
    // NOTE: the post-spawn (try_clone_reader / take_writer) child-kill branch can't be induced
    // without a fault injector; the true CONCURRENT cap race is bounded by the reservation (the
    // check + `fetch_add` are atomic under the `sessions` lock), not unit-tested.
    #[cfg(unix)]
    #[test]
    fn failed_spawn_releases_reservation_and_registers_nothing() {
        let app = tauri::test::mock_app();
        let m = WebPtyManager::default();
        let err = m
            .spawn_program(
                app.handle(),
                &CreateSessionOpts::default(),
                "/nonexistent/prmonitor-webpty-shell",
            )
            .expect_err("a bogus program must fail to spawn");
        assert!(!err.message.is_empty());
        assert!(m.list().is_empty(), "a failed create registers no session");
        assert_eq!(
            m.reserved_count(),
            0,
            "a failed create releases its reservation (no leak)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pty_resize_updates_stored_grid() {
        let app = tauri::test::mock_app();
        app.manage(crate::state::AppState::default());
        let m = WebPtyManager::default();
        let session = m
            .spawn_program(app.handle(), &CreateSessionOpts::default(), "/bin/sh")
            .expect("spawn /bin/sh");
        let id = session.session_id;
        assert_eq!(m.session_grid(&id), Some((DEFAULT_COLS, DEFAULT_ROWS)));
        m.resize(&id, 120, 40).expect("resize");
        assert_eq!(m.session_grid(&id), Some((120, 40)));
        // `list` reads the same updated grid.
        let row = m
            .list()
            .into_iter()
            .find(|s| s.session_id == id)
            .expect("session listed");
        assert_eq!((row.cols, row.rows), (120, 40));
        m.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn pty_close_deregisters_synchronously() {
        let app = tauri::test::mock_app();
        app.manage(crate::state::AppState::default());
        let m = WebPtyManager::default();
        let session = m
            .spawn_program(app.handle(), &CreateSessionOpts::default(), "/bin/sh")
            .expect("spawn /bin/sh");
        let id = session.session_id;
        assert!(m.owns(&id));
        m.close(app.handle(), &id).expect("close");
        // Teardown ran inside `close`: the session is gone the moment it returns.
        assert!(!m.owns(&id), "close deregisters synchronously");
        assert!(m.list().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn pty_child_self_exit_deregisters_via_reader_teardown() {
        let app = tauri::test::mock_app();
        app.manage(crate::state::AppState::default());
        let m = WebPtyManager::default();
        let session = m
            .spawn_program(app.handle(), &CreateSessionOpts::default(), "/bin/sh")
            .expect("spawn /bin/sh");
        let id = session.session_id;
        m.write(&id, b"exit\n").expect("write exit");
        // The shell exits → master EOFs → the reader loop's `finish_session` deregisters it.
        let gone = poll_until(|| !m.owns(&id));
        assert!(gone, "a self-exiting shell deregisters via reader teardown");
    }

    // TEST-1: the subscribe replay path — spawn → write → ring fills → subscribe flips the flag and
    // the ring still holds the backlog (the replay doesn't drain it). Core concurrency path.
    #[cfg(unix)]
    #[test]
    fn pty_subscribe_replays_ring_and_sets_flag() {
        let app = tauri::test::mock_app();
        app.manage(crate::state::AppState::default());
        let m = WebPtyManager::default();
        let session = m
            .spawn_program(app.handle(), &CreateSessionOpts::default(), "/bin/sh")
            .expect("spawn /bin/sh");
        let id = session.session_id;
        m.write(&id, b"echo hi\n").expect("write");
        assert!(
            poll_until(|| m.scrollback_snapshot(&id).is_some_and(|b| !b.is_empty())),
            "ring should fill before subscribe"
        );
        assert_eq!(
            m.is_subscribed(&id),
            Some(false),
            "not subscribed before attach"
        );
        m.subscribe(app.handle(), &id).expect("subscribe");
        assert_eq!(m.is_subscribed(&id), Some(true), "subscribe flips the flag");
        assert!(
            m.scrollback_snapshot(&id).is_some_and(|b| !b.is_empty()),
            "ring is retained after the replay (subscribe doesn't drain it)"
        );
        m.shutdown();
    }

    // TEST-6: unsubscribe clears the flag; an unknown id is an actionable error.
    #[cfg(unix)]
    #[test]
    fn pty_unsubscribe_clears_flag_and_unknown_id_errors() {
        let app = tauri::test::mock_app();
        app.manage(crate::state::AppState::default());
        let m = WebPtyManager::default();
        let session = m
            .spawn_program(app.handle(), &CreateSessionOpts::default(), "/bin/sh")
            .expect("spawn /bin/sh");
        let id = session.session_id;
        m.subscribe(app.handle(), &id).expect("subscribe");
        assert_eq!(m.is_subscribed(&id), Some(true));
        m.unsubscribe(&id).expect("unsubscribe");
        assert_eq!(
            m.is_subscribed(&id),
            Some(false),
            "unsubscribe clears the flag"
        );
        let err = m
            .unsubscribe("webpty-does-not-exist")
            .expect_err("unknown id errors");
        assert!(
            err.message.contains("不存在"),
            "actionable: {}",
            err.message
        );
        m.shutdown();
    }

    // TEST-5: serial `close` idempotency — the first close tears down + deregisters; the second
    // finds nothing and returns a not-found AppError (no panic, no double teardown). The TRUE
    // concurrent close-vs-EOF race is guarded by `torn_down` (a swap), which isn't stably
    // unit-testable, so this covers the serial path the same guard also protects.
    #[cfg(unix)]
    #[test]
    fn pty_close_is_idempotent_second_close_not_found() {
        let app = tauri::test::mock_app();
        app.manage(crate::state::AppState::default());
        let m = WebPtyManager::default();
        let session = m
            .spawn_program(app.handle(), &CreateSessionOpts::default(), "/bin/sh")
            .expect("spawn /bin/sh");
        let id = session.session_id;
        m.close(app.handle(), &id).expect("first close");
        let err = m
            .close(app.handle(), &id)
            .expect_err("second close finds nothing");
        assert!(
            err.message.contains("不存在"),
            "actionable: {}",
            err.message
        );
    }

    #[cfg(unix)]
    #[test]
    fn pty_shutdown_clears_all_sessions() {
        let app = tauri::test::mock_app();
        app.manage(crate::state::AppState::default());
        let m = WebPtyManager::default();
        let a = m
            .spawn_program(app.handle(), &CreateSessionOpts::default(), "/bin/sh")
            .expect("spawn a");
        let b = m
            .spawn_program(app.handle(), &CreateSessionOpts::default(), "/bin/sh")
            .expect("spawn b");
        assert!(m.owns(&a.session_id) && m.owns(&b.session_id));
        m.shutdown();
        assert!(m.list().is_empty(), "shutdown drains the registry");
        assert!(!m.owns(&a.session_id));
        assert!(!m.owns(&b.session_id));
    }
}
