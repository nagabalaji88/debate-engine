//! `history.db`'s `run_catalog`: one insert at run start, one update at
//! completion, and `reindex` to rebuild the whole table from `run.db` files on
//! disk. ARCHITECTURE §8.5, INTERFACES §1.
//!
//! WAL + `busy_timeout` (5,000 ms) is what lets "any process, twice per run"
//! (§8.5's own words) not mean readers blocking on writers.

use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Schema(#[from] crate::schema::SchemaError),
    #[error("reading runs directory: {0}")]
    Io(#[from] std::io::Error),
}

/// Opens (creating if needed) `history.db` at `path`, in WAL mode with the
/// 5,000 ms `busy_timeout` §8.5 specifies, and applies its schema.
pub fn open_history_db(
    path: &Path,
    engine_version: &str,
    now: &str,
) -> Result<Connection, CatalogError> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    crate::schema::open_history_db(&conn, engine_version, now)?;
    Ok(conn)
}

/// What a run knows about itself at `init`, before anything has happened.
#[derive(Debug, Clone)]
pub struct NewRun {
    pub run_id: String,
    pub question: String,
    pub policy_version: String,
    pub started_at: String,
    pub run_path: String,
}

/// One row, `status = 'running'`. A row left in this state by a killed process
/// is exactly the signal `arbiter resume`/`doctor` read (§8.5).
pub fn insert_running(conn: &Connection, run: &NewRun) -> Result<(), CatalogError> {
    conn.execute(
        "INSERT INTO run_catalog (run_id, status, question, policy_version, started_at, run_path, cost, orphaned_cost)
         VALUES (?1, 'running', ?2, ?3, ?4, ?5, 0, 0)",
        params![run.run_id, run.question, run.policy_version, run.started_at, run.run_path],
    )?;
    Ok(())
}

/// What's known once a run stops, one way or another.
#[derive(Debug, Clone)]
pub struct Completion {
    pub run_id: String,
    /// `completed | failed | abandoned` — never `running` again (§8.5's own
    /// column comment lists exactly these four states total).
    pub status: String,
    pub outcome: Option<String>,
    pub confidence: Option<f64>,
    pub margin: Option<f64>,
    pub cost: f64,
    pub orphaned_cost: f64,
    pub duration_ms: Option<i64>,
    pub model_count: Option<i64>,
    pub depth: Option<String>,
    pub completed_at: String,
}

/// The single update at completion (§8.5: "a run inserts one row at start...
/// and updates it at completion").
pub fn update_completion(conn: &Connection, c: &Completion) -> Result<(), CatalogError> {
    conn.execute(
        "UPDATE run_catalog SET
            status = ?1, outcome = ?2, confidence = ?3, margin = ?4,
            cost = ?5, orphaned_cost = ?6, duration_ms = ?7, model_count = ?8,
            depth = ?9, completed_at = ?10
         WHERE run_id = ?11",
        params![
            c.status,
            c.outcome,
            c.confidence,
            c.margin,
            c.cost,
            c.orphaned_cost,
            c.duration_ms,
            c.model_count,
            c.depth,
            c.completed_at,
            c.run_id,
        ],
    )?;
    Ok(())
}

/// One `run_catalog` row, as `arbiter history` reads it back.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    pub run_id: String,
    pub status: String,
    pub question: String,
    pub outcome: Option<String>,
    pub confidence: Option<f64>,
    pub margin: Option<f64>,
    pub cost: f64,
    pub model_count: Option<i64>,
    pub depth: Option<String>,
    pub policy_version: String,
    pub started_at: String,
    pub completed_at: Option<String>,
}

/// `arbiter history [--outcome · --since · --min-confidence]` (ARCHITECTURE
/// §12) — every filter optional and AND-ed together.
#[derive(Debug, Clone, Default)]
pub struct HistoryFilter {
    pub outcome: Option<String>,
    /// An RFC3339 timestamp; only runs `started_at >=` this are returned.
    pub since: Option<String>,
    pub min_confidence: Option<f64>,
}

