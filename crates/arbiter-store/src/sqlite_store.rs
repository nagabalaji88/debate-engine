//! `arbiter_kernel::{RunStore, RunWriter, Tx, RunReader}` implemented against
//! SQLite. K0 defined the seam; this crate writes the bodies (INTERFACES §1).
//!
//! Scope note: this task (S2) delivers `RunStore::create`/`reopen`/`reader`,
//! `RunWriter::transact`, `Tx::append_event` (mechanical persistence — assigning
//! `seq` and storing whatever `content_hash`/`previous_event_hash` the caller
//! already computed) and `RunReader::events`. Four `Tx` methods
//! (`put_artifact`/`put_cache`/`commit_budget`/`set_call_state`) need tables S1
//! deliberately did not create (PLAN_DEVIATIONS.md D21: `artifacts`,
//! `cache_entries`, `budget`, `provider_calls` wait for K1/K2/K5), and
//! `RunReader::verify_chain` needs the hash-recomputation logic that is
//! explicitly S3's stated scope ("`events.rs`... `verify_chain` recomputes and
//! reports"). All five return `StoreError::Other` naming the task that lands
//! them, rather than a silently-wrong implementation.

use crate::lease::{self, LeaseError, Owner};
use crate::{now_rfc3339, schema};
use arbiter_core::RunId;
use arbiter_kernel::event::Event;
use arbiter_kernel::ids::{ArtifactId, CallId, ReservationId, Sequence};
use arbiter_kernel::provider::CallState;
use arbiter_kernel::store::{
    Artifact, CacheKey, CachedResponse, ChainStatus, Cost, Manifest, RunReader, RunStore,
    RunWriter, StoreError as KernelStoreError, Tx,
};
use rusqlite::Connection;
use std::path::PathBuf;

fn lease_error_to_store_error(e: LeaseError) -> KernelStoreError {
    match e {
        LeaseError::AlreadyOpen => KernelStoreError::AlreadyOpen,
        LeaseError::NotFound => KernelStoreError::Other("run not found".to_string()),
        LeaseError::Sqlite(e) => KernelStoreError::Other(e.to_string()),
    }
}

fn sqlite_error_to_store_error(e: rusqlite::Error) -> KernelStoreError {
    KernelStoreError::Other(e.to_string())
}

fn schema_error_to_store_error(e: schema::SchemaError) -> KernelStoreError {
    KernelStoreError::Other(e.to_string())
}

/// `runs/<id>/run.db`, per INTERFACES §1's concurrency-model path table. `root`
/// is the directory that contains every run's subdirectory (e.g. `.arbiter/runs`
/// for a project-local store).
#[derive(Debug, Clone)]
pub struct SqliteRunStore {
    root: PathBuf,
}

impl SqliteRunStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn run_db_path(&self, run_id: &RunId) -> PathBuf {
        self.root.join(run_id.as_str()).join("run.db")
    }

    fn open_and_init(&self, run_id: &RunId) -> Result<Connection, KernelStoreError> {
        let path = self.run_db_path(run_id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| KernelStoreError::Other(e.to_string()))?;
        }
        let conn = Connection::open(&path).map_err(sqlite_error_to_store_error)?;
        schema::open_run_db(&conn, env!("CARGO_PKG_VERSION"), &now_rfc3339())
            .map_err(schema_error_to_store_error)?;
        Ok(conn)
    }
}

impl RunStore for SqliteRunStore {
    fn create(
        &self,
        run_id: &RunId,
        _manifest: &Manifest,
    ) -> Result<Box<dyn RunWriter>, KernelStoreError> {
        let conn = self.open_and_init(run_id)?;
        let owner = Owner::current();
        lease::create(&conn, run_id.as_str(), &owner).map_err(lease_error_to_store_error)?;
        Ok(Box::new(SqliteRunWriter { conn }))
    }

    fn reopen(&self, run_id: &RunId) -> Result<Box<dyn RunWriter>, KernelStoreError> {
        let conn = self.open_and_init(run_id)?;
        let owner = Owner::current();
        lease::reopen(&conn, run_id.as_str(), &owner).map_err(lease_error_to_store_error)?;
        Ok(Box::new(SqliteRunWriter { conn }))
    }

    fn reader(&self, run_id: &RunId) -> Result<Box<dyn RunReader>, KernelStoreError> {
        let conn = self.open_and_init(run_id)?;
        Ok(Box::new(SqliteRunReader { conn }))
    }
}

