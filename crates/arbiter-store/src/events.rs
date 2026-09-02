//! Appending events with a real hash chain, and verifying one. ARCHITECTURE §8.1,
//! §8.3, §9; INTERFACES §13.
//!
//! `content_hash = blake3(canonical payload)` — PLAN_DEVIATIONS.md D22 resolves
//! "payload" as the event's whole content (every field but the two hash fields
//! themselves and the DB-assigned `sequence`), not literally just the JSON
//! `payload` field, since a hash that only covered `payload` would not catch a
//! tampered `event_type` or `stage` with the payload bytes left untouched — and
//! "an edited row is detected" (ARCHITECTURE §9) is the whole point of having one.
//! `previous_event_hash` is simply the prior row's `content_hash`, carried
//! forward — not cryptographically mixed into this row's own hash — so
//! `content_hash` is a pure function of one event and can be computed before the
//! store round-trip assigns its `seq`.

use crate::sqlite_store::SqliteRunReader;
use arbiter_kernel::event::Event;
use arbiter_kernel::ids::Sequence;
use arbiter_kernel::store::{ChainStatus, RunWriter, StoreError};
use serde::Serialize;

/// The fields that make up an event's content, in a fixed, deterministic order —
/// deliberately not `Event` reused directly, so the hash's shape survives a
/// refactor to `Event`'s own field order, and deliberately raw strings for
/// `event_type`/`payload` rather than the typed `EventType`/`serde_json::Value`,
/// so a row whose `event_type` this binary cannot parse can still be hashed and
/// verified (INTERFACES §13's forward-compatibility promise — see
/// [`RawEventRow`]). `payload` is the exact `TEXT` as stored (`Event::payload`
/// serialized once via `.to_string()` at write time), not re-canonicalized on
/// read, so verification only ever needs to match write-time bytes.
#[derive(Serialize)]
struct HashableContent<'a> {
    schema_version: u32,
    run_id: &'a str,
    event_id: &'a str,
    timestamp: &'a str,
    stage: &'a str,
    event_type: &'a str,
    durable: bool,
    payload: &'a str,
}

/// `blake3:`-prefixed, matching ARCHITECTURE §16's convention for hashes in JSON
/// fields (the same convention `arbiter_core::OptionVersion` already uses).
fn hash_content(c: &HashableContent) -> Result<String, StoreError> {
    let canonical = serde_json::to_string(c).map_err(|err| StoreError::Other(err.to_string()))?;
    Ok(format!(
        "blake3:{}",
        blake3::hash(canonical.as_bytes()).to_hex()
    ))
}

/// The `event_type` and `payload` strings passed here must be **exactly** what
/// `Tx::append_event` writes into the `event_type`/`payload` columns
/// (`sqlite_store.rs` stores `serde_json::to_string(&e.event_type)` — the quoted
/// JSON string, not a bare/trimmed one — and `e.payload.to_string()`) so that
/// verification, which reads those same columns back raw, recomputes an identical
/// hash. Trimming or re-normalizing either string here and not at write time (or
/// vice versa) would make every row fail verification despite never having been
/// tampered with.
fn canonical_content_hash(e: &Event) -> Result<String, StoreError> {
    let event_type_json =
        serde_json::to_string(&e.event_type).map_err(|err| StoreError::Other(err.to_string()))?;
    let payload_json = e.payload.to_string();
    hash_content(&HashableContent {
        schema_version: e.schema_version,
        run_id: e.run_id.as_str(),
        event_id: e.event_id.as_str(),
        timestamp: &e.timestamp,
        stage: e.stage.as_str(),
        event_type: &event_type_json,
        durable: e.durable,
        payload: &payload_json,
    })
}

/// One `events` row, exactly as stored — `event_type` and `payload` as raw
/// `TEXT`, not parsed. `RunReader::events`' typed view drops a row whose
/// `event_type` this binary does not recognise; chain verification reads through
/// this type instead, precisely so it does not have that limitation.
#[derive(Debug, Clone)]
pub struct RawEventRow {
    /// SQLite's native rowid width; cast to `u64` (`Sequence`'s own
    /// representation) only where a `Sequence` is actually constructed.
    pub seq: i64,
    pub run_id: String,
    pub schema_version: u32,
    pub event_id: String,
    pub timestamp: String,
    pub stage: String,
    pub event_type: String,
    pub durable: bool,
    pub payload: String,
    pub content_hash: String,
    pub previous_event_hash: Option<String>,
}

impl RawEventRow {
    fn recompute_hash(&self) -> Result<String, StoreError> {
        hash_content(&HashableContent {
            schema_version: self.schema_version,
            run_id: &self.run_id,
            event_id: &self.event_id,
            timestamp: &self.timestamp,
            stage: &self.stage,
            event_type: &self.event_type,
            durable: self.durable,
            payload: &self.payload,
        })
    }
}

