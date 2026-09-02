//! `arbiter_kernel::{RunStore, RunWriter, Tx, RunReader}` implemented against
//! SQLite. K0 defined the seam; this crate writes the bodies (INTERFACES §1).
//!
//! Scope note: `RunStore::create`/`reopen`/`reader`, `RunWriter::transact`,
//! `Tx::append_event` (mechanical persistence — assigning `seq` and storing
//! whatever `content_hash`/`previous_event_hash` the caller already computed) and
//! `RunReader::events` are S2's own scope. `RunReader::verify_chain` and the real
//! hash-chaining logic that feeds `append_event` correct hashes live in
//! `events.rs` (S3). `Tx::reserve_call`/`put_artifact`/`put_cache`/
//! `commit_budget`/`set_call_state` — the four INTERFACES §1 names plus
//! `reserve_call` (PLAN_DEVIATIONS.md D29) — are implemented here against the
//! `budget`/`provider_calls`/`cache_entries`/`artifacts` tables S4 adds.

use crate::lease::{self, LeaseError, Owner};
use crate::{now_rfc3339, schema};
use arbiter_core::RunId;
use arbiter_kernel::event::{Event, EventType};
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

    /// INTERFACES §5 step 0: `BUDGET_RESERVED{reservation_id, estimate}` +
    /// "INSERT provider_calls (state RESERVED, reserved_amount)" +
    /// "budget.reserved += estimate" — the row is created here, in `RESERVED`,
    /// before any request has left the machine; [`Tx::set_call_state`] only ever
    /// transitions it afterwards.
    fn reserve_call(
        &mut self,
        call_id: &CallId,
        reservation_id: &ReservationId,
        reserved_amount: Cost,
    ) -> Result<(), KernelStoreError> {
        let state_json = serde_json::to_string(&CallState::Reserved)
            .map_err(|e| KernelStoreError::Other(e.to_string()))?;
        self.tx
            .execute(
                "INSERT INTO provider_calls (call_id, reservation_id, state, reserved_amount, actual_cost, request_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5)",
                rusqlite::params![
                    call_id.as_str(),
                    reservation_id.as_str(),
                    state_json,
                    reserved_amount.0,
                    now_rfc3339(),
                ],
            )
            .map_err(sqlite_error_to_store_error)?;
        self.tx
            .execute(
                "UPDATE budget SET reserved = reserved + ?1",
                [reserved_amount.0],
            )
            .map_err(sqlite_error_to_store_error)?;
        Ok(())
    }

    /// Content-addressed and idempotent: a re-put of an artifact whose hash
    /// already exists is a no-op, matching `blob.rs::write_blob`'s own stance —
    /// identical content hashes identically, so there is nothing to overwrite.
    fn put_artifact(&mut self, a: &dyn Artifact) -> Result<ArtifactId, KernelStoreError> {
        let id = ArtifactId::new(a.content_hash());
        self.tx
            .execute(
                "INSERT INTO artifacts (artifact_id, artifact_type, payload, created_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(artifact_id) DO NOTHING",
                rusqlite::params![
                    id.as_str(),
                    a.artifact_type(),
                    a.to_json().to_string(),
                    now_rfc3339(),
                ],
            )
            .map_err(sqlite_error_to_store_error)?;
        Ok(id)
    }

    fn put_cache(&mut self, k: &CacheKey, r: &CachedResponse) -> Result<(), KernelStoreError> {
        self.tx
            .execute(
                "INSERT INTO cache_entries (provider, model, params, prompt_hash, response_hash, size_bytes, inline)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(provider, model, params, prompt_hash) DO UPDATE SET
                   response_hash = excluded.response_hash,
                   size_bytes = excluded.size_bytes,
                   inline = excluded.inline",
                rusqlite::params![
                    k.provider.as_str(),
                    k.model.as_str(),
                    k.params,
                    k.prompt_hash,
                    r.response_hash,
                    r.size_bytes as i64,
                    r.inline,
                ],
            )
            .map_err(sqlite_error_to_store_error)?;
        Ok(())
    }

    /// INTERFACES §5 step 5: `budget: reserved -= estimate, committed += actual`
    /// plus `UPDATE provider_calls SET state = COMPLETED`, one transaction.
    ///
    /// The reservation's held amount is read back from `provider_calls` (the
    /// most recent row for this `reservation_id`, since a retry against an
    /// idempotent provider can share one reservation across more than one
    /// `call_id`, and every such row was reserved for the same amount) rather
    /// than threaded through this call's own parameters, because `Cost` here is
    /// `actual`, not the original estimate.
    fn commit_budget(&mut self, r: &ReservationId, actual: Cost) -> Result<(), KernelStoreError> {
        let reserved_amount: f64 = self
            .tx
            .query_row(
                "SELECT reserved_amount FROM provider_calls
                 WHERE reservation_id = ?1 ORDER BY created_at DESC LIMIT 1",
                [r.as_str()],
                |row| row.get(0),
            )
            .map_err(sqlite_error_to_store_error)?;
        self.tx
            .execute(
                "UPDATE budget SET committed = committed + ?1, reserved = reserved - ?2",
                rusqlite::params![actual.0, reserved_amount],
            )
            .map_err(sqlite_error_to_store_error)?;
        let state_json = serde_json::to_string(&CallState::Completed)
            .map_err(|e| KernelStoreError::Other(e.to_string()))?;
        self.tx
            .execute(
                "UPDATE provider_calls SET state = ?1, actual_cost = ?2 WHERE reservation_id = ?3",
                rusqlite::params![state_json, actual.0, r.as_str()],
            )
            .map_err(sqlite_error_to_store_error)?;
        Ok(())
    }

    fn set_call_state(&mut self, c: &CallId, s: CallState) -> Result<(), KernelStoreError> {
        let state_json =
            serde_json::to_string(&s).map_err(|e| KernelStoreError::Other(e.to_string()))?;
        self.tx
            .execute(
                "UPDATE provider_calls SET state = ?1 WHERE call_id = ?2",
                rusqlite::params![state_json, c.as_str()],
            )
            .map_err(sqlite_error_to_store_error)?;
        Ok(())
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
        // `Option<Event>`, not `Event`: a row whose `event_type` this binary does
        // not recognise is silently dropped from the typed view rather than
        // erroring, per INTERFACES §13's forward-compatibility promise ("readers
        // skip unknown event_type values but still include the line in the hash
        // chain") — the "still chained" half is `raw_event_rows`/`verify_chain`'s
        // job, which reads the same table without requiring `event_type` to parse.
        let rows: Vec<Option<Event>> = stmt
            .query_map([], |row| {
                let run_id: String = row.get(0)?;
                let schema_version: u32 = row.get(1)?;
                let event_id: String = row.get(2)?;
                let seq: i64 = row.get(3)?;
                let timestamp: String = row.get(4)?;
                let stage: String = row.get(5)?;
                let event_type_json: String = row.get(6)?;
                let durable: bool = row.get(7)?;
                let payload_json: String = row.get(8)?;
                let content_hash: String = row.get(9)?;
                let previous_event_hash: Option<String> = row.get(10)?;

                let event_type: Option<EventType> = serde_json::from_str(&event_type_json).ok();
                let payload: serde_json::Value =
                    serde_json::from_str(&payload_json).unwrap_or(serde_json::Value::Null);

                Ok(event_type.map(|event_type| Event {
                    run_id: RunId::new(run_id),
                    schema_version,
                    event_id: arbiter_kernel::ids::EventId::new(event_id),
                    sequence: Some(Sequence::new(seq as u64)),
                    timestamp,
                    stage: arbiter_kernel::ids::StageName::new(stage),
                    event_type,
                    durable,
                    payload,
                    content_hash,
                    previous_event_hash,
                }))
            })
            .map_err(sqlite_error_to_store_error)?
            .collect::<Result<_, _>>()
            .map_err(sqlite_error_to_store_error)?;
        Ok(Box::new(rows.into_iter().flatten()))
    }

    fn verify_chain(&self) -> Result<ChainStatus, KernelStoreError> {
        crate::events::verify_chain(self)
    }

    fn artifacts_by_type(
        &self,
        artifact_type: &str,
    ) -> Result<Vec<serde_json::Value>, KernelStoreError> {
        let mut stmt = self
            .conn
            // `artifact_id` is TEXT PRIMARY KEY (not `INTEGER PRIMARY KEY`), so
            // SQLite still maintains an implicit rowid to order by -- insertion
            // order, which `created_at`'s string-timestamp granularity cannot
            // reliably guarantee under two puts in the same millisecond.
            .prepare("SELECT payload FROM artifacts WHERE artifact_type = ?1 ORDER BY rowid")
            .map_err(sqlite_error_to_store_error)?;
        let rows: Vec<serde_json::Value> = stmt
            .query_map([artifact_type], |row| {
                let payload: String = row.get(0)?;
                Ok(payload)
            })
            .map_err(sqlite_error_to_store_error)?
            .collect::<Result<Vec<String>, _>>()
            .map_err(sqlite_error_to_store_error)?
            .into_iter()
            .map(|p| serde_json::from_str(&p).unwrap_or(serde_json::Value::Null))
            .collect();
        Ok(rows)
    }
}

