//! The `Store` trait seam, INTERFACES §1: "SQLite owns exclusion; the traits
//! never mention a file." This crate defines the seam; `arbiter-store` implements
//! it (PLAN_DEVIATIONS.md D1). No SQLite, no filesystem, no implementation here —
//! K0's whole scope is these signatures compiling with `arbiter-core` as the only
//! internal dependency.
//!
//! Two places where copying INTERFACES §1 "verbatim" is impossible without
//! breaking the trait's own stated use, resolved and logged as
//! PLAN_DEVIATIONS.md D19/D20:
//!
//! - `Artifact` is used as both a concrete type (`&Artifact` in `Tx::put_artifact`)
//!   and a trait bound (`type In: Artifact` in `Stage`, INTERFACES §6) — the two
//!   readings contradict unless `Artifact` is a trait and `&Artifact` really means
//!   `&dyn Artifact`. Resolved as a trait (D19).
//! - `RunWriter::transact<T>` is generic, but `RunStore::create`/`reopen` return
//!   `Box<dyn RunWriter>` — a trait with a generic method cannot be made into a
//!   trait object at all in Rust, so the spec's own signature cannot compile as
//!   written. Resolved by dropping the generic return and having callers extract
//!   results by capturing them inside the closure instead (D20).

use crate::event::Event;
use crate::ids::{ArtifactId, CallId, ReservationId, Sequence};
use crate::provider::CallState;
use arbiter_core::{PolicyVersion, RunId};
use serde::{Deserialize, Serialize};

/// Only `AlreadyOpen` is spec-named (INTERFACES §1: "`create` and `reopen` fail
/// with `AlreadyOpen` rather than blocking"). Everything else a real
/// implementation needs — a missing run, a SQLite error, a chain break surfaced
/// through a fallible read — is deliberately left to `Other`, a variant this K0
/// task can name honestly without inventing the taxonomy a real store's failure
/// modes actually need; `arbiter-store`'s own task (S2+) is where that's earned.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("run already open")]
    AlreadyOpen,
    #[error("{0}")]
    Other(String),
}

/// `RunReader::verify_chain`'s result (INTERFACES §1). Never given its own enum
/// in either spec file — inferred from "a chain break... is not repairable... the
/// event records detection and the run is marked unverifiable" (ARCHITECTURE §9)
/// and the `ChainBreakDetected` event variant (INTERFACES §13): at minimum an
/// intact state and a broken state naming where the break was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChainStatus {
    Intact,
    Broken { at: Sequence },
}

/// The frozen run configuration snapshot `init` takes (ARCHITECTURE §7, §15).
/// Never given its own struct in either spec file — every field here is named in
/// prose as "recorded in the manifest" or "frozen by `init`", never assembled
/// into one listing (PLAN_DEVIATIONS.md D19).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub policy_version: PolicyVersion,
    /// Hash of the resolved `DecisionConfig`, frozen at `init` (ARCHITECTURE §7).
    pub config_hash: String,
    /// The prompt pack's hash, snapshotted at `init` (INTERFACES §15, §23).
    pub pack_hash: String,
    /// The correlation table's own version (INTERFACES §15), so a run can be
    /// explained against the grouping actually in force when it ran.
    pub correlation_table_version: String,
    /// Seeds `StageContext::rng` (`DeterministicRng`, INTERFACES §6) — "seeded
    /// from the manifest".
    pub rng_seed: u64,
}

/// A dollar amount. Every ledger quantity in ARCHITECTURE §7/§8.3 (`estimate`,
/// `reserved_amount`, `actual_cost`, `committed`, `reserved`) is this type; the
/// spec never states its representation beyond "money", so this is a bare `f64`
/// newtype rather than a fixed-point type no worked example asks for.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Cost(pub f64);

