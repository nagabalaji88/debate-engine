//! `StageGraph` semantics, INTERFACES §6 / ARCHITECTURE §7. The `Stage` trait,
//! `StageContext`, the idempotency-key formula, and the round-control types
//! (`ControlFlow`/`StopReason`).
//!
//! PLAN_DEVIATIONS.md D19's category of gap applies again here: several of
//! `Stage`'s own supporting types (`RunContext`, `Key`, `CostEstimate`,
//! `StageError`, and `StageContext`'s `ProviderRegistry`/`EventSink`/
//! `DeterministicRng`/`CancellationToken` fields) have no concrete definition
//! anywhere in either spec file. Each is authored here from whatever prose was
//! available; see each type's own doc comment for its anchor. `ProviderRegistry`
//! specifically stays a near-empty placeholder — it cannot be more than that
//! before P1 defines the `Provider` trait it would hold.

use crate::budget::BudgetLedger;
use crate::cache::ResponseCache;
use crate::event::EventType;
use crate::ids::StageName;
use crate::store::Cost;
use arbiter_core::{ClaimId, RunId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// A stage's idempotency key. `blake3:`-prefixed, matching the workspace's
/// existing hash-field convention.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Key(String);

impl Key {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What `Stage::idempotency_key` needs beyond the stage's own input.
///
/// Neither spec file gives this its own struct (PLAN_DEVIATIONS.md D19), and
/// the two files disagree on which axes the key formula itself should carry —
/// resolved in [`idempotency_key`]'s own doc comment (D23).
#[derive(Debug, Clone)]
pub struct RunContext {
    pub run_id: RunId,
    pub round: u32,
    pub engine_version: String,
    pub config_hash: String,
}

/// `blake3(stage_name ‖ engine_version ‖ config_hash ‖ round ‖
/// input_artifact_hashes)` — INTERFACES §5's literal formula, copied as given.
///
/// PLAN_DEVIATIONS.md D23: ARCHITECTURE §7 describes the key's axes
/// differently — "idempotency key per `(run_id, stage, input_hash)`" — and
/// argues `policy_version`/`pack_hash`/`table_version`/`config_hash` don't
/// need to be in it at all, since they're frozen constants within a run and
/// `run_id` already prevents cross-run collision. The two descriptions
/// disagree on whether `run_id` and `config_hash`/`engine_version` belong.
/// Resolved as the union of both: hashing a few extra, already-constant-
/// within-a-run values alongside the ones that actually vary (`round`,
/// `input_artifact_hashes`) cannot cause a false match, only, in principle,
/// an unnecessary one it never reaches — a strictly safer position than
/// picking one file's list and guessing the other's omission was deliberate.
/// Fields joined with a U+0001 separator so no two distinct input sequences
/// can concatenate to the same string.
pub fn idempotency_key(
    stage: &StageName,
    ctx: &RunContext,
    input_artifact_hashes: &[String],
) -> Key {
    let mut parts = vec![
        stage.as_str().to_string(),
        ctx.run_id.as_str().to_string(),
        ctx.engine_version.clone(),
        ctx.config_hash.clone(),
        ctx.round.to_string(),
    ];
    parts.extend(input_artifact_hashes.iter().cloned());
    let joined = parts.join("\u{1}");
    Key(format!(
        "blake3:{}",
        blake3::hash(joined.as_bytes()).to_hex()
    ))
}

/// Tracks which idempotency keys have already been computed, so a resumed or
/// re-entrant stage invocation can skip recomputation (ARCHITECTURE §7's whole
/// point of having the key at all).
#[derive(Debug, Default)]
pub struct IdempotencyMemo {
    seen: Mutex<BTreeSet<Key>>,
}

impl IdempotencyMemo {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` the first time this key is seen (the caller should compute);
    /// `false` on every subsequent call with an equal key (already done, skip).
    pub fn should_compute(&self, key: &Key) -> bool {
        self.seen.lock().unwrap().insert(key.clone())
    }
}

/// `Serial | PerItem { max: usize }` — INTERFACES §6, copied verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parallelism {
    Serial,
    PerItem { max: usize },
}

/// Never given a struct definition; assembled from ARCHITECTURE §11's own
/// cost-breakdown table columns (calls, tokens, dollar cost) — the exact
/// quantities a stage's pre-flight estimate needs to report.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostEstimate {
    pub calls: u32,
    pub tokens: u64,
    pub cost: Cost,
}

/// "Failure policy is declared per stage: `Fatal | DegradeWithEvent |
/// SkipItem`" (INTERFACES §6 prose) — stated as a requirement but not shown as
/// a trait method in that section's own code block; added to [`Stage`] as
/// `failure_policy()` since the prose gives no other place for it to live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailurePolicy {
    Fatal,
    DegradeWithEvent,
    SkipItem,
}

#[derive(Debug, thiserror::Error)]
pub enum StageError {
    #[error("{0}")]
    Other(String),
}

/// INTERFACES §9, copied verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Converged,
    RoundLimit,
    NoNewInformation,
    BudgetExhausted,
    TokenLimit,
    Deadline,
    Cancelled,
    ProviderFailure,
}

/// INTERFACES §6, copied verbatim. What `controller.decide` returns; the
/// executor re-instantiates the round subgraph
/// (`challenge.plan → challenge.run → rebuttal.run → controller.decide`) with
/// `round` folded into the idempotency key so a resumed run re-enters the
/// right iteration — the graph's one controlled loop, never an arbitrary cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlFlow {
    Continue { round: u32, focus: Vec<ClaimId> },
    Stop(StopReason),
}

