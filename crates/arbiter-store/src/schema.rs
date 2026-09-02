//! Schema application and versioning for `run.db` and `history.db`. ARCHITECTURE
//! §8.7: migrations applied in order, recorded in `schema_metadata`; opening a
//! store whose `db_schema_version` is newer than the binary is refused, not
//! guessed at.

use rusqlite::Connection;

/// The `db_schema_version` this binary understands. Bump alongside a new
/// migration file.
pub const CURRENT_DB_SCHEMA_VERSION: i64 = 1;

/// `run.db`'s only migration so far — `events`, `run`, `schema_metadata`,
/// `budget`, `provider_calls`, `cache_entries`, `artifacts` (PLAN_DEVIATIONS.md
/// D21/D29 explain why `stages` and the ten claim-graph/decision projections
/// ARCHITECTURE §8.1 names are not here yet).
const RUN_DB_MIGRATION_0001: &str = include_str!("../migrations/0001_initial.sql");

/// `history.db`'s schema — `run_catalog`, transcribed verbatim from ARCHITECTURE
/// §8.5's own `CREATE TABLE`, plus its own `schema_metadata` (a separate SQLite
/// file needs its own version row; `db_schema_version` is one axis governing two
/// independently-versioned files, per §8.7's own wording: "table layout in
/// `run.db` / `history.db`"). Kept as a Rust constant rather than a second
/// `migrations/` directory since the plan's file list for this task names only
/// one migrations file.
const HISTORY_DB_MIGRATION_0001: &str = r#"
CREATE TABLE run_catalog (
  run_id          TEXT PRIMARY KEY,
  status          TEXT NOT NULL,      -- running | completed | failed | abandoned
  question        TEXT NOT NULL,
  outcome         TEXT,               -- null while running
  confidence      REAL,
  margin          REAL,
  cost            REAL NOT NULL DEFAULT 0,
  orphaned_cost   REAL NOT NULL DEFAULT 0,
  duration_ms     INTEGER,
  model_count     INTEGER,
  depth           TEXT,
  policy_version  TEXT NOT NULL,      -- history is only comparable within one
  started_at      TEXT NOT NULL,
  completed_at    TEXT,
  run_path        TEXT NOT NULL
);
CREATE INDEX ix_catalog_time    ON run_catalog(started_at DESC);
CREATE INDEX ix_catalog_outcome ON run_catalog(policy_version, outcome, confidence);

CREATE TABLE schema_metadata (
    db_schema_version INTEGER NOT NULL,
    engine_version    TEXT NOT NULL,
    created_at        TEXT NOT NULL
);
"#;

#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error(
        "db_schema_version {found} is newer than this binary supports ({supported}); refusing to open"
    )]
    TooNew { found: i64, supported: i64 },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// Applies `run.db`'s schema to a fresh connection and records the initial
/// `schema_metadata` row. Idempotent: a connection that already has a
/// `schema_metadata` row is checked against [`CURRENT_DB_SCHEMA_VERSION`] instead
/// of re-run.
pub fn open_run_db(conn: &Connection, engine_version: &str, now: &str) -> Result<(), SchemaError> {
    open_with(conn, RUN_DB_MIGRATION_0001, engine_version, now)
}

/// Same contract as [`open_run_db`], for `history.db`.
pub fn open_history_db(
    conn: &Connection,
    engine_version: &str,
    now: &str,
) -> Result<(), SchemaError> {
    open_with(conn, HISTORY_DB_MIGRATION_0001, engine_version, now)
}

fn open_with(
    conn: &Connection,
    migration: &str,
    engine_version: &str,
    now: &str,
) -> Result<(), SchemaError> {
    let table_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_metadata')",
        [],
        |row| row.get(0),
    )?;

    if !table_exists {
        conn.execute_batch(migration)?;
        conn.execute(
            "INSERT INTO schema_metadata (db_schema_version, engine_version, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![CURRENT_DB_SCHEMA_VERSION, engine_version, now],
        )?;
        return Ok(());
    }

    let found: i64 = conn.query_row(
        "SELECT db_schema_version FROM schema_metadata LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    if found > CURRENT_DB_SCHEMA_VERSION {
        return Err(SchemaError::TooNew {
            found,
            supported: CURRENT_DB_SCHEMA_VERSION,
        });
    }
    // found <= CURRENT_DB_SCHEMA_VERSION: nothing to migrate yet -- there is only
    // one migration file so far. A future 0002_*.sql applies here, in order,
    // between this check and returning.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_is_rowid_alias() {
        let conn = Connection::open_in_memory().unwrap();
        open_run_db(&conn, "0.1.0", "2026-09-02T00:00:00Z").unwrap();

        conn.execute(
            "INSERT INTO events (seq, run_id, schema_version, event_id, timestamp, stage, event_type, durable, payload, content_hash, previous_event_hash)
             VALUES (1, 'run_1', 1, 'evt_1', '2026-09-02T00:00:00Z', 'claims.extract', 'CLAIM_EXTRACTED', 0, '{}', 'blake3:a', NULL)",
            [],
        )
        .unwrap();

        // rowid() must equal the seq we inserted -- proof `seq INTEGER PRIMARY
        // KEY` is a true rowid alias, not an ordinary indexed column, which is
        // what makes `ORDER BY seq` a sequential scan rather than a sort (§8.1).
        let rowid: i64 = conn
            .query_row("SELECT rowid FROM events WHERE seq = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rowid, 1);
    }

    #[test]
    fn opening_a_newer_db_schema_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        open_run_db(&conn, "0.1.0", "2026-09-02T00:00:00Z").unwrap();

        // Simulate a database written by a future binary.
        conn.execute(
            "UPDATE schema_metadata SET db_schema_version = ?1",
            [CURRENT_DB_SCHEMA_VERSION + 1],
        )
        .unwrap();

        let result = open_run_db(&conn, "0.1.0", "2026-09-02T00:00:00Z");
        assert!(matches!(
            result,
            Err(SchemaError::TooNew { found, supported })
                if found == CURRENT_DB_SCHEMA_VERSION + 1 && supported == CURRENT_DB_SCHEMA_VERSION
        ));
    }

    #[test]
    fn opening_an_already_initialized_db_is_a_no_op() {
        let conn = Connection::open_in_memory().unwrap();
        open_run_db(&conn, "0.1.0", "2026-09-02T00:00:00Z").unwrap();
        // Second open on the same connection must not fail or double-insert.
        open_run_db(&conn, "0.1.0", "2026-09-02T00:00:01Z").unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_metadata", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn history_db_creates_run_catalog_with_both_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        open_history_db(&conn, "0.1.0", "2026-09-02T00:00:00Z").unwrap();

        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND tbl_name='run_catalog' AND name LIKE 'ix_catalog_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 2);
    }

    #[test]
    fn no_compound_run_id_seq_index_exists_on_events() {
        let conn = Connection::open_in_memory().unwrap();
        open_run_db(&conn, "0.1.0", "2026-09-02T00:00:00Z").unwrap();

        // SQLite auto-creates one index for the `event_id UNIQUE` constraint --
        // that's expected and unrelated. What must not exist is any index that
        // also covers `run_id`, since `seq` alone (the primary key / rowid alias)
        // already gives free ordering and `run_id` is constant within a run.db.
        let mut stmt = conn
            .prepare(
                "SELECT sql FROM sqlite_master WHERE type='index' AND tbl_name='events' AND sql IS NOT NULL",
            )
            .unwrap();
        let indexes: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            indexes.iter().all(|sql| !sql.contains("run_id")),
            "found an index covering run_id on events, which would duplicate the seq primary key: {indexes:?}"
        );
    }
}