/// The response cache's key: `(provider, model, params, prompt_hash)` — "never
/// `prompt_hash` alone: the same prompt sent to two models has one `prompt_hash`
/// and two different answers" (INTERFACES §5). `params` is the call's own
/// parameters (temperature, max_tokens, ...); no concrete params type exists yet
/// anywhere in the workspace, so this holds their canonical serialized form.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CacheKey {
    pub provider: arbiter_core::ProviderId,
    pub model: arbiter_core::ModelId,
    pub params: String,
    pub prompt_hash: String,
}

/// A cached provider response. ARCHITECTURE §8.2: above `blob_threshold` (128 KB
/// default) the row holds a hash/size pointing into the blob store rather than
/// the payload inline — `inline` is `None` in exactly that case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedResponse {
    pub response_hash: String,
    pub size_bytes: u64,
    pub inline: Option<String>,
}

/// "Content-addressed, `serde`-typed, and versioned" (INTERFACES §6) — a trait,
/// not a struct, so a single `Tx` can store heterogeneous stage outputs (claims,
/// relations, judge scores, ...) behind one object-safe method (D19).
pub trait Artifact: std::fmt::Debug + Send + Sync {
    /// A stable name for the artifact's shape, e.g. `"claims.normalize.v1"`.
    fn artifact_type(&self) -> &'static str;
    /// `blake3` (or equivalent) of the artifact's canonical serialized form —
    /// what makes it content-addressed.
    fn content_hash(&self) -> String;
}

/// INTERFACES §1, copied verbatim except `manifest`/`run_id` types resolve to
/// this crate's/`arbiter-core`'s concrete types.
pub trait RunStore: Send + Sync {
    /// Opens a new run for writing. `AlreadyOpen` if another process holds the
    /// lease.
    fn create(&self, run_id: &RunId, manifest: &Manifest)
    -> Result<Box<dyn RunWriter>, StoreError>;
    /// Re-opens an existing run for writing, for `resume`.
    fn reopen(&self, run_id: &RunId) -> Result<Box<dyn RunWriter>, StoreError>;
    /// Concurrent reader. Never blocks the writer; never observes a partial
    /// commit.
    fn reader(&self, run_id: &RunId) -> Result<Box<dyn RunReader>, StoreError>;
}

pub trait RunWriter: Send {
    /// Everything inside the closure commits, or none of it does. `T` in
    /// INTERFACES §1's own `fn transact<T>(...) -> Result<T, StoreError>` cannot
    /// survive object safety here — `RunStore::create`/`reopen` return
    /// `Box<dyn RunWriter>`, and a trait with a generic method cannot be made
    /// into a trait object at all (D20). Callers extract a result by capturing it
    /// from inside the closure instead of receiving it as a return value.
    fn transact(
        &mut self,
        f: &mut dyn FnMut(&mut dyn Tx) -> Result<(), StoreError>,
    ) -> Result<(), StoreError>;
}

pub trait Tx {
    fn append_event(&mut self, e: &Event) -> Result<Sequence, StoreError>;
    fn put_artifact(&mut self, a: &dyn Artifact) -> Result<ArtifactId, StoreError>;
    fn put_cache(&mut self, k: &CacheKey, r: &CachedResponse) -> Result<(), StoreError>;
    fn commit_budget(&mut self, r: &ReservationId, actual: Cost) -> Result<(), StoreError>;
    fn set_call_state(&mut self, c: &CallId, s: CallState) -> Result<(), StoreError>;
}

pub trait RunReader: Send {
    /// Always ordered by sequence. SQL has no inherent row order and a
    /// byte-for-byte `DecisionRecord` cannot depend on one that happens to hold.
    fn events(&self) -> Result<Box<dyn Iterator<Item = Event>>, StoreError>;
    fn verify_chain(&self) -> Result<ChainStatus, StoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventType;
    use crate::ids::{EventId, StageName};
    use std::sync::{Arc, Mutex};

