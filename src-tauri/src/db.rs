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

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction};
use tauri::Manager;

use crate::error::{AppError, AppResult};

/// Current schema version. Bump + add an `apply_vN` step for every schema change; the
/// migration runner replays only the steps newer than the DB's `user_version`.
const SCHEMA_VERSION: i64 = 3;

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

    /// Opens an EXISTING `prmonitor.db` READ-ONLY at `path`, WITHOUT running migrations — for an
    /// out-of-app reader (the AB#1044 CLI client) that only needs the config blob while the
    /// running app owns the file. Read-only + a bounded `busy_timeout` rides out the app's brief
    /// write locks and GUARANTEES a second process never migrates / mutates the live store (a
    /// stale CLI binary must not stamp the schema or write through `from_conn`). Errors if the
    /// file is absent (= the app has never run / written config), letting the caller fall back.
    pub fn open_readonly_at(path: &Path) -> AppResult<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| AppError::new(format!("打开 SQLite（只读）失败: {e}")))?;
        conn.busy_timeout(Duration::from_millis(2000))
            .map_err(map_err)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
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
    // Fail closed on a DB written by a NEWER app (pr-review F3): an old binary must NOT
    // stamp the version back down and then write against a schema it doesn't understand.
    // Refuse to open rather than silently corrupt a forward-migrated store.
    if version > SCHEMA_VERSION {
        return Err(AppError::new(format!(
            "数据库 schema 版本 {version} 高于本程序支持的 {SCHEMA_VERSION}——拒绝降级打开（请升级应用）"
        )));
    }
    if version < 1 {
        apply_v1(conn)?;
    }
    if version < 2 {
        apply_v2(conn)?;
    }
    if version < 3 {
        apply_v3(conn)?;
    }
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(map_err)?;
    Ok(())
}

fn apply_v1(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(SCHEMA_V1).map_err(map_err)?;
    Ok(())
}

/// v2 (AB#1042): the trigger funnel persists the resolved review-comment URL per session.
/// Adds the column with `ALTER TABLE` rather than editing [`SCHEMA_V1`] — a fresh DB
/// (version 0) runs v1 (which has no `comment_url`) then this v2 step, while an existing
/// v1 install runs ONLY this step. Editing `SCHEMA_V1` to carry the column would make this
/// `ADD COLUMN` collide with "duplicate column" on every already-migrated v1 store.
fn apply_v2(conn: &Connection) -> AppResult<()> {
    conn.execute_batch("ALTER TABLE review_session ADD COLUMN comment_url TEXT;")
        .map_err(map_err)?;
    Ok(())
}