/// A deterministic PRNG seeded from the manifest (INTERFACES §6:
/// `StageContext::rng`, "seeded from the manifest"). SplitMix64 — public
/// domain, small, and sufficient for reproducible sampling; not
/// cryptographic, and nothing here claims it should be.
#[derive(Debug, Clone, Copy)]
pub struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    pub fn seeded(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`, from the top 53 bits (a `f64` mantissa's width).
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}

/// A shared, clonable cancellation flag. INTERFACES §6 names the field
/// (`cancel: CancellationToken`) without defining the type; `Arc<AtomicBool>`
/// is the minimal safe (`#![forbid(unsafe_code)]`) implementation of "tell
/// every clone of this token to stop."
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// The seam a stage appends events through. A trait, not a concrete type, for
/// the same reason `RunStore` is (D1): the real implementation needs
/// `arbiter-store`'s hash-chaining machinery (`events::ChainState`), which
/// this crate cannot depend on.
pub trait EventSink: std::fmt::Debug + Send + Sync {
    fn emit(&self, event_type: EventType, stage: &StageName, payload: serde_json::Value);
}

/// Placeholder until P1 defines the `Provider` trait this would hold a
/// registry of — there is nothing meaningful to put in this type before that
/// exists. Kept as a distinct, named type now rather than deferred entirely,
/// so `StageContext`'s shape matches INTERFACES §6 today; P1 fills in its
/// fields and lookup methods without changing `StageContext` itself.
#[derive(Debug, Default)]
pub struct ProviderRegistry {
    _private: (),
}

/// INTERFACES §6, copied field-for-field.
#[derive(Debug)]
pub struct StageContext<'a> {
    pub providers: &'a ProviderRegistry,
    pub budget: &'a BudgetLedger,
    pub events: &'a dyn EventSink,
    pub cache: &'a ResponseCache,
    pub deadline: Instant,
    pub cancel: CancellationToken,
    pub round: u32,
    pub rng: DeterministicRng,
}

/// INTERFACES §6, copied signature-for-signature, plus `failure_policy()`
/// (see [`FailurePolicy`]'s doc comment for why it's added here).
pub trait Stage: Send + Sync {
    type In: crate::store::Artifact;
    type Out: crate::store::Artifact;

    fn name(&self) -> StageName;
    fn parallelism(&self) -> Parallelism;
    fn failure_policy(&self) -> FailurePolicy;
    fn idempotency_key(&self, input: &Self::In, ctx: &RunContext) -> Key;
    fn cost_estimate(&self, input: &Self::In) -> CostEstimate;

    fn run(
        &self,
        input: Self::In,
        ctx: &StageContext<'_>,
    ) -> impl std::future::Future<Output = Result<Self::Out, StageError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Artifact;

    fn ctx(round: u32) -> RunContext {
        RunContext {
            run_id: RunId::new("run_1"),
            round,
            engine_version: "0.1.0".to_string(),
            config_hash: "blake3:cfg".to_string(),
        }
    }

    #[test]
    fn same_input_hash_is_not_recomputed() {
        let memo = IdempotencyMemo::new();
        let stage = StageName::new("claims.extract");
        let hashes = vec!["blake3:input_a".to_string()];

        let key1 = idempotency_key(&stage, &ctx(1), &hashes);
        let key2 = idempotency_key(&stage, &ctx(1), &hashes);
        assert_eq!(key1, key2, "identical inputs must produce identical keys");

        assert!(
            memo.should_compute(&key1),
            "first time this key is seen: compute"
        );
        assert!(
            !memo.should_compute(&key2),
            "same key again: already computed, skip"
        );
    }

    #[test]
    fn a_different_round_produces_a_different_key() {
        let stage = StageName::new("challenge.run");
        let hashes = vec!["blake3:input_a".to_string()];
        let key_round_1 = idempotency_key(&stage, &ctx(1), &hashes);
        let key_round_2 = idempotency_key(&stage, &ctx(2), &hashes);
        assert_ne!(
            key_round_1, key_round_2,
            "round must be part of the key so a resumed run re-enters the right iteration"
        );
    }

    #[test]
    fn a_different_input_hash_produces_a_different_key_and_is_recomputed() {
        let memo = IdempotencyMemo::new();
        let stage = StageName::new("claims.extract");
        let key_a = idempotency_key(&stage, &ctx(1), &["blake3:input_a".to_string()]);
        let key_b = idempotency_key(&stage, &ctx(1), &["blake3:input_b".to_string()]);
        assert_ne!(key_a, key_b);
        assert!(memo.should_compute(&key_a));
        assert!(
            memo.should_compute(&key_b),
            "a genuinely new input must compute"
        );
    }

