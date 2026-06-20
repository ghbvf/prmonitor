//! Unified SQLite persistence backend (#70).
//!
//! Horizontal infra (like [`crate::error`] / [`crate::state`]): owns the single
//! SQLite connection, the schema migration runner (`PRAGMA user_version`), and the
//! generic [`Database::with_conn`] / [`Database::with_tx`] accessors. It is the
//! schema's composition root — **ALL DDL lives here**, in ordered migration steps;
//! slices never `CREATE TABLE`. Each slice owns its own tables' *queries* in its own
//! `*_store` module, reaching the connection through the [`Database`] handle (a
//! horizontal dependency, exactly like [`crate::error::AppResult`] — NOT a cross-slice
//! import). Adding a slice table means editing this file's migration steps: the single
//! intended choke point for schema evolution, mirroring how [`crate::lib`]'s
//! `generate_handler!` is the command-registration choke point.
//!
//! **Why a `tauri::State`, not an [`crate::state::AppState`] field:** `app_data_dir()`
//! only resolves inside `setup`, while `AppState` is `.manage()`d at builder time
//! (keeping `AppState: Default`). So the composition root opens the DB in `setup` and
//! `app.manage(Database::open(..)?)`. Slices/commands already carry `app: &AppHandle`,
//! so they reach it via `app.state::<Database>()`, mirroring `app.state::<AppState>()`.
//! A command running before that manage would panic on `app.state::<Database>()`
//! (fail-fast) — but `setup` completes before any command is served, so it never does.

use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, Transaction};
use tauri::Manager;

use crate::error::{AppError, AppResult};

/// Current schema version. Bump + add an `apply_vN` step for every schema change; the
/// migration runner replays only the steps newer than the DB's `user_version`.
const SCHEMA_VERSION: i64 = 1;

/// `meta` guard key marking the one-time legacy JSON → SQLite import done (#70). Kept
/// SEPARATE from `user_version` so the import runs exactly once even across future
/// schema bumps (a schema migration must not re-trigger the data import).
const META_LEGACY_IMPORTED: &str = "legacyImported";

/// The single SQLite connection behind a `std::sync::Mutex` — the same "sync store
/// behind a Mutex" shape the JSON stores used, so all the existing write-lock reasoning
/// (registry/ledger) carries over unchanged. `Connection: Send` ⇒ `Mutex<Connection>:
/// Send + Sync`, so this is a valid `tauri::State`.
pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    /// Opens (creating if absent) the app's `prmonitor.db` under `app_data_dir`, sets
    /// pragmas, and runs schema migrations. The one-time legacy JSON import is driven
    /// SEPARATELY by the composition root (it must read the old `tauri-plugin-store`
    /// files via `app`), gated by [`Database::legacy_imported`].
    pub fn open<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> AppResult<Self> {
        let dir = app
            .path()
            .app_data_dir()
            .map_err(|e| AppError::new(format!("解析应用数据目录失败: {e}")))?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| AppError::new(format!("创建应用数据目录失败: {e}")))?;
        let path = dir.join("prmonitor.db");
        let conn =
            Connection::open(&path).map_err(|e| AppError::new(format!("打开 SQLite 失败: {e}")))?;
        Self::from_conn(conn)
    }

    /// In-memory database — schema migrated, no legacy import. Used by store round-trip
    /// tests across slices (each slice's `*_store` tests open one of these directly).
    pub fn open_in_memory() -> AppResult<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| AppError::new(format!("打开内存 SQLite 失败: {e}")))?;
        Self::from_conn(conn)
    }

    fn from_conn(conn: Connection) -> AppResult<Self> {
        // `execute_batch` (not `pragma_update`) for journal_mode: it returns the
        // resulting mode as a row, which `pragma_update`'s `execute` would reject. WAL
        // silently stays "memory" for in-memory DBs — harmless.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(map_err)?;
        run_migrations(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Runs `f` with a shared `&Connection` (a read or a single-statement write).
    /// Serializes all DB access process-wide via the connection mutex. The closure is
    /// synchronous; callers in async paths (the pump) never hold the guard across an
    /// `.await` (the closure body has none).
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> AppResult<T> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        f(&conn).map_err(map_err)
    }

    /// Runs `f` inside a transaction (atomic multi-statement read-modify-write),
    /// committing on `Ok` and rolling back on `Err`. The closure returns [`AppResult`]
    /// so it can interleave slice query helpers (which already map to [`AppError`]).
    pub fn with_tx<T>(&self, f: impl FnOnce(&Transaction) -> AppResult<T>) -> AppResult<T> {
        let mut conn = self.conn.lock().expect("db mutex poisoned");
        let tx = conn.transaction().map_err(map_err)?;
        let out = f(&tx)?;
        tx.commit().map_err(map_err)?;
        Ok(out)
    }

    /// Whether the one-time legacy JSON import has already run (#70 guard). The
    /// composition root checks this before reading the old JSON stores.
    pub fn legacy_imported(&self) -> AppResult<bool> {
        self.with_conn(|conn| meta_get(conn, META_LEGACY_IMPORTED).map(|v| v.is_some()))
    }
}