/// v3 (PR #176 fix): persist the engine that created each review session. Existing rows
/// predate Claude, so the only valid historical default is `codex`.
fn apply_v3(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(
        "ALTER TABLE review_session ADD COLUMN engine_kind TEXT NOT NULL DEFAULT 'codex';",
    )
    .map_err(map_err)?;
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

-- No FK to review_session: history capture is BEST-EFFORT (the pump logs+swallows
-- persistence errors so a DB hiccup never breaks the live stream). A rigid FK would,
-- if the session-row upsert lost a race/failed, make every `append_item` FK-violate and
-- silently drop that session's whole history. An orphan history row (recoverable, still
-- readable by thread_id) is strictly better than losing the content (review F2).
CREATE TABLE IF NOT EXISTS review_history_item (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT    NOT NULL,
    item_id   TEXT    NOT NULL,
    kind      TEXT    NOT NULL,
    text      TEXT    NOT NULL,
    UNIQUE (thread_id, item_id)
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

    /// v1 → current migration lock: a DB stamped at v1 (no post-v1 columns) must gain every
    /// later review_session column and stamp to the current schema version.
    #[test]
    fn migrate_v1_to_current_adds_review_session_columns() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        // Replay v1 exactly as an existing v1 install has it (no `comment_url`), then stamp
        // the version so the runner sees a v1 DB and applies only the v2 delta.
        apply_v1(&conn).expect("seed v1");
        conn.pragma_update(None, "user_version", 1)
            .expect("stamp v1");
        // Precondition: a v1 `review_session` has NO `comment_url` column.
        assert!(
            !review_session_has_comment_url(&conn),
            "v1 must not already have comment_url"
        );

        run_migrations(&conn).expect("v1 → current migrates");

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("read version");
        assert_eq!(version, SCHEMA_VERSION, "stamped to current schema");
        assert!(
            review_session_has_comment_url(&conn),
            "v2 added the comment_url column"
        );
        assert!(
            review_session_has_engine_kind(&conn),
            "v3 added the engine_kind column"
        );
    }

    /// Fresh migration lock: opening a brand-new DB replays ALL migrations. Assert the runner
    /// lands on `SCHEMA_VERSION` AND that every post-v1 column is present.
    #[test]
    fn fresh_open_migrates_to_current_schema() {
        let db = Database::open_in_memory().expect("open");
        db.with_conn(|conn| {
            let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
            assert_eq!(
                version, SCHEMA_VERSION,
                "fresh open stamps the current schema"
            );
            assert_eq!(SCHEMA_VERSION, 3, "current schema is v3");
            assert!(
                review_session_has_comment_url(conn),
                "fresh v0 → v2 has the comment_url column"
            );
            assert!(
                review_session_has_engine_kind(conn),
                "fresh v0 → v3 has the engine_kind column"
            );
            Ok(())
        })
        .expect("query");
    }

    /// v2 → v3 migration lock: persisted review sessions must carry the engine that created
    /// them, so a follow-up after config changes routes back to the original engine.
    #[test]
    fn migrate_v2_to_v3_adds_engine_kind_column() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        apply_v1(&conn).expect("seed v1");
        apply_v2(&conn).expect("seed v2");
        conn.pragma_update(None, "user_version", 2)
            .expect("stamp v2");
        assert!(
            !review_session_has_engine_kind(&conn),
            "v2 must not already have engine_kind"
        );

        run_migrations(&conn).expect("v2 -> v3 migrates");

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("read version");
        assert_eq!(version, 3, "stamped to v3");
        assert!(
            review_session_has_engine_kind(&conn),
            "v3 added the engine_kind column"
        );
    }

    /// Whether `review_session` has a `comment_url` column (via `PRAGMA table_info`).
    fn review_session_has_comment_url(conn: &Connection) -> bool {
        review_session_has_column(conn, "comment_url")
    }

    fn review_session_has_engine_kind(conn: &Connection) -> bool {
        review_session_has_column(conn, "engine_kind")
    }

    fn review_session_has_column(conn: &Connection, column: &str) -> bool {
        let mut stmt = conn
            .prepare("PRAGMA table_info(review_session)")
            .expect("table_info");
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1)) // col 1 = name
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        names.iter().any(|n| n == column)
    }

    /// Forward-compat guard (pr-review F3): a DB stamped with a HIGHER schema version than
    /// this build supports is refused, not silently downgraded back to the current version.
    #[test]
    fn run_migrations_rejects_future_schema_version() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("stamp a future version");
        let err = run_migrations(&conn).expect_err("future version must be rejected");
        assert!(err.message.contains("高于"), "{}", err.message);
        // The version was left intact, NOT stamped back down to the current schema.
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("read version");
        assert_eq!(version, SCHEMA_VERSION + 1, "future version untouched");
    }

    /// A unique on-disk path for the `open_readonly_at` tests (process-scoped, per-tag).
    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("prmonitor_ro_{}_{tag}.db", std::process::id()))
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    /// AB#1044: a missing file is an error (the app has never written config), so the CLI caller
    /// can fall back to defaults rather than silently opening/creating a blank store.
    #[test]
    fn open_readonly_at_errors_on_missing_file() {
        let path = temp_db_path("missing");
        cleanup(&path);
        assert!(Database::open_readonly_at(&path).is_err());
    }

    /// AB#1044: the read-only connection must REJECT writes — a second process reading the live
    /// store can never mutate the app's data.
    #[test]
    fn open_readonly_at_rejects_writes() {
        let path = temp_db_path("ro");
        cleanup(&path);
        {
            let conn = Connection::open(&path).expect("create");
            run_migrations(&conn).expect("migrate");
        }
        let db = Database::open_readonly_at(&path).expect("open ro");
        let res = db.with_conn(|c| {
            c.execute(
                "INSERT OR REPLACE INTO config_blob (id, json) VALUES (1, '{}')",
                [],
            )
        });
        assert!(res.is_err(), "a read-only connection must reject writes");
        cleanup(&path);
    }

    /// AB#1044 regression guard: `open_readonly_at` must NOT run migrations — a stale CLI binary
    /// opening a v1 store must leave `user_version` at 1, never stamp/downgrade the app's schema.
    /// (A normal `open` would migrate it to `SCHEMA_VERSION`.)
    #[test]
    fn open_readonly_at_does_not_migrate() {
        let path = temp_db_path("nomig");
        cleanup(&path);
        {
            let conn = Connection::open(&path).expect("create");
            apply_v1(&conn).expect("seed v1");
            conn.pragma_update(None, "user_version", 1)
                .expect("stamp v1");
        }
        let db = Database::open_readonly_at(&path).expect("open ro");
        let version: i64 = db
            .with_conn(|c| c.pragma_query_value(None, "user_version", |r| r.get(0)))
            .expect("read version");
        assert_eq!(version, 1, "open_readonly_at must not migrate the store");
        cleanup(&path);
    }
}