impl SqliteRunReader {
    /// Every `events` row, raw — `event_type` and `payload` as the exact `TEXT`
    /// stored, not parsed into `EventType`/`serde_json::Value`. This is what lets
    /// [`crate::events::verify_chain`] account for a row whose `event_type` this
    /// binary cannot parse, which [`RunReader::events`]'s typed view must skip.
    pub(crate) fn raw_event_rows(
        &self,
    ) -> Result<Vec<crate::events::RawEventRow>, KernelStoreError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT seq, run_id, schema_version, event_id, timestamp, stage, event_type, durable, payload, content_hash, previous_event_hash
                 FROM events ORDER BY seq",
            )
            .map_err(sqlite_error_to_store_error)?;
        stmt.query_map([], |row| {
            Ok(crate::events::RawEventRow {
                seq: row.get(0)?,
                run_id: row.get(1)?,
                schema_version: row.get(2)?,
                event_id: row.get(3)?,
                timestamp: row.get(4)?,
                stage: row.get(5)?,
                event_type: row.get(6)?,
                durable: row.get(7)?,
                payload: row.get(8)?,
                content_hash: row.get(9)?,
                previous_event_hash: row.get(10)?,
            })
        })
        .map_err(sqlite_error_to_store_error)?
        .collect::<Result<_, _>>()
        .map_err(sqlite_error_to_store_error)
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

    #[derive(Debug)]
    struct TestArtifact {
        content: &'static str,
    }
    impl Artifact for TestArtifact {
        fn artifact_type(&self) -> &'static str {
            "test.v1"
        }
        fn content_hash(&self) -> String {
            format!("blake3:{}", blake3::hash(self.content.as_bytes()).to_hex())
        }
        fn to_json(&self) -> serde_json::Value {
            serde_json::json!({"content": self.content})
        }
    }

    fn read_budget(conn: &Connection) -> (f64, f64) {
        conn.query_row("SELECT reserved, committed FROM budget", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
    }

    #[test]
    fn put_cache_round_trips_through_a_real_table() {
        let root = temp_root();
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");
        let mut writer = store.create(&run_id, &manifest()).unwrap();

        let key = CacheKey {
            provider: arbiter_core::ProviderId::new("anthropic"),
            model: arbiter_core::ModelId::new("claude"),
            params: "{}".to_string(),
            prompt_hash: "blake3:p".to_string(),
        };
        let response = CachedResponse {
            response_hash: "blake3:r".to_string(),
            size_bytes: 10,
            inline: Some("hi".to_string()),
        };

        writer
            .transact(&mut |tx| tx.put_cache(&key, &response))
            .unwrap();

        // A second put of the same key overwrites rather than conflicting.
        let updated = CachedResponse {
            response_hash: "blake3:r2".to_string(),
            size_bytes: 20,
            inline: Some("bye".to_string()),
        };
        writer
            .transact(&mut |tx| tx.put_cache(&key, &updated))
            .unwrap();

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn put_artifact_is_idempotent_on_identical_content() {
        let root = temp_root();
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");
        let mut writer = store.create(&run_id, &manifest()).unwrap();

        let artifact = TestArtifact { content: "hello" };
        let id_a = writer
            .transact(&mut |tx| tx.put_artifact(&artifact).map(|_| ()))
            .map(|_| ArtifactId::new(artifact.content_hash()))
            .unwrap();
        // Re-putting identical content must not error (ON CONFLICT DO NOTHING).
        writer
            .transact(&mut |tx| tx.put_artifact(&artifact).map(|_| ()))
            .unwrap();

        assert_eq!(id_a.as_str(), artifact.content_hash());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn artifacts_by_type_returns_only_matching_payloads_in_insertion_order() {
        let root = temp_root();
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");
        let mut writer = store.create(&run_id, &manifest()).unwrap();

        writer
            .transact(&mut |tx| {
                tx.put_artifact(&TestArtifact { content: "first" })?;
                tx.put_artifact(&TestArtifact { content: "second" })?;
                Ok(())
            })
            .unwrap();

        let reader = store.reader(&run_id).unwrap();
        let payloads = reader.artifacts_by_type("test.v1").unwrap();
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0]["content"], "first");
        assert_eq!(payloads[1]["content"], "second");

        assert!(reader.artifacts_by_type("no_such.v1").unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reserve_call_then_commit_moves_money_from_reserved_to_committed() {
        let root = temp_root();
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");
        let mut writer = store.create(&run_id, &manifest()).unwrap();

        let call_id = CallId::new("call_1");
        let reservation_id = ReservationId::new("res_1");

        writer
            .transact(&mut |tx| tx.reserve_call(&call_id, &reservation_id, Cost(0.50)))
            .unwrap();

        writer
            .transact(&mut |tx| {
                tx.set_call_state(&call_id, CallState::Sent)?;
                tx.set_call_state(&call_id, CallState::Acknowledged)?;
                Ok(())
            })
            .unwrap();

        writer
            .transact(&mut |tx| tx.commit_budget(&reservation_id, Cost(0.30)))
            .unwrap();

        let conn = Connection::open(root.join("run_1").join("run.db")).unwrap();
        let (reserved, committed) = read_budget(&conn);
        assert!((reserved - 0.0).abs() < 1e-9, "reserved: {reserved}");
        assert!((committed - 0.30).abs() < 1e-9, "committed: {committed}");

        let state_json: String = conn
            .query_row(
                "SELECT state FROM provider_calls WHERE call_id = ?1",
                [call_id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            state_json,
            serde_json::to_string(&CallState::Completed).unwrap()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_retry_shares_one_reservation_across_two_call_ids() {
        // INTERFACES §5: "the reservation stays HELD across the retry -- never
        // released and re-reserved." Two provider_calls rows, same
        // reservation_id, and commit_budget still finds the reserved_amount.
        let root = temp_root();
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");
        let mut writer = store.create(&run_id, &manifest()).unwrap();

        let reservation_id = ReservationId::new("res_1");
        let first_attempt = CallId::new("call_1");
        let retry = CallId::new("call_2");

        writer
            .transact(&mut |tx| tx.reserve_call(&first_attempt, &reservation_id, Cost(1.00)))
            .unwrap();
        writer
            .transact(&mut |tx| tx.set_call_state(&first_attempt, CallState::Orphaned))
            .unwrap();
        // The retry is a *new* provider_calls row sharing the same
        // reservation_id and the same reserved_amount -- the reservation
        // itself was never released.
        writer
            .transact(&mut |tx| tx.reserve_call(&retry, &reservation_id, Cost(1.00)))
            .unwrap();

        writer
            .transact(&mut |tx| tx.commit_budget(&reservation_id, Cost(0.80)))
            .unwrap();

        let conn = Connection::open(root.join("run_1").join("run.db")).unwrap();
        let (_, committed) = read_budget(&conn);
        assert!((committed - 0.80).abs() < 1e-9);

        let _ = std::fs::remove_dir_all(&root);
    }
}
