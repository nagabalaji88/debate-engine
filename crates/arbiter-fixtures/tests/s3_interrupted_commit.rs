//! F2 — `interrupted_commit` (S3), ARCHITECTURE §18's CI suite: "process
//! killed mid-transaction → the partial write is not there on reopen."

use arbiter_core::{PolicyVersion, RunId};
use arbiter_kernel::event::{Event, EventType};
use arbiter_kernel::ids::{EventId, StageName};
use arbiter_kernel::store::{Manifest, RunStore, StoreError, Tx};
use arbiter_store::sqlite_store::SqliteRunStore;

fn temp_root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "arbiter_fixtures_interrupted_commit_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn manifest() -> Manifest {
    Manifest {
        policy_version: PolicyVersion::new("argument-v1"),
        config_hash: "blake3:config".to_string(),
        pack_hash: "blake3:pack".to_string(),
        correlation_table_version: "v1".to_string(),
        rng_seed: 1,
    }
}

fn event(id: &str, run_id: &RunId, event_type: EventType) -> Event {
    Event {
        schema_version: 1,
        event_id: EventId::new(id),
        run_id: run_id.clone(),
        sequence: None,
        timestamp: "2026-01-01T00:00:00Z".to_string(),
        stage: StageName::new("test"),
        event_type,
        durable: true,
        payload: serde_json::json!({}),
        content_hash: "blake3:x".to_string(),
        previous_event_hash: None,
    }
}

/// `SqliteRunWriter::transact` rolls the SQLite transaction back whenever
/// the caller's closure returns `Err` (`sqlite_store.rs`'s own documented
/// behavior) -- the same guarantee that protects a real process kill mid-
/// transaction, since neither path ever reaches `COMMIT`. One event commits
/// cleanly first; a second is appended inside a transaction whose closure
/// then returns `Err`, standing in for the process dying before this
/// transaction's own commit. Reopening (a fresh reader against the same
/// database file) must find the first event and only the first: the
/// second's partial write must not be there.
#[test]
fn interrupted_commit() {
    let dir = temp_root();
    let store = SqliteRunStore::new(dir.clone());
    let run_id = RunId::new("run_interrupted");

    let mut writer = store.create(&run_id, &manifest()).unwrap();

    writer
        .transact(&mut |tx: &mut dyn Tx| {
            tx.append_event(&event("evt_1", &run_id, EventType::RunStarted))?;
            Ok(())
        })
        .expect("the first transaction must commit cleanly");

    let result = writer.transact(&mut |tx: &mut dyn Tx| {
        tx.append_event(&event(
            "evt_2_never_committed",
            &run_id,
            EventType::StageStarted,
        ))?;
        Err(StoreError::Other(
            "simulated crash mid-transaction".to_string(),
        ))
    });
    assert!(
        result.is_err(),
        "the interrupted transaction must surface as an error, not silently succeed"
    );

    drop(writer);

    let reader = store.reader(&run_id).unwrap();
    let ids: Vec<String> = reader
        .events()
        .unwrap()
        .map(|e| e.event_id.as_str().to_string())
        .collect();

    assert_eq!(
        ids,
        vec!["evt_1".to_string()],
        "only the cleanly-committed event survives; the interrupted write is gone"
    );

    drop(reader);
    let _ = std::fs::remove_dir_all(&dir);
}
