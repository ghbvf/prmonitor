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
const SCHEMA_VERSION: i64 = 11;

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
    if version < 4 {
        apply_v4(conn)?;
    }
    if version < 5 {
        apply_v5(conn)?;
    }
    if version < 6 {
        apply_v6(conn)?;
    }
    if version < 7 {
        apply_v7(conn)?;
    }
    if version < 8 {
        apply_v8(conn)?;
    }
    if version < 9 {
        apply_v9(conn)?;
    }
    if version < 10 {
        apply_v10(conn)?;
    }
    if version < 11 {
        apply_v11(conn)?;
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

/// v4 (AB#1065): the event inbox. Persists every inbound webhook delivery (GitHub ingest +
/// Azure audit), deduped by delivery identity, for listing / raw inspection / replay. A
/// FRESH `CREATE TABLE` batch (like [`SCHEMA_V1`]), NOT an `ALTER` — it adds a brand-new
/// table, so a fresh DB (version 0) runs v1..v4 and an existing v3 install runs ONLY this
/// step (the `CREATE TABLE IF NOT EXISTS` is also idempotent under replay).
fn apply_v4(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(SCHEMA_V4).map_err(map_err)?;
    Ok(())
}

/// v5 (AB#1066): the action outbox. Persists every queued side effect (today the
/// review-completion desktop notification) so a pending action survives an app restart and a
/// failed one retries to a terminal dead-letter. A FRESH `CREATE TABLE` batch (like
/// [`SCHEMA_V1`] / [`SCHEMA_V4`]), NOT an `ALTER` — a fresh DB (version 0) runs v1..v5 and an
/// existing v4 install runs ONLY this step (the `CREATE TABLE IF NOT EXISTS` is also idempotent
/// under replay).
fn apply_v5(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(SCHEMA_V5).map_err(map_err)?;
    Ok(())
}

/// v6 (AB#1204): the outbox review-execution claim. Closes the cross-restart duplicate-review
/// window: the AB#1069 outbox executor is at-least-once, so a crash AFTER an outbox `review`/`check`
/// action started a review but BEFORE the row was marked `done` re-runs that action next boot —
/// and the purely-in-memory [`crate::review::session::SessionRegistry::try_reserve_pair`] reserves
/// freely after a restart, so the replay launches a DUPLICATE review (a second `pm:` comment). This
/// table is a write-ahead claim keyed by the OUTBOX ROW id (the unit of at-least-once replay), so
/// the replay can tell "this action already started a review (resolve its outcome, don't duplicate)"
/// from "a new review need" — keying on `(project,pr,kind)` instead would wrongly suppress a
/// legitimate re-review of a NEW commit. A FRESH `CREATE TABLE` batch (like [`SCHEMA_V4`] /
/// [`SCHEMA_V5`]), NOT an `ALTER` — a fresh DB runs v1..v6 and an existing v5 install runs ONLY this
/// step (the `CREATE TABLE IF NOT EXISTS` is also idempotent under replay).
fn apply_v6(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(SCHEMA_V6).map_err(map_err)?;
    Ok(())
}

/// v7 (AB#1204): add a `FOREIGN KEY(outbox_id) REFERENCES action_outbox(id) ON DELETE CASCADE` to
/// `outbox_review_claim` so a claim is AUTOMATICALLY removed when its owning `action_outbox` row is
/// deleted (retention prune in `outbox::store`, `DELETE FROM action_outbox`). SQLite cannot
/// `ALTER TABLE … ADD CONSTRAINT`, so the only way to add an FK to an existing table is the
/// documented 12-step table-rebuild: create a `_new` table WITH the FK, copy the rows, drop the old
/// table, rename `_new` into place (see [`SCHEMA_V7`]). A fresh DB (version 0) runs v1..v6 (creating
/// the FK-less claim) then this v7 step rebuilds it WITH the FK — the end state is identical to an
/// existing-v6 install upgrading. The rebuild touches `outbox_review_claim` (the CHILD/referencing
/// side); nothing references IT, so the DROP/RENAME is safe even under `foreign_keys=ON`.
fn apply_v7(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(SCHEMA_V7).map_err(map_err)?;
    Ok(())
}

/// v8 (#1445): metadata-only remote terminal audit trail. This intentionally stores no terminal
/// input and no screen contents; only the listener/action/result/request origin metadata needed to
/// investigate remote terminal access.
fn apply_v8(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(SCHEMA_V8).map_err(map_err)?;
    Ok(())
}

/// v9 (#1379): default rule production needs a persisted candidate on inbox rows, and review/check
/// actions need a dedupe key while pending so repeated auto-dispatch cannot enqueue duplicates.
fn apply_v9(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(SCHEMA_V9).map_err(map_err)?;
    Ok(())
}

/// v10 (#1371): rule-engine match audit. The FK chain makes inbox→rule match→outbox trace
/// non-optional at the DB layer.
fn apply_v10(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(SCHEMA_V10).map_err(map_err)?;
    Ok(())
}

/// v11 (#1553): remote-access gate denial audit. This records only request metadata needed to
/// investigate blocked remote entrypoint attempts; it intentionally stores no request body, bearer
/// token, or terminal input.
fn apply_v11(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(SCHEMA_V11).map_err(map_err)?;
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

/// v4 schema — the event inbox (AB#1065). One row per inbound webhook delivery.
///
/// `dedupe_key` carries the inbox's **Hard** ingress-idempotency carrier: the `UNIQUE`
/// constraint makes a double-insert of the SAME delivery identity (a webhook retry / tunnel
/// re-delivery) UNEXPRESSIBLE at the storage layer — `insert_dedup`'s
/// `INSERT … ON CONFLICT(dedupe_key) DO NOTHING` relies on it to process each delivery
/// exactly once (the upstream of the inbox funnel; the downstream review/check dedup is the outbox
/// pending `dedupe_key` plus the review executor's durable claim — see `inbox::service`).
///
/// `event_json` stores the serialized normalized [`crate::model::Event`] (the wire envelope
/// the panel renders + replay reads back). `raw_payload` is the verbatim delivery body (the
/// `inbox_get_raw` audit source). `webhook_event_json` stores the parsed
/// `pr::webhook::WebhookEvent` JSON for GitHub entries so a GitHub replay re-feeds the SAME
/// classified event through the vetted `ingest_webhook` path WITHOUT re-running the
/// route-dependent `parse_delivery` (which needs the live route snapshot, absent at replay
/// time); `NULL` for Azure audit entries (replay re-invokes the refresher instead).
const SCHEMA_V4: &str = r#"
CREATE TABLE IF NOT EXISTS inbox_event (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    dedupe_key         TEXT    NOT NULL UNIQUE,
    source             TEXT    NOT NULL,
    event_type         TEXT    NOT NULL,
    project_id         TEXT    NOT NULL,
    repo               TEXT    NOT NULL,
    number             INTEGER,
    event_json         TEXT    NOT NULL,
    raw_payload        TEXT    NOT NULL,
    -- The parsed `pr::webhook::WebhookEvent` JSON, ONLY for replay reconstruction: a GitHub
    -- replay re-feeds this through `ingest_webhook` instead of re-running `parse_delivery`
    -- (which is private + needs the live route snapshot, so it is not standalone-callable at
    -- replay time). NULL for Azure audit rows (replay re-invokes the refresher).
    webhook_event_json TEXT,
    status             TEXT    NOT NULL,
    received_at_epoch  INTEGER NOT NULL,
    processed_at_epoch INTEGER,
    error              TEXT
);
CREATE INDEX IF NOT EXISTS idx_inbox_event_project ON inbox_event(project_id, id DESC);
"#;

/// v5 schema — the action outbox (AB#1066). One row per queued side effect.
///
/// `kind` is the pinned [`crate::model::ActionKind`] wire string (the executor router branches
/// on it). `payload` is the serialized action body (today a `model::Notification` JSON; the
/// `outbox_get_raw` audit source). `summary` is a short human label the panel renders without
/// deserializing the payload. `status` is the pinned [`crate::model::ActionStatus`] wire string
/// — the worker selects `pending` rows whose `next_attempt_at <= now`, executes them, and on
/// failure either bumps `attempt_count` + reschedules `next_attempt_at` (still `pending`) or, at
/// the attempt cap, flips to the terminal `dead` (the dead-letter) with `last_error`. No
/// `UNIQUE`/dedupe constraint (unlike `inbox_event`): the outbox is a producer queue, at-least-
/// once by design — a crash mid-execute leaves the row `pending` to re-run next boot.
const SCHEMA_V5: &str = r#"
CREATE TABLE IF NOT EXISTS action_outbox (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id      TEXT    NOT NULL,
    kind            TEXT    NOT NULL,
    summary         TEXT    NOT NULL,
    payload         TEXT    NOT NULL,
    status          TEXT    NOT NULL,
    attempt_count   INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    last_error      TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
-- The worker's claim query: pending rows whose next_attempt_at is due, oldest first.
CREATE INDEX IF NOT EXISTS idx_action_outbox_due ON action_outbox(status, next_attempt_at);
-- The panel's per-project listing, newest first.
CREATE INDEX IF NOT EXISTS idx_action_outbox_project ON action_outbox(project_id, id DESC);
"#;

/// v6 schema (AB#1204): the outbox review-execution claim — see [`apply_v6`].
///
/// **Hard carrier (`outbox_id PRIMARY KEY`):** the claim is 1:1 with its `action_outbox` row, so a
/// second `INSERT` for the same row is unexpressible — `review::claim_store::begin_claim` relies on
/// `INSERT … ON CONFLICT(outbox_id) DO NOTHING`, the same idempotency-by-key shape `inbox_event`'s
/// `UNIQUE(dedupe_key)` uses. `thread_id` is NULL between the write-ahead claim and the
/// `thread/start` that fills it; on a replay an existing claim's `thread_id` resolves the prior
/// review's `review_session` outcome to decide suppress-vs-rerun. NO `head_sha` / `(project,pr,kind)`
/// key on purpose: keying on the outbox row id is what avoids over-blocking a new-commit re-review.
const SCHEMA_V6: &str = r#"
CREATE TABLE IF NOT EXISTS outbox_review_claim (
    outbox_id   INTEGER PRIMARY KEY,
    project_id  TEXT    NOT NULL,
    pr_number   INTEGER NOT NULL,
    kind        TEXT    NOT NULL,
    thread_id   TEXT,
    created_at  INTEGER NOT NULL
);
"#;

/// v7 schema (AB#1204): rebuild `outbox_review_claim` WITH a child FK to `action_outbox` — see
/// [`apply_v7`]. Same columns / types / order / constraints as [`SCHEMA_V6`] (the `INSERT … SELECT *`
/// copy is positional, so the column layout MUST stay byte-identical) PLUS the trailing FK.
///
/// **Hard carrier (`FOREIGN KEY(outbox_id) REFERENCES action_outbox(id) ON DELETE CASCADE`):** the
/// claim is a CHILD of its `action_outbox` row, so "the owning outbox row is deleted (retention
/// prune in `outbox::store`) but the claim survives as an orphan" is now UNEXPRESSIBLE at the DB
/// layer — SQLite cascades the delete automatically (`foreign_keys=ON`, set in `from_conn`). This
/// is what bounds `outbox_review_claim` WITHOUT the outbox slice ever naming the claim table: it
/// pairs with F2 (a `Dead` row RETAINS its claim as a manual-retry suppress breadcrumb), so a
/// dead/leaked claim is not eagerly released but is instead reaped when its owning row is finally
/// pruned — review-blind cleanup driven purely by the FK. The PK shape (`outbox_id PRIMARY KEY`)
/// and all other columns are unchanged from v6; only the FK is added.
///
/// `foreign_keys` is toggled OFF for the rebuild (SQLite's documented 12-step procedure): the
/// positional `INSERT … SELECT *` must not be FK-checked row-by-row mid-rebuild, and `DROP TABLE`
/// on the old table must not trip a (transient) reference check. `run_migrations` runs OUTSIDE a
/// transaction, so this `PRAGMA` takes effect (a no-op inside a tx); `from_conn` restores
/// `foreign_keys=ON` is unnecessary because we re-enable it here at the end of the batch.
const SCHEMA_V7: &str = r#"
PRAGMA foreign_keys=OFF;
CREATE TABLE outbox_review_claim_new (
    outbox_id   INTEGER PRIMARY KEY,
    project_id  TEXT    NOT NULL,
    pr_number   INTEGER NOT NULL,
    kind        TEXT    NOT NULL,
    thread_id   TEXT,
    created_at  INTEGER NOT NULL,
    FOREIGN KEY(outbox_id) REFERENCES action_outbox(id) ON DELETE CASCADE
);
INSERT INTO outbox_review_claim_new SELECT * FROM outbox_review_claim;
DROP TABLE outbox_review_claim;
ALTER TABLE outbox_review_claim_new RENAME TO outbox_review_claim;
PRAGMA foreign_keys=ON;
"#;

const SCHEMA_V8: &str = r#"
CREATE TABLE IF NOT EXISTS terminal_audit (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms       INTEGER NOT NULL,
    listener_id TEXT    NOT NULL,
    action      TEXT    NOT NULL,
    ok          INTEGER NOT NULL,
    host        TEXT    NOT NULL,
    origin      TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_terminal_audit_listener_ts
    ON terminal_audit(listener_id, ts_ms);
"#;

const SCHEMA_V9: &str = r#"
ALTER TABLE inbox_event ADD COLUMN candidate_json TEXT;
ALTER TABLE action_outbox ADD COLUMN dedupe_key TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_action_outbox_dedupe
    ON action_outbox(project_id, dedupe_key)
    WHERE dedupe_key IS NOT NULL AND status = 'pending';
"#;

const SCHEMA_V10: &str = r#"
CREATE TABLE IF NOT EXISTS rule_match (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_id        TEXT    NOT NULL,
    rule_name      TEXT    NOT NULL,
    inbox_event_id INTEGER NOT NULL,
    project_id     TEXT    NOT NULL,
    action_count   INTEGER NOT NULL DEFAULT 0,
    error          TEXT,
    created_at     INTEGER NOT NULL,
    FOREIGN KEY(inbox_event_id) REFERENCES inbox_event(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_rule_match_inbox ON rule_match(inbox_event_id, id);
CREATE INDEX IF NOT EXISTS idx_rule_match_project ON rule_match(project_id, id DESC);

CREATE TABLE IF NOT EXISTS rule_match_action (
    rule_match_id    INTEGER NOT NULL,
    action_outbox_id INTEGER NOT NULL,
    PRIMARY KEY(rule_match_id, action_outbox_id),
    FOREIGN KEY(rule_match_id) REFERENCES rule_match(id) ON DELETE CASCADE,
    FOREIGN KEY(action_outbox_id) REFERENCES action_outbox(id) ON DELETE CASCADE
);
"#;

const SCHEMA_V11: &str = r#"
CREATE TABLE IF NOT EXISTS remote_access_audit (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms         INTEGER NOT NULL,
    entrypoint_id TEXT    NOT NULL,
    route         TEXT    NOT NULL,
    capability    TEXT    NOT NULL,
    gate          TEXT    NOT NULL,
    decision      TEXT    NOT NULL,
    peer_ip       TEXT    NOT NULL,
    effective_ip  TEXT    NOT NULL,
    host          TEXT    NOT NULL,
    origin        TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_remote_access_audit_entrypoint_ts
    ON remote_access_audit(entrypoint_id, ts_ms);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::thread;
    use std::time::Instant;

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
                "action_outbox",
                "config_blob",
                "dispatch_event",
                "dispatch_key",
                "inbox_event",
                "meta",
                "outbox_review_claim",
                "remote_access_audit",
                "review_history_item",
                "review_session",
                "terminal_audit",
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
            assert_eq!(SCHEMA_VERSION, 11, "current schema is v11");
            assert!(
                review_session_has_comment_url(conn),
                "fresh v0 → v2 has the comment_url column"
            );
            assert!(
                review_session_has_engine_kind(conn),
                "fresh v0 → v3 has the engine_kind column"
            );
            assert!(
                table_exists(conn, "inbox_event"),
                "fresh v0 → v4 has the inbox_event table"
            );
            assert!(
                table_exists(conn, "action_outbox"),
                "fresh v0 → v5 has the action_outbox table"
            );
            assert!(
                table_exists(conn, "outbox_review_claim"),
                "fresh v0 → v6 has the outbox_review_claim table"
            );
            assert!(
                table_has_fk(conn, "outbox_review_claim"),
                "fresh v0 → v7 rebuilt outbox_review_claim WITH the action_outbox FK cascade"
            );
            assert!(
                table_exists(conn, "terminal_audit"),
                "fresh v0 → v8 has the terminal_audit table"
            );
            assert!(
                index_exists(conn, "idx_terminal_audit_listener_ts"),
                "fresh v0 → v8 has the terminal_audit listener/time index"
            );
            assert!(
                table_has_column(conn, "inbox_event", "candidate_json"),
                "fresh v0 → v9 has inbox_event.candidate_json"
            );
            assert!(
                table_has_column(conn, "action_outbox", "dedupe_key"),
                "fresh v0 → v9 has action_outbox.dedupe_key"
            );
            assert!(
                index_exists(conn, "idx_action_outbox_dedupe"),
                "fresh v0 → v9 has pending-action dedupe index"
            );
            assert!(
                table_exists(conn, "rule_match"),
                "fresh v0 → v10 has the rule_match table"
            );
            assert!(
                table_has_fk(conn, "rule_match"),
                "fresh v0 → v10 links rule_match to inbox_event"
            );
            assert!(
                table_exists(conn, "rule_match_action"),
                "fresh v0 → v10 has the rule_match_action table"
            );
            assert!(
                table_has_fk(conn, "rule_match_action"),
                "fresh v0 → v10 links rule_match_action to match/outbox rows"
            );
            assert!(
                table_exists(conn, "remote_access_audit"),
                "fresh v0 → v11 has the remote_access_audit table"
            );
            assert!(
                index_exists(conn, "idx_remote_access_audit_entrypoint_ts"),
                "fresh v0 → v11 has the remote_access_audit entrypoint/time index"
            );
            Ok(())
        })
        .expect("query");
    }

    /// v2 → current migration lock: persisted review sessions must carry the engine that created
    /// them, so a follow-up after config changes routes back to the original engine.
    #[test]
    fn migrate_v2_to_current_adds_engine_kind_column() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        apply_v1(&conn).expect("seed v1");
        apply_v2(&conn).expect("seed v2");
        conn.pragma_update(None, "user_version", 2)
            .expect("stamp v2");
        assert!(
            !review_session_has_engine_kind(&conn),
            "v2 must not already have engine_kind"
        );

        // `run_migrations` replays ALL pending steps, so a v2 DB lands on the CURRENT schema
        // (v3's engine_kind AND every later step); this test's job is to lock that the v3 step
        // (engine_kind) runs on that path.
        run_migrations(&conn).expect("v2 -> current migrates");

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("read version");
        assert_eq!(version, SCHEMA_VERSION, "stamped to the current schema");
        assert!(
            review_session_has_engine_kind(&conn),
            "v3 added the engine_kind column"
        );
    }

    /// v3 → v4 migration lock (AB#1065): a DB stamped at v3 (no `inbox_event` table) must gain
    /// the inbox table and stamp to v4. Mirrors `migrate_v2_to_v3_…`: a missing table here means
    /// the inbox store's first query fails at runtime, not compile time, so pin the table's
    /// arrival on the existing-install upgrade path (the fresh-open path is covered by
    /// `migrations_create_all_tables_and_stamp_version`).
    #[test]
    fn migrate_v3_to_v4_adds_inbox_event_table() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        apply_v1(&conn).expect("seed v1");
        apply_v2(&conn).expect("seed v2");
        apply_v3(&conn).expect("seed v3");
        conn.pragma_update(None, "user_version", 3)
            .expect("stamp v3");
        assert!(
            !table_exists(&conn, "inbox_event"),
            "v3 must not already have inbox_event"
        );

        // `run_migrations` replays ALL pending steps, so a v3 DB lands on the CURRENT schema
        // (v4's inbox_event AND every later step); this test's job is to lock that the v4 step
        // (inbox_event) runs on that existing-install path.
        run_migrations(&conn).expect("v3 -> current migrates");

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("read version");
        assert_eq!(version, SCHEMA_VERSION, "stamped to the current schema");
        assert!(
            table_exists(&conn, "inbox_event"),
            "v4 added the inbox_event table"
        );
        // The replay-reconstruction column must be present too — `inbox::store::insert_dedup` /
        // `get_replayable` bind it by name, so a missing column fails at runtime, not compile
        // time. Pin it on the migration path alongside the table itself.
        assert!(
            table_has_column(&conn, "inbox_event", "webhook_event_json"),
            "v4 inbox_event has the webhook_event_json (replay) column"
        );
    }

    /// v4 → v5 migration lock (AB#1066): a DB stamped at v4 (no `action_outbox` table) must gain
    /// the outbox table and stamp to v5. Mirrors `migrate_v3_to_v4_…`: a missing table here means
    /// the outbox store's first query fails at runtime, not compile time, so pin the table's
    /// arrival on the existing-install upgrade path (the fresh-open path is covered by
    /// `migrations_create_all_tables_and_stamp_version`). Also pins the retry-bookkeeping columns
    /// (`attempt_count` / `next_attempt_at` / `last_error`) the worker binds by name.
    #[test]
    fn migrate_v4_to_v5_adds_action_outbox_table() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        apply_v1(&conn).expect("seed v1");
        apply_v2(&conn).expect("seed v2");
        apply_v3(&conn).expect("seed v3");
        apply_v4(&conn).expect("seed v4");
        conn.pragma_update(None, "user_version", 4)
            .expect("stamp v4");
        assert!(
            !table_exists(&conn, "action_outbox"),
            "v4 must not already have action_outbox"
        );

        // `run_migrations` replays ALL pending steps, so a v4 DB lands on the CURRENT schema
        // (v5's action_outbox AND every later step); this test's job is to lock that the v5 step
        // runs on the existing-install path.
        run_migrations(&conn).expect("v4 -> current migrates");

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("read version");
        assert_eq!(version, SCHEMA_VERSION, "stamped to the current schema");
        assert!(
            table_exists(&conn, "action_outbox"),
            "v5 added the action_outbox table"
        );
        for col in ["attempt_count", "next_attempt_at", "last_error"] {
            assert!(
                table_has_column(&conn, "action_outbox", col),
                "v5 action_outbox has the {col} retry column"
            );
        }
        // The worker's claim (`status, next_attempt_at`) and the panel's listing (`project_id, id`)
        // rely on these indexes; a typo in the DDL would only fail at runtime. Pin them here.
        for idx in ["idx_action_outbox_due", "idx_action_outbox_project"] {
            assert!(index_exists(&conn, idx), "v5 created the {idx} index");
        }
    }

    /// v5 → v6 migration lock (AB#1204): a DB stamped at v5 (no `outbox_review_claim` table) must
    /// gain the claim table at v6. Mirrors `migrate_v4_to_v5_…`: a missing table here means
    /// `review::claim_store`'s first query fails at runtime, not compile time, so pin the table's
    /// arrival on the existing-install upgrade path (fresh-open is covered by
    /// `migrations_create_all_tables_and_stamp_version`). Also pins the `thread_id` column the
    /// replay-resolution reads by name, and the **Hard** `outbox_id PRIMARY KEY` idempotency carrier.
    ///
    /// Runs the v6 step IN ISOLATION (`apply_v6`, not the full `run_migrations`): v6 is the FK-LESS
    /// claim table, and v7 (`migrate_v6_to_v7_adds_fk_cascade`) is what adds the FK. Keeping this
    /// step-scoped pins v6's exact shape independently of later rebuilds (the full-replay end state
    /// is covered by the fresh-open + v6→v7 tests).
    #[test]
    fn migrate_v5_to_v6_adds_outbox_review_claim_table() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        apply_v1(&conn).expect("seed v1");
        apply_v2(&conn).expect("seed v2");
        apply_v3(&conn).expect("seed v3");
        apply_v4(&conn).expect("seed v4");
        apply_v5(&conn).expect("seed v5");
        conn.pragma_update(None, "user_version", 5)
            .expect("stamp v5");
        assert!(
            !table_exists(&conn, "outbox_review_claim"),
            "v5 must not already have outbox_review_claim"
        );

        // Just the v6 delta — v6's claim table is FK-LESS (v7 adds the FK).
        apply_v6(&conn).expect("v5 -> v6 adds the claim table");

        assert!(
            table_exists(&conn, "outbox_review_claim"),
            "v6 added the outbox_review_claim table"
        );
        assert!(
            table_has_column(&conn, "outbox_review_claim", "thread_id"),
            "v6 outbox_review_claim has the thread_id (replay-resolution) column"
        );
        // v6 has NO FK yet (it is added by v7).
        assert!(
            !table_has_fk(&conn, "outbox_review_claim"),
            "v6 outbox_review_claim is FK-less (the FK arrives in v7)"
        );

        // Pin the **Hard** idempotency carrier (`outbox_id PRIMARY KEY`): INSERTing the SAME
        // `outbox_id` twice with `ON CONFLICT(outbox_id) DO NOTHING` (the exact shape
        // `review::claim_store::begin_claim` relies on) must not error and must not add a second row.
        // A schema drift that dropped the PK (or keyed on something else) would let the second INSERT
        // create a duplicate claim — re-opening the cross-restart duplicate-review window AB#1204 closes.
        let insert_claim = |conn: &Connection| {
            conn.execute(
                "INSERT INTO outbox_review_claim \
                 (outbox_id, project_id, pr_number, kind, created_at) \
                 VALUES (1, 'p1', 7, 'review', 0) \
                 ON CONFLICT(outbox_id) DO NOTHING",
                [],
            )
        };
        insert_claim(&conn).expect("first claim insert");
        insert_claim(&conn).expect("second claim insert (PK conflict → DO NOTHING, no error)");
        let claim_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM outbox_review_claim WHERE outbox_id = 1",
                [],
                |r| r.get(0),
            )
            .expect("count claims");
        assert_eq!(
            claim_rows, 1,
            "outbox_id PRIMARY KEY makes a duplicate claim unexpressible (still one row)"
        );
    }

    /// v6 → v7 migration lock (AB#1204): a DB stamped at v6 (FK-LESS `outbox_review_claim`) must gain
    /// the child FK `outbox_id → action_outbox(id) ON DELETE CASCADE` and stamp to v7. This is the
    /// **Hard** carrier proof: deleting an `action_outbox` row CASCADE-removes its claim, so an
    /// orphaned claim outliving its owning outbox row is unexpressible at the DB layer (the bound that
    /// pairs with F2's "Dead retains the claim"). Existing claim rows survive the table rebuild
    /// (`INSERT … SELECT *` copies them). Needs `foreign_keys=ON` for the cascade to fire at delete
    /// time — `open_in_memory` sets it, but this test opens a RAW connection, so it sets it explicitly.
    #[test]
    fn migrate_v6_to_v7_adds_fk_cascade() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .expect("enable fk");
        apply_v1(&conn).expect("seed v1");
        apply_v2(&conn).expect("seed v2");
        apply_v3(&conn).expect("seed v3");
        apply_v4(&conn).expect("seed v4");
        apply_v5(&conn).expect("seed v5");
        apply_v6(&conn).expect("seed v6");
        conn.pragma_update(None, "user_version", 6)
            .expect("stamp v6");
        // Precondition: the v6 claim table has NO FK.
        assert!(
            !table_has_fk(&conn, "outbox_review_claim"),
            "v6 must not already have the FK"
        );

        // Seed an `action_outbox` row + its claim BEFORE the migration, so the rebuild's
        // `INSERT … SELECT *` is exercised on real data (existing claims must survive).
        conn.execute(
            "INSERT INTO action_outbox \
             (id, project_id, kind, summary, payload, status, next_attempt_at, created_at, updated_at) \
             VALUES (100, 'p1', 'review', 's', '{}', 'pending', 0, 0, 0)",
            [],
        )
        .expect("seed action_outbox row");
        conn.execute(
            "INSERT INTO outbox_review_claim \
             (outbox_id, project_id, pr_number, kind, thread_id, created_at) \
             VALUES (100, 'p1', 7, 'review', 't-100', 0)",
            [],
        )
        .expect("seed claim");

        run_migrations(&conn).expect("v6 -> current migrates");

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("read version");
        assert_eq!(version, SCHEMA_VERSION, "stamped to the current schema");
        assert!(
            table_has_fk(&conn, "outbox_review_claim"),
            "v7 added the outbox_id → action_outbox FK"
        );
        // The pre-existing claim survived the table rebuild (INSERT … SELECT * copied it).
        let survived: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM outbox_review_claim WHERE outbox_id = 100",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(survived, 1, "existing claim survives the rebuild");

        // The Hard proof: deleting the owning action_outbox row CASCADE-deletes its claim.
        conn.execute("DELETE FROM action_outbox WHERE id = 100", [])
            .expect("delete owning row");
        let after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM outbox_review_claim WHERE outbox_id = 100",
                [],
                |r| r.get(0),
            )
            .expect("count after delete");
        assert_eq!(
            after, 0,
            "ON DELETE CASCADE reaps the claim when its owning outbox row is pruned"
        );
    }

    #[test]
    fn migrate_v7_to_v8_adds_terminal_audit_table() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .expect("enable fk");
        apply_v1(&conn).expect("seed v1");
        apply_v2(&conn).expect("seed v2");
        apply_v3(&conn).expect("seed v3");
        apply_v4(&conn).expect("seed v4");
        apply_v5(&conn).expect("seed v5");
        apply_v6(&conn).expect("seed v6");
        apply_v7(&conn).expect("seed v7");
        conn.pragma_update(None, "user_version", 7)
            .expect("stamp v7");
        assert!(
            !table_exists(&conn, "terminal_audit"),
            "v7 must not already have terminal_audit"
        );

        run_migrations(&conn).expect("v7 -> current migrates");

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("read version");
        assert_eq!(version, SCHEMA_VERSION, "stamped to the current schema");
        assert!(
            table_exists(&conn, "terminal_audit"),
            "v8 added the terminal_audit table"
        );
        assert!(
            index_exists(&conn, "idx_terminal_audit_listener_ts"),
            "v8 added the terminal_audit listener/time index"
        );
    }

    #[test]
    fn migrate_v8_to_v9_adds_candidate_and_outbox_dedupe_columns() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .expect("enable fk");
        apply_v1(&conn).expect("seed v1");
        apply_v2(&conn).expect("seed v2");
        apply_v3(&conn).expect("seed v3");
        apply_v4(&conn).expect("seed v4");
        apply_v5(&conn).expect("seed v5");
        apply_v6(&conn).expect("seed v6");
        apply_v7(&conn).expect("seed v7");
        apply_v8(&conn).expect("seed v8");
        conn.pragma_update(None, "user_version", 8)
            .expect("stamp v8");
        assert!(
            table_exists(&conn, "terminal_audit"),
            "v8 has terminal_audit"
        );
        assert!(
            !table_has_column(&conn, "inbox_event", "candidate_json"),
            "v8 must not already have candidate_json"
        );
        assert!(
            !table_has_column(&conn, "action_outbox", "dedupe_key"),
            "v8 must not already have dedupe_key"
        );

        run_migrations(&conn).expect("v8 -> current migrates");

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("read version");
        assert_eq!(version, SCHEMA_VERSION, "stamped to the current schema");
        assert!(
            table_has_column(&conn, "inbox_event", "candidate_json"),
            "v9 added inbox_event.candidate_json"
        );
        assert!(
            table_has_column(&conn, "action_outbox", "dedupe_key"),
            "v9 added action_outbox.dedupe_key"
        );
        assert!(
            index_exists(&conn, "idx_action_outbox_dedupe"),
            "v9 added the pending outbox dedupe index"
        );
    }

    #[test]
    fn migrate_v10_to_v11_adds_remote_access_audit_table() {
        let conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .expect("enable fk");
        apply_v1(&conn).expect("seed v1");
        apply_v2(&conn).expect("seed v2");
        apply_v3(&conn).expect("seed v3");
        apply_v4(&conn).expect("seed v4");
        apply_v5(&conn).expect("seed v5");
        apply_v6(&conn).expect("seed v6");
        apply_v7(&conn).expect("seed v7");
        apply_v8(&conn).expect("seed v8");
        apply_v9(&conn).expect("seed v9");
        apply_v10(&conn).expect("seed v10");
        conn.pragma_update(None, "user_version", 10)
            .expect("stamp v10");
        assert!(
            !table_exists(&conn, "remote_access_audit"),
            "v10 must not already have remote_access_audit"
        );

        run_migrations(&conn).expect("v10 -> current migrates");

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("read version");
        assert_eq!(version, SCHEMA_VERSION, "stamped to the current schema");
        assert!(
            table_exists(&conn, "remote_access_audit"),
            "v11 added the remote_access_audit table"
        );
        assert!(
            index_exists(&conn, "idx_remote_access_audit_entrypoint_ts"),
            "v11 added the remote_access_audit entrypoint/time index"
        );
    }

    /// Whether an index of the given name exists (via `sqlite_master`).
    fn index_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='index' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()
        .expect("query sqlite_master")
        .is_some()
    }

    /// Whether a table of the given name exists (via `sqlite_master`).
    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()
        .expect("query sqlite_master")
        .is_some()
    }

    /// Whether `table` has a column named `column` (via `PRAGMA table_info`). Generic over the
    /// table (vs `review_session_has_column`) so the inbox migration test can pin its columns.
    fn table_has_column(conn: &Connection, table: &str, column: &str) -> bool {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("table_info");
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1)) // col 1 = name
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        names.iter().any(|n| n == column)
    }

    /// Whether `table` has at least one FOREIGN KEY (via `PRAGMA foreign_key_list`). Used to pin the
    /// AB#1204 v7 FK arrival on `outbox_review_claim` (a missing FK = the cascade never fires, so an
    /// orphaned claim could outlive its outbox row — the exact regression v7 prevents).
    fn table_has_fk(conn: &Connection, table: &str) -> bool {
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM pragma_foreign_key_list('{table}')"),
                [],
                |r| r.get(0),
            )
            .expect("foreign_key_list");
        count > 0
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

    /// AB#1082 / issue #1376 spike: measure whether the single process-wide
    /// `Mutex<Connection>` becomes a meaningful bottleneck once outbox polling, inbox writes, PR
    /// list reads, and local-API status reads run concurrently. Ignored by default because it is a
    /// timing benchmark, not a deterministic regression assertion.
    ///
    /// Tuning knobs:
    /// - `PRMONITOR_DB_SPIKE_DURATION_MS` (default: 1500)
    /// - `PRMONITOR_DB_SPIKE_READERS` (default: 8)
    /// - `PRMONITOR_DB_SPIKE_SEED_ROWS` (default: 1000)
    #[test]
    #[ignore = "spike benchmark; run with --ignored --nocapture"]
    fn sqlite_read_pool_spike_under_outbox_poll_inbox_and_status_reads() {
        let cfg = SpikeConfig::from_env();
        println!(
            "sqlite contention spike: duration_ms={} readers={} seed_rows={}",
            cfg.duration.as_millis(),
            cfg.readers,
            cfg.seed_rows
        );

        let single = run_spike_mode("single_mutex", ReadRouting::SingleMutex, &cfg);
        let readonly_pool = run_spike_mode("readonly_pool", ReadRouting::ReadonlyPool, &cfg);
        print_spike_report(&single);
        print_spike_report(&readonly_pool);
        print_spike_decision(&single, &readonly_pool);
    }

    #[derive(Clone, Copy)]
    enum ReadRouting {
        SingleMutex,
        ReadonlyPool,
    }

    struct SpikeConfig {
        duration: Duration,
        readers: usize,
        seed_rows: usize,
    }

    impl SpikeConfig {
        fn from_env() -> Self {
            Self {
                duration: Duration::from_millis(env_u64("PRMONITOR_DB_SPIKE_DURATION_MS", 1500)),
                readers: env_usize("PRMONITOR_DB_SPIKE_READERS", 8).max(1),
                seed_rows: env_usize("PRMONITOR_DB_SPIKE_SEED_ROWS", 1000).max(10),
            }
        }
    }

    fn env_u64(name: &str, default: u64) -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default)
    }

    fn env_usize(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(default)
    }

    struct SpikeRunReport {
        mode: &'static str,
        elapsed: Duration,
        outbox_due_read: Vec<Duration>,
        pr_poll_read: Vec<Duration>,
        review_status_read: Vec<Duration>,
        inbox_write: Vec<Duration>,
        outbox_write: Vec<Duration>,
    }

    struct SpikeThreadStats {
        outbox_due_read: Vec<Duration>,
        pr_poll_read: Vec<Duration>,
        review_status_read: Vec<Duration>,
        inbox_write: Vec<Duration>,
        outbox_write: Vec<Duration>,
    }

    impl SpikeThreadStats {
        fn new() -> Self {
            Self {
                outbox_due_read: Vec::new(),
                pr_poll_read: Vec::new(),
                review_status_read: Vec::new(),
                inbox_write: Vec::new(),
                outbox_write: Vec::new(),
            }
        }

        fn merge_into(self, report: &mut SpikeRunReport) {
            report.outbox_due_read.extend(self.outbox_due_read);
            report.pr_poll_read.extend(self.pr_poll_read);
            report.review_status_read.extend(self.review_status_read);
            report.inbox_write.extend(self.inbox_write);
            report.outbox_write.extend(self.outbox_write);
        }
    }

    fn run_spike_mode(
        mode: &'static str,
        routing: ReadRouting,
        cfg: &SpikeConfig,
    ) -> SpikeRunReport {
        let path = temp_db_path(mode);
        cleanup(&path);

        let writer_conn = Connection::open(&path).expect("open writer db");
        let writer = Arc::new(Database::from_conn(writer_conn).expect("migrate writer db"));
        seed_spike_db(writer.as_ref(), cfg.seed_rows);

        let read_handles: Vec<Arc<Database>> = match routing {
            ReadRouting::SingleMutex => vec![Arc::clone(&writer); cfg.readers],
            ReadRouting::ReadonlyPool => (0..cfg.readers)
                .map(|_| Arc::new(Database::open_readonly_at(&path).expect("open readonly db")))
                .collect(),
        };

        let stop = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();
        for (worker_id, db) in read_handles.iter().enumerate() {
            let db = Arc::clone(db);
            let stop = Arc::clone(&stop);
            let seed_rows = cfg.seed_rows;
            workers.push(thread::spawn(move || {
                let mut stats = SpikeThreadStats::new();
                let mut n = worker_id as i64;
                while !stop.load(Ordering::Relaxed) {
                    record_latency(&mut stats.outbox_due_read, || {
                        read_due_outbox(db.as_ref(), n)
                    });
                    record_latency(&mut stats.pr_poll_read, || read_tracked_prs(db.as_ref()));
                    record_latency(&mut stats.review_status_read, || {
                        read_review_status(db.as_ref(), n, seed_rows)
                    });
                    n += 1;
                }
                stats
            }));
        }

        for writer_id in 0..2 {
            let db = Arc::clone(&writer);
            let stop = Arc::clone(&stop);
            let seed_rows = cfg.seed_rows;
            workers.push(thread::spawn(move || {
                let mut stats = SpikeThreadStats::new();
                let mut n = writer_id as i64;
                while !stop.load(Ordering::Relaxed) {
                    if writer_id == 0 {
                        record_latency(&mut stats.inbox_write, || {
                            write_inbox_event(db.as_ref(), n)
                        });
                    } else {
                        record_latency(&mut stats.outbox_write, || {
                            write_outbox_retry(db.as_ref(), n, seed_rows)
                        });
                    }
                    n += 1;
                }
                stats
            }));
        }

        let started = Instant::now();
        thread::sleep(cfg.duration);
        stop.store(true, Ordering::Relaxed);
        let elapsed = started.elapsed();

        let mut report = SpikeRunReport {
            mode,
            elapsed,
            outbox_due_read: Vec::new(),
            pr_poll_read: Vec::new(),
            review_status_read: Vec::new(),
            inbox_write: Vec::new(),
            outbox_write: Vec::new(),
        };
        for worker in workers {
            worker.join().expect("worker joins").merge_into(&mut report);
        }
        cleanup(&path);
        report
    }

    fn record_latency(samples: &mut Vec<Duration>, f: impl FnOnce() -> AppResult<()>) {
        let started = Instant::now();
        f().expect("spike operation succeeds");
        samples.push(started.elapsed());
    }

    fn seed_spike_db(db: &Database, rows: usize) {
        db.with_tx(|tx| {
            let mut outbox = tx
                .prepare(
                    "INSERT INTO action_outbox \
                     (project_id, kind, summary, payload, status, attempt_count, next_attempt_at, \
                      last_error, created_at, updated_at) \
                     VALUES ('project-a', 'notification', ?1, '{}', 'pending', 0, ?2, NULL, ?2, ?2)",
                )
                .map_err(map_err)?;
            let mut tracked = tx
                .prepare(
                    "INSERT INTO tracked_pr \
                     (project_id, number, title, labels_json, url, kind, skip_reason, \
                      first_seen_epoch, last_seen_epoch, archived) \
                     VALUES ('project-a', ?1, ?2, '[\"review\"]', ?3, 'review', NULL, ?4, ?4, 0)",
                )
                .map_err(map_err)?;
            let mut session = tx
                .prepare(
                    "INSERT INTO review_session \
                     (thread_id, project_id, pr_number, turn_id, kind, status, created_at, \
                      updated_at, comment_url, engine_kind) \
                     VALUES (?1, 'project-a', ?2, 'turn', 'review', 'running', ?3, ?3, NULL, 'codex')",
                )
                .map_err(map_err)?;
            for i in 0..rows {
                let n = i as i64;
                outbox
                    .execute(rusqlite::params![
                        format!("seed action {i}"),
                        (n % 60) - 30,
                    ])
                    .map_err(map_err)?;
                tracked
                    .execute(rusqlite::params![
                        n + 1,
                        format!("PR {i}"),
                        format!("https://example.invalid/pr/{i}"),
                        n,
                    ])
                    .map_err(map_err)?;
                session
                    .execute(rusqlite::params![
                        format!("thread-{i}"),
                        n + 1,
                        n,
                    ])
                    .map_err(map_err)?;
            }
            Ok(())
        })
        .expect("seed spike rows");
    }

    fn read_due_outbox(db: &Database, n: i64) -> AppResult<()> {
        db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM action_outbox \
                 WHERE status = 'pending' AND next_attempt_at <= ?1 ORDER BY id LIMIT 25",
            )?;
            let ids: Vec<i64> = stmt
                .query_map([n % 60], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<_>>()?;
            std::hint::black_box(ids);
            Ok(())
        })
    }

    fn read_tracked_prs(db: &Database) -> AppResult<()> {
        db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT number, title, labels_json FROM tracked_pr \
                 WHERE project_id = 'project-a' ORDER BY number DESC LIMIT 100",
            )?;
            let rows: Vec<(i64, String, String)> = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?;
            std::hint::black_box(rows);
            Ok(())
        })
    }

    fn read_review_status(db: &Database, n: i64, seed_rows: usize) -> AppResult<()> {
        let idx = n.rem_euclid(seed_rows as i64);
        db.with_conn(|conn| {
            let row: Option<(String, Option<String>)> = conn
                .query_row(
                    "SELECT status, comment_url FROM review_session WHERE thread_id = ?1",
                    [format!("thread-{idx}")],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                )
                .optional()?;
            std::hint::black_box(row);
            Ok(())
        })
    }

    fn write_inbox_event(db: &Database, n: i64) -> AppResult<()> {
        db.with_tx(|tx| {
            tx.execute(
                "INSERT INTO inbox_event \
                 (dedupe_key, source, event_type, project_id, repo, number, event_json, \
                  raw_payload, webhook_event_json, status, received_at_epoch) \
                 VALUES (?1, 'github', 'pr', 'project-a', 'owner/repo', ?2, '{}', '{}', NULL, \
                         'received', ?2) \
                 ON CONFLICT(dedupe_key) DO NOTHING",
                rusqlite::params![format!("spike:{n}"), n],
            )
            .map_err(map_err)?;
            Ok(())
        })
    }

    fn write_outbox_retry(db: &Database, n: i64, seed_rows: usize) -> AppResult<()> {
        let id = n.rem_euclid(seed_rows as i64) + 1;
        db.with_conn(|conn| {
            conn.execute(
                "UPDATE action_outbox \
                 SET attempt_count = attempt_count + 1, next_attempt_at = ?2, updated_at = ?2 \
                 WHERE id = ?1",
                rusqlite::params![id, n],
            )
            .map(|_| ())
        })
    }

    fn print_spike_report(report: &SpikeRunReport) {
        println!();
        println!("mode={}", report.mode);
        print_metric(
            report.mode,
            "outbox_due_read",
            &report.outbox_due_read,
            report.elapsed,
        );
        print_metric(
            report.mode,
            "pr_poll_read",
            &report.pr_poll_read,
            report.elapsed,
        );
        print_metric(
            report.mode,
            "review_status_read",
            &report.review_status_read,
            report.elapsed,
        );
        print_metric(
            report.mode,
            "inbox_write",
            &report.inbox_write,
            report.elapsed,
        );
        print_metric(
            report.mode,
            "outbox_write",
            &report.outbox_write,
            report.elapsed,
        );
    }

    fn print_metric(mode: &str, op: &str, samples: &[Duration], elapsed: Duration) {
        let summary = LatencySummary::from(samples);
        println!(
            "{mode}.{op}: count={} ops_per_sec={:.1} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} max_ms={:.3}",
            samples.len(),
            samples.len() as f64 / elapsed.as_secs_f64(),
            summary.p50_ms,
            summary.p95_ms,
            summary.p99_ms,
            summary.max_ms,
        );
    }

    fn print_spike_decision(single: &SpikeRunReport, readonly_pool: &SpikeRunReport) {
        let single_tail = worst_read_tail(single);
        let pool_tail = worst_read_tail(readonly_pool);
        let p95_improvement = improvement(single_tail.p95_ms, pool_tail.p95_ms);
        let p99_improvement = improvement(single_tail.p99_ms, pool_tail.p99_ms);
        println!();
        println!(
            "decision_input: single_read_p95_ms={:.3} single_read_p99_ms={:.3} \
             readonly_pool_read_p95_ms={:.3} readonly_pool_read_p99_ms={:.3} \
             p95_improvement={:.1}% p99_improvement={:.1}%",
            single_tail.p95_ms,
            single_tail.p99_ms,
            pool_tail.p95_ms,
            pool_tail.p99_ms,
            p95_improvement * 100.0,
            p99_improvement * 100.0,
        );
        if single_tail.p95_ms <= 25.0 && p95_improvement < 0.30 {
            println!(
                "decision: no immediate read pool; keep the single writer Database and defer production changes"
            );
        } else if (single_tail.p95_ms > 50.0 || single_tail.p99_ms > 200.0)
            && (p95_improvement >= 0.40 || p99_improvement >= 0.40)
        {
            println!(
                "decision: create follow-up for a read-only pool; keep exactly one writer connection"
            );
        } else {
            println!(
                "decision: inconclusive; rerun with a longer duration and larger seed before changing production"
            );
        }
    }

    fn worst_read_tail(report: &SpikeRunReport) -> ReadTailSummary {
        [
            LatencySummary::from(&report.outbox_due_read),
            LatencySummary::from(&report.pr_poll_read),
            LatencySummary::from(&report.review_status_read),
        ]
        .into_iter()
        .fold(ReadTailSummary::default(), |worst, summary| {
            ReadTailSummary {
                p95_ms: worst.p95_ms.max(summary.p95_ms),
                p99_ms: worst.p99_ms.max(summary.p99_ms),
            }
        })
    }

    #[derive(Default)]
    struct ReadTailSummary {
        p95_ms: f64,
        p99_ms: f64,
    }

    fn improvement(before: f64, after: f64) -> f64 {
        if before <= f64::EPSILON {
            0.0
        } else {
            ((before - after) / before).max(0.0)
        }
    }

    struct LatencySummary {
        p50_ms: f64,
        p95_ms: f64,
        p99_ms: f64,
        max_ms: f64,
    }

    impl LatencySummary {
        fn from(samples: &[Duration]) -> Self {
            if samples.is_empty() {
                return Self {
                    p50_ms: 0.0,
                    p95_ms: 0.0,
                    p99_ms: 0.0,
                    max_ms: 0.0,
                };
            }
            let mut nanos: Vec<u128> = samples.iter().map(Duration::as_nanos).collect();
            nanos.sort_unstable();
            Self {
                p50_ms: nanos[percentile_index(nanos.len(), 50)] as f64 / 1_000_000.0,
                p95_ms: nanos[percentile_index(nanos.len(), 95)] as f64 / 1_000_000.0,
                p99_ms: nanos[percentile_index(nanos.len(), 99)] as f64 / 1_000_000.0,
                max_ms: *nanos.last().expect("non-empty") as f64 / 1_000_000.0,
            }
        }
    }

    fn percentile_index(len: usize, percentile: usize) -> usize {
        ((len - 1) * percentile) / 100
    }

    #[test]
    fn spike_decision_uses_worst_read_path_not_merged_read_samples() {
        let report = SpikeRunReport {
            mode: "single_mutex",
            elapsed: Duration::from_secs(1),
            outbox_due_read: vec![Duration::from_millis(75); 5],
            pr_poll_read: vec![Duration::from_millis(1); 500],
            review_status_read: vec![Duration::from_millis(1); 500],
            inbox_write: Vec::new(),
            outbox_write: Vec::new(),
        };

        let mut merged = Vec::new();
        merged.extend(report.outbox_due_read.iter().copied());
        merged.extend(report.pr_poll_read.iter().copied());
        merged.extend(report.review_status_read.iter().copied());

        let merged_summary = LatencySummary::from(&merged);
        let worst_tail = worst_read_tail(&report);

        assert_eq!(
            merged_summary.p99_ms, 1.0,
            "merged samples hide a small hot read path behind many fast reads"
        );
        assert_eq!(worst_tail.p95_ms, 75.0);
        assert_eq!(worst_tail.p99_ms, 75.0);
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