    #[test]
    fn deterministic_rng_is_reproducible_from_the_same_seed() {
        let mut a = DeterministicRng::seeded(42);
        let mut b = DeterministicRng::seeded(42);
        for _ in 0..10 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let mut c = DeterministicRng::seeded(43);
        assert_ne!(DeterministicRng::seeded(42).next_u64(), c.next_u64());
    }

    #[test]
    fn deterministic_rng_next_f64_stays_in_unit_range() {
        let mut r = DeterministicRng::seeded(7);
        for _ in 0..1000 {
            let v = r.next_f64();
            assert!((0.0..1.0).contains(&v), "{v} out of [0,1)");
        }
    }

    #[test]
    fn cancellation_token_clones_share_state() {
        let token = CancellationToken::new();
        let clone = token.clone();
        assert!(!token.is_cancelled());
        clone.cancel();
        assert!(
            token.is_cancelled(),
            "cancelling a clone must cancel every clone"
        );
    }

    // A minimal concrete `Stage`, proving the trait as given (INTERFACES §6's
    // signatures, plus `failure_policy()`) is actually implementable and
    // runnable against a real `StageContext` -- not just type-level scaffolding
    // nothing can instantiate.

    #[derive(Debug)]
    struct Question(String);
    impl crate::store::Artifact for Question {
        fn artifact_type(&self) -> &'static str {
            "test.question.v1"
        }
        fn content_hash(&self) -> String {
            format!("blake3:{}", blake3::hash(self.0.as_bytes()).to_hex())
        }
    }

    #[derive(Debug)]
    struct WordCount(usize);
    impl crate::store::Artifact for WordCount {
        fn artifact_type(&self) -> &'static str {
            "test.word_count.v1"
        }
        fn content_hash(&self) -> String {
            format!("blake3:{}", blake3::hash(&self.0.to_le_bytes()).to_hex())
        }
    }

    struct CountWords;
    impl Stage for CountWords {
        type In = Question;
        type Out = WordCount;

        fn name(&self) -> StageName {
            StageName::new("test.count_words")
        }
        fn parallelism(&self) -> Parallelism {
            Parallelism::Serial
        }
        fn failure_policy(&self) -> FailurePolicy {
            FailurePolicy::Fatal
        }
        fn idempotency_key(&self, input: &Self::In, ctx: &RunContext) -> Key {
            idempotency_key(&self.name(), ctx, &[input.content_hash()])
        }
        fn cost_estimate(&self, _input: &Self::In) -> CostEstimate {
            CostEstimate {
                calls: 0,
                tokens: 0,
                cost: Cost(0.0),
            }
        }
        async fn run(
            &self,
            input: Self::In,
            ctx: &StageContext<'_>,
        ) -> Result<Self::Out, StageError> {
            if ctx.cancel.is_cancelled() {
                return Err(StageError::Other("cancelled".to_string()));
            }
            ctx.events.emit(
                EventType::StageCompleted,
                &self.name(),
                serde_json::json!({"words": input.0.split_whitespace().count()}),
            );
            Ok(WordCount(input.0.split_whitespace().count()))
        }
    }

    #[derive(Debug, Default)]
    struct RecordingSink {
        emitted: Mutex<Vec<(EventType, String)>>,
    }
    impl EventSink for RecordingSink {
        fn emit(&self, event_type: EventType, stage: &StageName, _payload: serde_json::Value) {
            self.emitted
                .lock()
                .unwrap()
                .push((event_type, stage.as_str().to_string()));
        }
    }

    #[tokio::test]
    async fn a_concrete_stage_runs_against_a_real_stage_context() {
        let stage = CountWords;
        let registry = ProviderRegistry::default();
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let stage_ctx = StageContext {
            providers: &registry,
            budget: &budget,
            events: &sink,
            cache: &cache,
            deadline: Instant::now() + std::time::Duration::from_secs(30),
            cancel: CancellationToken::new(),
            round: 1,
            rng: DeterministicRng::seeded(1),
        };

        let input = Question("what should we build".to_string());
        let out = stage.run(input, &stage_ctx).await.unwrap();
        assert_eq!(out.0, 4);
        assert_eq!(sink.emitted.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_cancelled_context_stops_a_running_stage() {
        let stage = CountWords;
        let registry = ProviderRegistry::default();
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let stage_ctx = StageContext {
            providers: &registry,
            budget: &budget,
            events: &sink,
            cache: &cache,
            deadline: Instant::now() + std::time::Duration::from_secs(30),
            cancel,
            round: 1,
            rng: DeterministicRng::seeded(1),
        };

        let input = Question("irrelevant".to_string());
        let result = stage.run(input, &stage_ctx).await;
        assert!(result.is_err());
        assert!(sink.emitted.lock().unwrap().is_empty());
    }
}