/// The in-memory tip of a run's hash chain. A fresh run starts `empty()`; a
/// resumed one is rebuilt from the store with [`ChainState::from_last_event`] so
/// the next append still chains correctly across a restart.
#[derive(Debug, Clone, Default)]
pub struct ChainState {
    last_hash: Option<String>,
}

impl ChainState {
    pub fn empty() -> Self {
        Self { last_hash: None }
    }

    pub fn from_last_event(last: Option<&Event>) -> Self {
        Self {
            last_hash: last.map(|e| e.content_hash.clone()),
        }
    }

    /// Computes `content_hash`, sets `previous_event_hash` to the chain's current
    /// tip, and advances the tip to the newly-sealed event's hash. `event`'s own
    /// `content_hash`/`previous_event_hash` are ignored on input — this is the
    /// only place either field is set.
    fn seal(&mut self, mut event: Event) -> Result<Event, StoreError> {
        event.previous_event_hash = self.last_hash.clone();
        event.content_hash = canonical_content_hash(&event)?;
        self.last_hash = Some(event.content_hash.clone());
        Ok(event)
    }
}

/// Seals and appends one event inside its own transaction. For appending many
/// events atomically together, seal each with [`ChainState::seal`] (via a loop
/// calling this function's body inline) inside one `writer.transact` — this
/// helper covers the common one-event-per-transaction case directly.
pub fn append_chained(
    writer: &mut dyn RunWriter,
    chain: &mut ChainState,
    event: Event,
) -> Result<Sequence, StoreError> {
    let sealed = chain.seal(event)?;
    let mut result = None;
    writer.transact(&mut |tx| {
        result = Some(tx.append_event(&sealed)?);
        Ok(())
    })?;
    Ok(result.expect("transact only returns Ok after the closure ran to completion"))
}

/// Appends every event in `events`, each sealed against the running chain tip, in
/// one transaction — all commit together or none do.
pub fn append_chained_batch(
    writer: &mut dyn RunWriter,
    chain: &mut ChainState,
    events: Vec<Event>,
) -> Result<Vec<Sequence>, StoreError> {
    let mut sealed = Vec::with_capacity(events.len());
    for e in events {
        sealed.push(chain.seal(e)?);
    }
    let mut sequences = Vec::with_capacity(sealed.len());
    writer.transact(&mut |tx| {
        sequences.clear();
        for e in &sealed {
            sequences.push(tx.append_event(e)?);
        }
        Ok(())
    })?;
    Ok(sequences)
}

