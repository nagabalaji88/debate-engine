//! Bridges a stage's in-process `EventSink` (ARCHITECTURE §7/§9) to a real,
//! hash-chained `arbiter-store` run — the orchestrator's own concern, not any
//! stage's (D42, PLAN_DEVIATIONS.md).
//!
//! `EventSink::emit(&self, ...)` (`arbiter-kernel/src/stage.rs`, fixed by
//! every G2-G9 stage already shipped) returns `()`, not a `Result` — there is
//! no channel for a store write failure to travel back through it without
//! reopening every stage's own already-tested call site. Resolved by
//! recording the first failure into `RunHandle` and having the orchestrator
//! poll `RunHandle::take_error` after each stage completes, rather than
//! silently losing it or panicking inside a trait method whose signature
//! this task does not own.

use arbiter_core::RunId;
use arbiter_kernel::event::{Event, EventType};
use arbiter_kernel::ids::{ArtifactId, EventId, StageName};
use arbiter_kernel::stage::EventSink;
use arbiter_kernel::store::{Artifact, RunWriter, StoreError};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

struct Inner {
    writer: Box<dyn RunWriter>,
    chain: arbiter_store::events::ChainState,
}

/// Owns the one open `RunWriter` for a run, shared between event emission
/// (via [`Sink`], which every stage's `StageContext::events` borrows) and
/// artifact persistence (called directly by the orchestrator between
/// stages — no stage calls `put_artifact` itself).
#[derive(Debug)]
pub struct RunHandle {
    run_id: RunId,
    inner: Mutex<Inner>,
    next_event_id: AtomicU64,
    /// Mixed into every event id this handle mints, alongside `next_event_id`
    /// — a counter alone collides the moment a *second* `RunHandle` is ever
    /// constructed against the same run (`resume`, `accept`, a second
    /// `replay`, ...): each restarts its own counter at 1, so its first
    /// event id would repeat one this run's very first process already used
    /// (`events.event_id` is `UNIQUE`, so this failed loudly rather than
    /// silently, but only once a second handle actually existed to collide —
    /// PLAN_DEVIATIONS.md D45). A nanosecond timestamp captured once per
    /// `RunHandle` construction makes every instance's own id space disjoint
    /// from every other's, with no need to read prior state back first.
    instance_tag: u128,
    error: Mutex<Option<StoreError>>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner").finish_non_exhaustive()
    }
}

impl RunHandle {
    pub fn new(run_id: RunId, writer: Box<dyn RunWriter>) -> Self {
        let instance_tag = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self {
            run_id,
            inner: Mutex::new(Inner {
                writer,
                chain: arbiter_store::events::ChainState::empty(),
            }),
            next_event_id: AtomicU64::new(1),
            instance_tag,
            error: Mutex::new(None),
        }
    }

    /// Continues a chain that already has events in it (e.g. `init`'s own
    /// `RUN_STARTED`, appended before this handle exists) — otherwise the
    /// first event this handle appends would carry `previous_event_hash:
    /// None`, breaking the chain against what is already on disk.
    pub fn continuing_from(mut self, last_event: Option<&Event>) -> Self {
        self.inner.get_mut().unwrap().chain =
            arbiter_store::events::ChainState::from_last_event(last_event);
        self
    }

    /// Borrows an [`EventSink`] backed by this handle — pass to every stage's
    /// `StageContext::events` for the run's lifetime.
    pub fn sink(&self) -> Sink<'_> {
        Sink(self)
    }

    pub fn put_artifact(&self, artifact: &dyn Artifact) -> Result<ArtifactId, StoreError> {
        let mut inner = self.inner.lock().unwrap();
        let mut result = None;
        inner.writer.transact(&mut |tx| {
            result = Some(tx.put_artifact(artifact)?);
            Ok(())
        })?;
        Ok(result.expect("transact only returns Ok after the closure ran"))
    }

    /// Persists one `ResponseCache::snapshot()` entry to `cache_entries` —
    /// the only path that table is ever written through (PLAN_DEVIATIONS.md
    /// D44); see the orchestrator's own call site for when.
    pub fn put_cache_entry(
        &self,
        key: &arbiter_kernel::store::CacheKey,
        response: &arbiter_kernel::store::CachedResponse,
    ) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        inner.writer.transact(&mut |tx| tx.put_cache(key, response))
    }

    /// Appends one event outside the `EventSink` seam — used for the two
    /// lifecycle events (`RUN_COMPLETED`/`RUN_FAILED`) no stage emits, since
    /// they bracket the whole run rather than belonging to any one stage.
    pub fn append_lifecycle_event(
        &self,
        event_type: EventType,
        payload: serde_json::Value,
    ) -> Result<(), StoreError> {
        self.append(StageName::new("run"), event_type, payload, true)
    }

    fn append(
        &self,
        stage: StageName,
        event_type: EventType,
        payload: serde_json::Value,
        durable: bool,
    ) -> Result<(), StoreError> {
        let n = self.next_event_id.fetch_add(1, Ordering::SeqCst);
        let event = Event {
            schema_version: 1,
            event_id: EventId::new(format!(
                "evt_{}_{}_{n:06}",
                self.run_id.as_str(),
                self.instance_tag
            )),
            run_id: self.run_id.clone(),
            sequence: None,
            timestamp: arbiter_store::now_rfc3339(),
            stage,
            event_type,
            durable,
            payload,
            content_hash: String::new(),
            previous_event_hash: None,
        };
        let mut guard = self.inner.lock().unwrap();
        let inner = &mut *guard;
        arbiter_store::events::append_chained(inner.writer.as_mut(), &mut inner.chain, event)?;
        Ok(())
    }

    /// The first store-write failure recorded by [`Sink::emit`], if any,
    /// taken so a repeated poll after the orchestrator has already handled
    /// it does not re-report the same failure.
    pub fn take_error(&self) -> Option<StoreError> {
        self.error.lock().unwrap().take()
    }
}

/// The `EventSink` every stage in a run shares — a thin borrow over
/// [`RunHandle`], since `StageContext::events: &'a dyn EventSink` needs a
/// reference, not an owned value.
#[derive(Debug)]
pub struct Sink<'a>(&'a RunHandle);

impl EventSink for Sink<'_> {
    fn emit(&self, event_type: EventType, stage: &StageName, payload: serde_json::Value) {
        if let Err(e) = self.0.append(stage.clone(), event_type, payload, false) {
            let mut slot = self.0.error.lock().unwrap();
            if slot.is_none() {
                *slot = Some(e);
            }
        }
    }
}