    #[derive(Debug)]
    struct TestArtifact;
    impl Artifact for TestArtifact {
        fn artifact_type(&self) -> &'static str {
            "test.v1"
        }
        fn content_hash(&self) -> String {
            "blake3:test".to_string()
        }
    }

    /// An in-memory `RunStore`/`RunWriter`/`Tx`/`RunReader` built entirely from
    /// these trait definitions, with no SQLite involved -- this is the compile-
    /// and-behave check K0 promises ("compiles with arbiter-core as its only
    /// internal dep"), plus proof the object-safety fix in D20 actually lets a
    /// caller get a value out of `transact`.
    struct MemWriter {
        events: Arc<Mutex<Vec<Event>>>,
        opened_twice: bool,
    }

    impl RunWriter for MemWriter {
        fn transact(
            &mut self,
            f: &mut dyn FnMut(&mut dyn Tx) -> Result<(), StoreError>,
        ) -> Result<(), StoreError> {
            if self.opened_twice {
                return Err(StoreError::AlreadyOpen);
            }
            let mut tx = MemTx {
                events: self.events.clone(),
            };
            f(&mut tx)
        }
    }

    struct MemTx {
        events: Arc<Mutex<Vec<Event>>>,
    }

    impl Tx for MemTx {
        fn append_event(&mut self, e: &Event) -> Result<Sequence, StoreError> {
            let mut events = self.events.lock().unwrap();
            let seq = Sequence::new(events.len() as u64 + 1);
            let mut stored = e.clone();
            stored.sequence = Some(seq);
            events.push(stored);
            Ok(seq)
        }
        fn put_artifact(&mut self, a: &dyn Artifact) -> Result<ArtifactId, StoreError> {
            Ok(ArtifactId::new(a.content_hash()))
        }
        fn put_cache(&mut self, _k: &CacheKey, _r: &CachedResponse) -> Result<(), StoreError> {
            Ok(())
        }
        fn commit_budget(&mut self, _r: &ReservationId, _actual: Cost) -> Result<(), StoreError> {
            Ok(())
        }
        fn set_call_state(&mut self, _c: &CallId, _s: CallState) -> Result<(), StoreError> {
            Ok(())
        }
    }

    fn sample_event() -> Event {
        Event {
            schema_version: 1,
            event_id: EventId::new("evt_1"),
            run_id: RunId::new("run_1"),
            sequence: None,
            timestamp: "2026-08-31T12:04:11.221Z".to_string(),
            stage: StageName::new("claims.extract"),
            event_type: EventType::ClaimExtracted,
            durable: false,
            payload: serde_json::json!({}),
            content_hash: "blake3:abc".to_string(),
            previous_event_hash: None,
        }
    }

    #[test]
    fn transact_lets_a_caller_extract_a_value_via_closure_capture() {
        let mut writer = MemWriter {
            events: Arc::new(Mutex::new(Vec::new())),
            opened_twice: false,
        };
        let mut captured: Option<Sequence> = None;
        writer
            .transact(&mut |tx| {
                let seq = tx.append_event(&sample_event())?;
                let artifact_id = tx.put_artifact(&TestArtifact)?;
                assert_eq!(artifact_id.as_str(), "blake3:test");
                captured = Some(seq);
                Ok(())
            })
            .unwrap();
        assert_eq!(captured, Some(Sequence::new(1)));
    }

    #[test]
    fn a_second_open_on_the_same_writer_reports_already_open_not_a_hang() {
        let mut writer = MemWriter {
            events: Arc::new(Mutex::new(Vec::new())),
            opened_twice: true,
        };
        let result = writer.transact(&mut |_tx| Ok(()));
        assert!(matches!(result, Err(StoreError::AlreadyOpen)));
    }

    #[test]
    fn chain_status_round_trips_through_json() {
        let broken = ChainStatus::Broken {
            at: Sequence::new(7),
        };
        let json = serde_json::to_value(broken).unwrap();
        let back: ChainStatus = serde_json::from_value(json).unwrap();
        assert_eq!(broken, back);
    }
}