#[derive(Debug)]
pub struct SqliteRunWriter {
    conn: Connection,
}

impl RunWriter for SqliteRunWriter {
    fn transact(
        &mut self,
        f: &mut dyn FnMut(&mut dyn Tx) -> Result<(), KernelStoreError>,
    ) -> Result<(), KernelStoreError> {
        let tx = self
            .conn
            .transaction()
            .map_err(sqlite_error_to_store_error)?;
        let mut wrapped = SqliteTx { tx };
        let result = f(&mut wrapped);
        match result {
            Ok(()) => wrapped.tx.commit().map_err(sqlite_error_to_store_error),
            Err(e) => {
                // rusqlite::Transaction rolls back on drop when not committed;
                // an explicit rollback surfaces its own error instead of a
                // silent drop-time failure.
                wrapped.tx.rollback().map_err(sqlite_error_to_store_error)?;
                Err(e)
            }
        }
    }
}

#[derive(Debug)]
struct SqliteTx<'conn> {
    tx: rusqlite::Transaction<'conn>,
}

const NOT_YET_IMPLEMENTED_BUDGET: &str = "provider_calls/budget tables land in K1 (budget ledger) / K2 (call state machine) — PLAN_DEVIATIONS.md D21";
const NOT_YET_IMPLEMENTED_CACHE: &str =
    "cache_entries table lands in K5 (response cache) — PLAN_DEVIATIONS.md D21";
const NOT_YET_IMPLEMENTED_ARTIFACTS: &str =
    "artifacts table lands in S4 (projections) — PLAN_DEVIATIONS.md D21";
const NOT_YET_IMPLEMENTED_CHAIN: &str = "verify_chain's hash-recomputation logic is S3's scope";

impl Tx for SqliteTx<'_> {
    fn append_event(&mut self, e: &Event) -> Result<Sequence, KernelStoreError> {
        self.tx
            .execute(
                "INSERT INTO events (seq, run_id, schema_version, event_id, timestamp, stage, event_type, durable, payload, content_hash, previous_event_hash)
                 VALUES (NULL, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    e.run_id.as_str(),
                    e.schema_version,
                    e.event_id.as_str(),
                    e.timestamp,
                    e.stage.as_str(),
                    serde_json::to_string(&e.event_type).map_err(|err| KernelStoreError::Other(err.to_string()))?,
                    e.durable,
                    e.payload.to_string(),
                    e.content_hash,
                    e.previous_event_hash,
                ],
            )
            .map_err(sqlite_error_to_store_error)?;
        Ok(Sequence::new(self.tx.last_insert_rowid() as u64))
    }

    fn put_artifact(&mut self, _a: &dyn Artifact) -> Result<ArtifactId, KernelStoreError> {
        Err(KernelStoreError::Other(
            NOT_YET_IMPLEMENTED_ARTIFACTS.to_string(),
        ))
    }

    fn put_cache(&mut self, _k: &CacheKey, _r: &CachedResponse) -> Result<(), KernelStoreError> {
        Err(KernelStoreError::Other(
            NOT_YET_IMPLEMENTED_CACHE.to_string(),
        ))
    }

    fn commit_budget(&mut self, _r: &ReservationId, _actual: Cost) -> Result<(), KernelStoreError> {
        Err(KernelStoreError::Other(
            NOT_YET_IMPLEMENTED_BUDGET.to_string(),
        ))
    }

    fn set_call_state(&mut self, _c: &CallId, _s: CallState) -> Result<(), KernelStoreError> {
        Err(KernelStoreError::Other(
            NOT_YET_IMPLEMENTED_BUDGET.to_string(),
        ))
    }
}

#[derive(Debug)]
pub struct SqliteRunReader {
    conn: Connection,
}