/// Maps a rusqlite error into the app error funnel ([`AppError`]).
pub(crate) fn map_err(e: rusqlite::Error) -> AppError {
    AppError::new(format!("数据库错误: {e}"))
}

/// Reads a `meta` value by key (rusqlite-level so it composes inside `with_conn`/`with_tx`).
fn meta_get(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    })
    .optional()
}

/// Marks the one-time legacy import done (#70). MUST be called inside the SAME
/// transaction as the imported-row inserts so a crash mid-import rolls back the guard
/// too and re-runs cleanly.
pub fn mark_legacy_imported(tx: &Transaction) -> AppResult<()> {
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, '1')",
        [META_LEGACY_IMPORTED],
    )
    .map_err(map_err)?;
    Ok(())
}

/// Replays schema steps newer than the DB's `user_version`, then stamps the current
/// version. Each future schema change = a new `apply_vN` + a `< N` gate here.
fn run_migrations(conn: &Connection) -> AppResult<()> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(map_err)?;
    if version < 1 {
        apply_v1(conn)?;
    }
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(map_err)?;
    Ok(())
}

fn apply_v1(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(SCHEMA_V1).map_err(map_err)?;
    Ok(())
}

/// v1 schema — the unified store (#70). `review_session` precedes `review_history_item`
/// (the FK target must exist first under `foreign_keys=ON`). Per-project partitioning
/// is a real `project_id TEXT` column (replacing the JSON stores' `prefix:{pid}` keys).
const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);

CREATE TABLE IF NOT EXISTS config_blob (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS tracked_pr (
    project_id       TEXT    NOT NULL,
    number           INTEGER NOT NULL,
    title            TEXT    NOT NULL,
    labels_json      TEXT    NOT NULL,
    url              TEXT    NOT NULL,
    kind             TEXT    NOT NULL,
    skip_reason      TEXT,
    first_seen_epoch INTEGER NOT NULL,
    last_seen_epoch  INTEGER NOT NULL,
    archived         INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (project_id, number)
);
CREATE INDEX IF NOT EXISTS idx_tracked_pr_project ON tracked_pr(project_id, number DESC);

CREATE TABLE IF NOT EXISTS dispatch_key (
    project_id TEXT NOT NULL,
    key        TEXT NOT NULL,
    PRIMARY KEY (project_id, key)
);

CREATE TABLE IF NOT EXISTS dispatch_event (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id          TEXT    NOT NULL,
    pr                  INTEGER NOT NULL,
    kind                TEXT    NOT NULL,
    head_sha            TEXT    NOT NULL,
    key                 TEXT    NOT NULL,
    dispatched_at_epoch INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_dispatch_event_lookup ON dispatch_event(project_id, pr, kind);

CREATE TABLE IF NOT EXISTS review_session (
    thread_id  TEXT    PRIMARY KEY,
    project_id TEXT    NOT NULL,
    pr_number  INTEGER NOT NULL,
    turn_id    TEXT    NOT NULL DEFAULT '',
    kind       TEXT    NOT NULL,
    status     TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_review_session_pr ON review_session(project_id, pr_number, created_at);

CREATE TABLE IF NOT EXISTS review_history_item (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT    NOT NULL,
    item_id   TEXT    NOT NULL,
    kind      TEXT    NOT NULL,
    text      TEXT    NOT NULL,
    UNIQUE (thread_id, item_id),
    FOREIGN KEY (thread_id) REFERENCES review_session(thread_id)
);
CREATE INDEX IF NOT EXISTS idx_history_thread ON review_history_item(thread_id, id);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// v1 migration lock (Medium per ai-robust.md): a missing/renamed table here means
    /// a slice store's first query fails at runtime, not at compile time — so pin the
    /// table set + the stamped `user_version`.
    #[test]
    fn migrations_create_all_tables_and_stamp_version() {
        let db = Database::open_in_memory().expect("open");
        db.with_conn(|conn| {
            let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
            assert_eq!(version, SCHEMA_VERSION, "user_version stamped");

            let mut stmt =
                conn.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
            let names: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?;
            for expected in [
                "config_blob",
                "dispatch_event",
                "dispatch_key",
                "meta",
                "review_history_item",
                "review_session",
                "tracked_pr",
            ] {
                assert!(names.contains(&expected.to_string()), "missing {expected}");
            }
            Ok(())
        })
        .expect("query");
    }

    /// The legacy-import guard flips exactly once and is observable through the public
    /// `legacy_imported()` the composition root gates on.
    #[test]
    fn legacy_import_guard_flips_once() {
        let db = Database::open_in_memory().expect("open");
        assert!(!db.legacy_imported().expect("read guard"), "starts unset");

        db.with_tx(mark_legacy_imported).expect("mark");
        assert!(db.legacy_imported().expect("read guard"), "set after mark");

        // Idempotent: marking again (INSERT OR REPLACE) keeps it true with no dup row.
        db.with_tx(mark_legacy_imported).expect("re-mark");
        assert!(db.legacy_imported().expect("read guard"));
    }
}
