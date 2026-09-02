//! F1: the shared scaffolding every one of the 36 CI fixtures (F2, §18)
//! needs to construct a real `StageContext` and run one or more stages
//! against a scripted `MockProvider` — zero LLM tokens, no network.
//!
//! `arbiter-fixtures` cannot depend on `arbiter-cli` (the dependency rule,
//! X2's own test: "nothing depends on cli"), so this is not a thin wrapper
//! around `arbiter-cli::orchestrator::run_pipeline` — it is its own,
//! smaller construction of the same primitives (`ProviderRegistry`,
//! `BudgetLedger`, `ResponseCache`, `EventSink`), built directly against
//! `arbiter-kernel`'s public API the same way `orchestrator.rs` itself is.
//! Most fixtures need only a handful of stages, not the full thirteen, so
//! this is deliberately a *toolkit* for wiring exactly the stages one
//! fixture needs, not a second copy of the full pipeline (PLAN_DEVIATIONS.md
//! D47).

use arbiter_core::Policy;
use arbiter_kernel::budget::BudgetLedger;
use arbiter_kernel::cache::ResponseCache;
use arbiter_kernel::event::EventType;
use arbiter_kernel::ids::StageName;
use arbiter_kernel::prompt::{PromptTemplate, VariableSchema};
use arbiter_kernel::stage::{
    CancellationToken, DeterministicRng, EventSink, ProviderRegistry, StageContext,
};
use arbiter_kernel::store::Cost;
use arbiter_providers::mock::MockProvider;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Every event a stage emitted during a fixture's run, for assertions like
/// "a `CALL_ORPHANED` was emitted" or "no `BUDGET_EXHAUSTED` fired" —
/// exactly the kind of proof several fixtures need (`crash_midcall`,
/// `budget_exceeded`, `adaptive_stop`, ...).
#[derive(Debug, Default)]
pub struct RecordingSink {
    events: Mutex<Vec<(EventType, StageName, serde_json::Value)>>,
}

impl RecordingSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Vec<(EventType, StageName, serde_json::Value)> {
        self.events.lock().unwrap().clone()
    }

    pub fn contains(&self, event_type: EventType) -> bool {
        self.events
            .lock()
            .unwrap()
            .iter()
            .any(|(t, _, _)| *t == event_type)
    }

    pub fn count(&self, event_type: EventType) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _, _)| *t == event_type)
            .count()
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, event_type: EventType, stage: &StageName, payload: serde_json::Value) {
        self.events
            .lock()
            .unwrap()
            .push((event_type, stage.clone(), payload));
    }
}

/// Owns every piece a `StageContext` borrows from, so a fixture can build
/// one, register scripted providers, and call `stage.run(input,
/// &harness.ctx(round)).await` directly — the same shape every G-stage's
/// own unit tests already use internally, just shared here instead of
/// redefined per fixture.
#[derive(Debug)]
pub struct Harness {
    pub providers: ProviderRegistry,
    pub budget: BudgetLedger,
    pub cache: ResponseCache,
    pub events: RecordingSink,
    deadline: Instant,
    rng_seed: u64,
}

impl Harness {
    /// `budget_cap`: `None` for unbounded (most fixtures don't care about
    /// the cap and would rather fail loudly on a real bug than on an
    /// under-sized test budget); `Some(cost)` for fixtures that are
    /// specifically about the cap (`budget_exceeded`, `budget_reconciliation`).
    pub fn new(budget_cap: Option<Cost>) -> Self {
        Self {
            providers: ProviderRegistry::new(),
            budget: match budget_cap {
                Some(cap) => BudgetLedger::new(Some(cap)),
                None => BudgetLedger::unbounded(),
            },
            cache: ResponseCache::new(),
            events: RecordingSink::new(),
            deadline: Instant::now() + Duration::from_secs(300),
            rng_seed: 1,
        }
    }

    pub fn with_rng_seed(mut self, seed: u64) -> Self {
        self.rng_seed = seed;
        self
    }

    pub fn register(&mut self, provider: MockProvider) -> &mut Self {
        self.providers.register(Box::new(provider));
        self
    }

    pub fn ctx(&self, round: u32) -> StageContext<'_> {
        StageContext {
            providers: &self.providers,
            budget: &self.budget,
            events: &self.events,
            cache: &self.cache,
            deadline: self.deadline,
            cancel: CancellationToken::new(),
            round,
            rng: DeterministicRng::seeded(self.rng_seed),
        }
    }
}

/// The one policy this codebase implements — every fixture's shared
/// starting point for weights/graph/thresholds/confidence constants.
pub fn policy() -> Policy {
    Policy::argument_v1()
}

/// A minimal in-memory prompt template — fixtures never load the real
/// `prompts/default/v1/` pack from disk (that would make them depend on the
/// filesystem layout and drift with template wording changes that have
/// nothing to do with what a fixture is proving); every G-stage's own unit
/// tests already build templates this same minimal way.
pub fn template(stage: &str, body: &str, variables: &[&str]) -> PromptTemplate {
    PromptTemplate {
        stage: StageName::new(stage),
        body: body.to_string(),
        variables: VariableSchema::new(variables.iter().map(|s| s.to_string())),
    }
}

/// A `MockProvider` under `mock`, capabilities matching what every stage's
/// own call path already assumes (no structured output, no streaming, no
/// idempotency key) — the same shape `arbiter-cli::synthetic::SyntheticProvider`
/// declares, since a fixture provider is not testing capability negotiation.
pub fn mock_provider() -> MockProvider {
    use arbiter_kernel::provider::ProviderCapabilities;
    MockProvider::new(
        arbiter_core::ProviderId::new("mock"),
        ProviderCapabilities {
            structured_output: false,
            streaming: false,
            idempotency: None,
        },
    )
}