impl RunReader for SqliteRunReader {
    fn events(&self) -> Result<Box<dyn Iterator<Item = Event>>, KernelStoreError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT run_id, schema_version, event_id, seq, timestamp, stage, event_type, durable, payload, content_hash, previous_event_hash
                 FROM events ORDER BY seq",
            )
            .map_err(sqlite_error_to_store_error)?;
        let rows: Vec<Event> = stmt
            .query_map([], |row| {
                let event_type_json: String = row.get(6)?;
                let payload_json: String = row.get(8)?;
                Ok(Event {
                    run_id: RunId::new(row.get::<_, String>(0)?),
                    schema_version: row.get(1)?,
                    event_id: arbiter_kernel::ids::EventId::new(row.get::<_, String>(2)?),
                    sequence: Some(Sequence::new(row.get::<_, i64>(3)? as u64)),
                    timestamp: row.get(4)?,
                    stage: arbiter_kernel::ids::StageName::new(row.get::<_, String>(5)?),
                    event_type: serde_json::from_str(&event_type_json)
                        .expect("event_type stored by append_event is always valid JSON"),
                    durable: row.get(7)?,
                    payload: serde_json::from_str(&payload_json)
                        .expect("payload stored by append_event is always valid JSON"),
                    content_hash: row.get(9)?,
                    previous_event_hash: row.get(10)?,
                })
            })
            .map_err(sqlite_error_to_store_error)?
            .collect::<Result<_, _>>()
            .map_err(sqlite_error_to_store_error)?;
        Ok(Box::new(rows.into_iter()))
    }

    fn verify_chain(&self) -> Result<ChainStatus, KernelStoreError> {
        Err(KernelStoreError::Other(
            NOT_YET_IMPLEMENTED_CHAIN.to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arbiter_kernel::event::EventType;
    use arbiter_kernel::ids::{EventId, StageName};

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "arbiter_sqlite_store_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn manifest() -> Manifest {
        Manifest {
            policy_version: arbiter_core::PolicyVersion::new("argument-v1"),
            config_hash: "blake3:cfg".to_string(),
            pack_hash: "blake3:pack".to_string(),
            correlation_table_version: "2026.1".to_string(),
            rng_seed: 42,
        }
    }

    fn sample_event(run_id: &RunId, event_id: &str) -> Event {
        Event {
            schema_version: 1,
            event_id: EventId::new(event_id),
            run_id: run_id.clone(),
            sequence: None,
            timestamp: now_rfc3339(),
            stage: StageName::new("claims.extract"),
            event_type: EventType::ClaimExtracted,
            durable: false,
            payload: serde_json::json!({"n": 1}),
            content_hash: "blake3:placeholder".to_string(),
            previous_event_hash: None,
        }
    }

    #[test]
    fn create_then_reader_sees_events_written_through_transact() {
        let root = temp_root();
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");

        let mut writer = store.create(&run_id, &manifest()).unwrap();
        writer
            .transact(&mut |tx| {
                tx.append_event(&sample_event(&run_id, "evt_1"))?;
                tx.append_event(&sample_event(&run_id, "evt_2"))?;
                Ok(())
            })
            .unwrap();

        let reader = store.reader(&run_id).unwrap();
        let events: Vec<Event> = reader.events().unwrap().collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_id.as_str(), "evt_1");
        assert_eq!(events[1].event_id.as_str(), "evt_2");
        assert_eq!(events[0].sequence, Some(Sequence::new(1)));
        assert_eq!(events[1].sequence, Some(Sequence::new(2)));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_create_on_the_same_run_is_already_open() {
        let root = temp_root();
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");

        store.create(&run_id, &manifest()).unwrap();
        let result = store.create(&run_id, &manifest());
        assert!(matches!(result, Err(KernelStoreError::AlreadyOpen)));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_transaction_writes_nothing() {
        let root = temp_root();
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");
        let mut writer = store.create(&run_id, &manifest()).unwrap();

        let result = writer.transact(&mut |tx| {
            tx.append_event(&sample_event(&run_id, "evt_1"))?;
            Err(KernelStoreError::Other("simulated failure".to_string()))
        });
        assert!(result.is_err());

        let reader = store.reader(&run_id).unwrap();
        let events: Vec<Event> = reader.events().unwrap().collect();
        assert!(
            events.is_empty(),
            "a rolled-back transaction must leave no trace: got {events:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn budget_and_cache_methods_report_not_yet_implemented_rather_than_panic() {
        let root = temp_root();
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");
        let mut writer = store.create(&run_id, &manifest()).unwrap();

        writer
            .transact(&mut |tx| {
                let cache_result = tx.put_cache(
                    &CacheKey {
                        provider: arbiter_core::ProviderId::new("anthropic"),
                        model: arbiter_core::ModelId::new("claude"),
                        params: "{}".to_string(),
                        prompt_hash: "blake3:p".to_string(),
                    },
                    &CachedResponse {
                        response_hash: "blake3:r".to_string(),
                        size_bytes: 10,
                        inline: Some("hi".to_string()),
                    },
                );
                assert!(matches!(cache_result, Err(KernelStoreError::Other(_))));
                Ok(())
            })
            .unwrap();

        let _ = std::fs::remove_dir_all(&root);
    }
}