/// Newest-started first, matching `ix_catalog_time`'s own `started_at DESC`
/// ordering — the index this query is built to use.
pub fn list_runs(
    conn: &Connection,
    filter: &HistoryFilter,
) -> Result<Vec<RunSummary>, CatalogError> {
    let mut sql = "SELECT run_id, status, question, outcome, confidence, margin, cost, \
                    model_count, depth, policy_version, started_at, completed_at \
                    FROM run_catalog WHERE 1=1"
        .to_string();
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(outcome) = &filter.outcome {
        args.push(Box::new(outcome.clone()));
        sql.push_str(&format!(" AND outcome = ?{}", args.len()));
    }
    if let Some(since) = &filter.since {
        args.push(Box::new(since.clone()));
        sql.push_str(&format!(" AND started_at >= ?{}", args.len()));
    }
    if let Some(min_confidence) = filter.min_confidence {
        args.push(Box::new(min_confidence));
        sql.push_str(&format!(" AND confidence >= ?{}", args.len()));
    }
    sql.push_str(" ORDER BY started_at DESC");

    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
    let rows = stmt
        .query_map(params.as_slice(), |r| {
            Ok(RunSummary {
                run_id: r.get(0)?,
                status: r.get(1)?,
                question: r.get(2)?,
                outcome: r.get(3)?,
                confidence: r.get(4)?,
                margin: r.get(5)?,
                cost: r.get(6)?,
                model_count: r.get(7)?,
                depth: r.get(8)?,
                policy_version: r.get(9)?,
                started_at: r.get(10)?,
                completed_at: r.get(11)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Rebuilds `run_catalog` by scanning `runs_root` for `<run_id>/run.db` files
/// and upserting one row per run found — "a scan and an upsert, no watermark, no
/// delta pass, no lock choreography" (§8.1's changelog on this exact rewrite).
///
/// Limited today to what `run.db` actually stores (S1: `events`, `run`,
/// `schema_metadata` only — D21): `run_id`, `run_path` and `started_at`/`status`
/// from the `run` table. `question`, `outcome`, `confidence`, `margin`,
/// `model_count` and `depth` are not yet derivable from any table `run.db`
/// carries — those land once S4 gives `run.db` a `decision` projection to read
/// them back from. Until then `reindex` upserts what it can and leaves the rest
/// `NULL`, rather than guessing.
pub fn reindex(history_conn: &Connection, runs_root: &Path) -> Result<usize, CatalogError> {
    let mut count = 0;
    let entries = match std::fs::read_dir(runs_root) {
        Ok(e) => e,
        // A runs directory that simply doesn't exist yet (a fresh project, no
        // run has ever been started) is not an error -- reindex has nothing to
        // do. Any other failure (permissions, a path that exists but isn't a
        // directory, ...) is a real problem and must not be swallowed the same
        // way, or a misconfigured `runs_root` would silently report "0 runs
        // indexed" instead of surfacing why.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(CatalogError::Io(e)),
    };
    for entry in entries.flatten() {
        let run_dir = entry.path();
        let db_path = run_dir.join("run.db");
        if !db_path.is_file() {
            continue;
        }
        let run_conn =
            Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let row: Option<(String, String, i64)> = run_conn
            .query_row(
                "SELECT run_id, started_at, lease_epoch FROM run LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((run_id, started_at, _lease_epoch)) = row else {
            continue;
        };

        let run_path = run_dir.to_string_lossy().to_string();
        // Upsert: try update first (the common case, a previously-indexed run);
        // insert if no row existed yet.
        let updated = history_conn.execute(
            "UPDATE run_catalog SET started_at = ?1, run_path = ?2 WHERE run_id = ?3",
            params![started_at, run_path, run_id],
        )?;
        if updated == 0 {
            history_conn.execute(
                "INSERT INTO run_catalog (run_id, status, question, policy_version, started_at, run_path, cost, orphaned_cost)
                 VALUES (?1, 'abandoned', '', '', ?2, ?3, 0, 0)",
                params![run_id, started_at, run_path],
            )?;
        }
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn temp_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "arbiter_catalog_test_{label}_{}_{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn new_run(id: &str) -> NewRun {
        NewRun {
            run_id: id.to_string(),
            question: "modular monolith or microservices?".to_string(),
            policy_version: "argument-v1".to_string(),
            started_at: crate::now_rfc3339(),
            run_path: format!("/runs/{id}"),
        }
    }

    #[test]
    fn insert_then_update_moves_a_run_from_running_to_completed() {
        let path = temp_path("lifecycle");
        let conn = open_history_db(&path, "0.1.0", &crate::now_rfc3339()).unwrap();
        insert_running(&conn, &new_run("run_1")).unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM run_catalog WHERE run_id = 'run_1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "running");

        update_completion(
            &conn,
            &Completion {
                run_id: "run_1".to_string(),
                status: "completed".to_string(),
                outcome: Some("MAJORITY_WITH_DISSENT".to_string()),
                confidence: Some(0.84),
                margin: Some(0.30),
                cost: 1.23,
                orphaned_cost: 0.0,
                duration_ms: Some(42_000),
                model_count: Some(5),
                depth: Some("standard".to_string()),
                completed_at: crate::now_rfc3339(),
            },
        )
        .unwrap();

        let (status, outcome, confidence): (String, Option<String>, Option<f64>) = conn
            .query_row(
                "SELECT status, outcome, confidence FROM run_catalog WHERE run_id = 'run_1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "completed");
        assert_eq!(outcome.as_deref(), Some("MAJORITY_WITH_DISSENT"));
        assert!((confidence.unwrap() - 0.84).abs() < 1e-9);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn concurrent_writers_do_not_block_readers() {
        let path = temp_path("concurrency");
        {
            // Ensure the schema exists before threads race on it.
            open_history_db(&path, "0.1.0", &crate::now_rfc3339()).unwrap();
        }
        let path = Arc::new(path);

        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            let conn = open_history_db(&writer_path, "0.1.0", &crate::now_rfc3339()).unwrap();
            for i in 0..50 {
                insert_running(&conn, &new_run(&format!("run_{i}"))).unwrap();
                update_completion(
                    &conn,
                    &Completion {
                        run_id: format!("run_{i}"),
                        status: "completed".to_string(),
                        outcome: None,
                        confidence: None,
                        margin: None,
                        cost: 0.0,
                        orphaned_cost: 0.0,
                        duration_ms: None,
                        model_count: None,
                        depth: None,
                        completed_at: crate::now_rfc3339(),
                    },
                )
                .unwrap();
            }
        });

        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            let conn = Connection::open(&*reader_path).unwrap();
            conn.busy_timeout(Duration::from_millis(5000)).unwrap();
            // Every read must complete without erroring while the writer above
            // is concurrently inserting/updating -- that's the WAL guarantee
            // this test exists to prove, not any particular row count seen.
            for _ in 0..50 {
                let _: i64 = conn
                    .query_row("SELECT COUNT(*) FROM run_catalog", [], |r| r.get(0))
                    .unwrap();
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();

        let conn = Connection::open(&*path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM run_catalog", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 50);

        let _ = std::fs::remove_file(&*path);
    }

    #[test]
    fn list_runs_filters_by_outcome_since_and_min_confidence() {
        let path = temp_path("list_runs");
        let conn = open_history_db(&path, "0.1.0", &crate::now_rfc3339()).unwrap();

        let rows = [
            ("run_early", "2026-01-01T00:00:00Z", "Consensus", 0.9),
            ("run_late_low", "2026-06-01T00:00:00Z", "Consensus", 0.4),
            (
                "run_late_high",
                "2026-06-01T00:00:00Z",
                "SplitDecision",
                0.7,
            ),
        ];
        for (id, started_at, outcome, confidence) in rows {
            insert_running(
                &conn,
                &NewRun {
                    run_id: id.to_string(),
                    question: "q".to_string(),
                    policy_version: "argument-v1".to_string(),
                    started_at: started_at.to_string(),
                    run_path: format!("/runs/{id}"),
                },
            )
            .unwrap();
            update_completion(
                &conn,
                &Completion {
                    run_id: id.to_string(),
                    status: "completed".to_string(),
                    outcome: Some(outcome.to_string()),
                    confidence: Some(confidence),
                    margin: Some(0.1),
                    cost: 0.5,
                    orphaned_cost: 0.0,
                    duration_ms: Some(1000),
                    model_count: Some(3),
                    depth: Some("standard".to_string()),
                    completed_at: crate::now_rfc3339(),
                },
            )
            .unwrap();
        }

        let all = list_runs(&conn, &HistoryFilter::default()).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(
            all[0].run_id, "run_late_low",
            "newest started_at first (tie broken by insertion order among equal timestamps is not asserted)"
        );

        let by_outcome = list_runs(
            &conn,
            &HistoryFilter {
                outcome: Some("Consensus".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(by_outcome.len(), 2);
        assert!(
            by_outcome
                .iter()
                .all(|r| r.outcome.as_deref() == Some("Consensus"))
        );

        let since = list_runs(
            &conn,
            &HistoryFilter {
                since: Some("2026-03-01T00:00:00Z".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(since.len(), 2);

        let min_conf = list_runs(
            &conn,
            &HistoryFilter {
                min_confidence: Some(0.6),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(min_conf.len(), 2);
        assert!(min_conf.iter().all(|r| r.confidence.unwrap() >= 0.6));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reindex_upserts_one_row_per_run_db_found() {
        use crate::sqlite_store::SqliteRunStore;
        use arbiter_core::{PolicyVersion, RunId};
        use arbiter_kernel::store::{Manifest, RunStore};

        let runs_root = temp_path("reindex_runs");
        std::fs::create_dir_all(&runs_root).unwrap();
        let store = SqliteRunStore::new(&runs_root);
        let manifest = Manifest {
            policy_version: PolicyVersion::new("argument-v1"),
            config_hash: "blake3:c".to_string(),
            pack_hash: "blake3:p".to_string(),
            correlation_table_version: "1".to_string(),
            rng_seed: 1,
        };
        store.create(&RunId::new("run_a"), &manifest).unwrap();
        store.create(&RunId::new("run_b"), &manifest).unwrap();

        let history_path = temp_path("reindex_history");
        let history_conn = open_history_db(&history_path, "0.1.0", &crate::now_rfc3339()).unwrap();

        let n = reindex(&history_conn, &runs_root).unwrap();
        assert_eq!(n, 2);

        let count: i64 = history_conn
            .query_row("SELECT COUNT(*) FROM run_catalog", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);

        // Idempotent: reindexing again must not duplicate rows.
        reindex(&history_conn, &runs_root).unwrap();
        let count_again: i64 = history_conn
            .query_row("SELECT COUNT(*) FROM run_catalog", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count_again, 2);

        let _ = std::fs::remove_dir_all(&runs_root);
        let _ = std::fs::remove_file(&history_path);
    }

    #[test]
    fn reindex_treats_a_missing_runs_directory_as_zero_runs_not_an_error() {
        let history_path = temp_path("reindex_missing_dir");
        let history_conn = open_history_db(&history_path, "0.1.0", &crate::now_rfc3339()).unwrap();
        let never_created = temp_path("reindex_never_created_dir");

        let n = reindex(&history_conn, &never_created).unwrap();
        assert_eq!(n, 0);

        let _ = std::fs::remove_file(&history_path);
    }

    #[test]
    fn reindex_surfaces_a_real_io_error_instead_of_silently_reporting_zero() {
        let history_path = temp_path("reindex_real_error");
        let history_conn = open_history_db(&history_path, "0.1.0", &crate::now_rfc3339()).unwrap();

        // A path that exists but is a file, not a directory: read_dir fails
        // with something other than NotFound, which must propagate as an
        // error rather than being swallowed into "0 runs indexed".
        let not_a_directory = temp_path("reindex_not_a_directory");
        std::fs::write(&not_a_directory, b"not a directory").unwrap();

        let result = reindex(&history_conn, &not_a_directory);
        assert!(
            matches!(result, Err(CatalogError::Io(_))),
            "expected an Io error, got {result:?}"
        );

        let _ = std::fs::remove_file(&not_a_directory);
        let _ = std::fs::remove_file(&history_path);
    }
}