/// `RunReader::verify_chain`'s real implementation (S2 shipped a stub). Reads
/// every row's raw columns directly — not through
/// [`arbiter_kernel::store::RunReader::events`], which silently skips rows whose
/// `event_type` this binary does not recognise (INTERFACES §13's forward-
/// compatibility promise: "readers skip unknown `event_type` values but still
/// include the line in the hash chain"). Chain integrity does not require
/// understanding a row's `event_type`, only its bytes.
pub fn verify_chain(reader: &SqliteRunReader) -> Result<ChainStatus, StoreError> {
    let rows = reader.raw_event_rows()?;

    let mut expected_previous: Option<String> = None;
    for row in rows {
        if row.previous_event_hash != expected_previous {
            return Ok(ChainStatus::Broken {
                at: Sequence::new(row.seq as u64),
            });
        }
        let recomputed = row.recompute_hash()?;
        if recomputed != row.content_hash {
            return Ok(ChainStatus::Broken {
                at: Sequence::new(row.seq as u64),
            });
        }
        expected_previous = Some(row.content_hash);
    }
    Ok(ChainStatus::Intact)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite_store::SqliteRunStore;
    use arbiter_core::{PolicyVersion, RunId};
    use arbiter_kernel::event::EventType;
    use arbiter_kernel::ids::{EventId, StageName};
    use arbiter_kernel::store::{Manifest, RunStore};

    fn manifest() -> Manifest {
        Manifest {
            policy_version: PolicyVersion::new("argument-v1"),
            config_hash: "blake3:cfg".to_string(),
            pack_hash: "blake3:pack".to_string(),
            correlation_table_version: "2026.1".to_string(),
            rng_seed: 1,
        }
    }

    fn temp_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "arbiter_events_test_{label}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn raw_event(run_id: &RunId, id: &str, n: u32) -> Event {
        Event {
            schema_version: 1,
            event_id: EventId::new(id),
            run_id: run_id.clone(),
            sequence: None,
            timestamp: crate::now_rfc3339(),
            stage: StageName::new("claims.extract"),
            event_type: EventType::ClaimExtracted,
            durable: false,
            payload: serde_json::json!({"n": n}),
            content_hash: String::new(),
            previous_event_hash: None,
        }
    }

    #[test]
    fn a_fresh_chain_of_two_events_verifies_intact() {
        let root = temp_root("fresh");
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");
        let mut writer = store.create(&run_id, &manifest()).unwrap();
        let mut chain = ChainState::empty();

        append_chained(&mut *writer, &mut chain, raw_event(&run_id, "evt_1", 1)).unwrap();
        append_chained(&mut *writer, &mut chain, raw_event(&run_id, "evt_2", 2)).unwrap();

        let reader = store.reader(&run_id).unwrap();
        let status = reader.verify_chain().unwrap();
        assert_eq!(status, ChainStatus::Intact);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn chain_verifies_over_10k_events() {
        let root = temp_root("10k");
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");
        let mut writer = store.create(&run_id, &manifest()).unwrap();
        let mut chain = ChainState::empty();

        let events: Vec<Event> = (0..10_000)
            .map(|i| raw_event(&run_id, &format!("evt_{i}"), i))
            .collect();
        append_chained_batch(&mut *writer, &mut chain, events).unwrap();

        let reader = store.reader(&run_id).unwrap();
        assert_eq!(reader.events().unwrap().count(), 10_000);
        assert_eq!(reader.verify_chain().unwrap(), ChainStatus::Intact);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_edited_row_is_detected_not_repaired() {
        let root = temp_root("edited");
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");
        let mut writer = store.create(&run_id, &manifest()).unwrap();
        let mut chain = ChainState::empty();
        append_chained(&mut *writer, &mut chain, raw_event(&run_id, "evt_1", 1)).unwrap();
        append_chained(&mut *writer, &mut chain, raw_event(&run_id, "evt_2", 2)).unwrap();
        append_chained(&mut *writer, &mut chain, raw_event(&run_id, "evt_3", 3)).unwrap();

        // Simulate "someone edited the database directly" -- change row 2's
        // payload without recomputing its content_hash.
        {
            let conn =
                rusqlite::Connection::open(root.join(run_id.as_str()).join("run.db")).unwrap();
            conn.execute(
                "UPDATE events SET payload = '{\"n\": 9999}' WHERE seq = 2",
                [],
            )
            .unwrap();
        }

        let reader = store.reader(&run_id).unwrap();
        let status = reader.verify_chain().unwrap();
        assert_eq!(
            status,
            ChainStatus::Broken {
                at: Sequence::new(2)
            }
        );

        // Never repaired: re-verifying reports the same break, not a silent fix.
        let status_again = reader.verify_chain().unwrap();
        assert_eq!(
            status_again,
            ChainStatus::Broken {
                at: Sequence::new(2)
            }
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_event_type_is_skipped_but_still_chained() {
        let root = temp_root("unknown_type");
        let store = SqliteRunStore::new(&root);
        let run_id = RunId::new("run_1");
        let mut writer = store.create(&run_id, &manifest()).unwrap();
        let mut chain = ChainState::empty();
        append_chained(&mut *writer, &mut chain, raw_event(&run_id, "evt_1", 1)).unwrap();

        // A future binary's event_type this one doesn't know, inserted with a
        // correctly-chained hash computed by hand over the *actual* raw string
        // that will land in the column -- `ChainState::seal` can't produce this
        // directly, since it only ever writes `EventType` values this binary
        // compiles, but the hash function it calls (`hash_content`) is generic
        // over raw strings and is exactly what a future binary would also call.
        let future_run_id = run_id.as_str().to_string();
        let future_timestamp = crate::now_rfc3339();
        let future_stage = "future.stage".to_string();
        let future_event_type_json = "\"A_FUTURE_EVENT_TYPE\"".to_string(); // matches
        // serde's SCREAMING_SNAKE_CASE rename convention a future EventType would use
        let future_payload_json = serde_json::json!({"n": 2}).to_string();
        let previous_hash = chain.last_hash.clone();
        let future_hash = hash_content(&HashableContent {
            schema_version: 1,
            run_id: &future_run_id,
            event_id: "evt_2",
            timestamp: &future_timestamp,
            stage: &future_stage,
            event_type: &future_event_type_json,
            durable: false,
            payload: &future_payload_json,
        })
        .unwrap();
        {
            let conn =
                rusqlite::Connection::open(root.join(run_id.as_str()).join("run.db")).unwrap();
            conn.execute(
                "INSERT INTO events (seq, run_id, schema_version, event_id, timestamp, stage, event_type, durable, payload, content_hash, previous_event_hash)
                 VALUES (NULL, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    future_run_id,
                    1,
                    "evt_2",
                    future_timestamp,
                    future_stage,
                    future_event_type_json,
                    false,
                    future_payload_json,
                    future_hash,
                    previous_hash,
                ],
            )
            .unwrap();
        }

        let reader = store.reader(&run_id).unwrap();
        // events() skips the row it cannot parse -- only evt_1 is visible.
        let visible: Vec<Event> = reader.events().unwrap().collect();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].event_id.as_str(), "evt_1");

        // ...but the chain still accounts for it: verify_chain must see both rows
        // and confirm the (correctly-hashed) unknown-type row is intact, not
        // absent or broken.
        assert_eq!(reader.verify_chain().unwrap(), ChainStatus::Intact);

        let _ = std::fs::remove_dir_all(&root);
    }
}
