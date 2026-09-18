//! F2 — `provider_timeout` (K2), ARCHITECTURE §18's CI suite: "SkipItem,
//! reservation released, 4-model debate completes."
//!
//! **§18's summary line and §8.4's state table disagree, and §8.4 wins**
//! (PLAN_DEVIATIONS.md D60). §18 says "reservation released"; §8.4's own
//! branch table says a call that reached `SENT` is held:
//!
//! > | `SENT` | it may have been billed, and there is no id to check | hold;
//! > report as orphaned |
//!
//! and, a paragraph above it, **"`ORPHANED` must never silently become
//! `FAILED`. Collapsing the two is what produces a duplicate charge on
//! resume."** A timeout is the textbook case: the request left the machine, so
//! nothing here can prove the provider did not run and bill the completion.
//! This fixture asserted the release, which made it a test *for* the
//! collapse §8.4 forbids. What §18 is really fixing is the `SkipItem`
//! behaviour -- a weaker debate rather than a failed round -- and that is
//! unchanged and still asserted below.

use arbiter_core::{ModelId, ProviderId};
use arbiter_fixtures::harness::{RecordingSink, template};
use arbiter_kernel::budget::BudgetLedger;
use arbiter_kernel::cache::ResponseCache;
use arbiter_kernel::provider::{ProviderCapabilities, ProviderError};
use arbiter_kernel::stage::{
    CancellationToken, DeterministicRng, ProviderRegistry, Stage, StageContext,
};
use arbiter_kernel::stages::positions_generate::{PositionsGenerate, Question};
use arbiter_kernel::store::Cost;
use arbiter_providers::mock::MockProvider;
use std::time::{Duration, Instant};

fn provider(id: &str) -> MockProvider {
    MockProvider::new(
        ProviderId::new(id),
        ProviderCapabilities {
            structured_output: false,
            streaming: false,
            idempotency: None,
        },
    )
}

/// A single model timing out in round 1 must not fail the round: the
/// remaining three panel members' positions still come back, the timed-out
/// model's reservation is *held* as an orphan (§8.4: money that may already be
/// spent stays held until reconciliation), and the stage itself returns `Ok`,
/// never a `StageError`.
#[tokio::test]
async fn provider_timeout() {
    let p1 = provider("p1");
    p1.script_text("position from p1");
    let p2 = provider("p2"); // this one times out
    p2.script(Err(ProviderError::Other("timed out".to_string())));
    let p3 = provider("p3");
    p3.script_text("position from p3");
    let p4 = provider("p4");
    p4.script_text("position from p4");

    let mut providers = ProviderRegistry::new();
    providers.register(Box::new(p1));
    providers.register(Box::new(p2));
    providers.register(Box::new(p3));
    providers.register(Box::new(p4));

    let budget = BudgetLedger::new(Some(Cost(10.0)));
    let cache = ResponseCache::new();
    let events = RecordingSink::new();
    let c = StageContext {
        providers: &providers,
        budget: &budget,
        events: &events,
        cache: &cache,
        deadline: Instant::now() + Duration::from_secs(30),
        cancel: CancellationToken::new(),
        round: 1,
        rng: DeterministicRng::seeded(1),
    };

    let stage = PositionsGenerate::new(
        vec![
            (ModelId::new("m1"), ProviderId::new("p1")),
            (ModelId::new("m2"), ProviderId::new("p2")),
            (ModelId::new("m3"), ProviderId::new("p3")),
            (ModelId::new("m4"), ProviderId::new("p4")),
        ],
        template("positions.generate", "{{question}}", &["question"]),
        Cost(1.0),
        4,
    );

    let out = stage
        .run(
            Question {
                text: "Should we adopt a modular monolith?".to_string(),
            },
            &c,
        )
        .await
        .expect("one model timing out must SkipItem, never fail the whole round");

    assert_eq!(
        out.0.len(),
        3,
        "the debate completes with the three models that answered"
    );
    assert!(
        !out.0.iter().any(|p| p.model.as_str() == "m2"),
        "the timed-out model must not contribute a position"
    );
    assert_eq!(
        budget.reserved(),
        Cost(1.0),
        "a timed-out call reached SENT, so its reservation is held as orphaned spend, \
         not handed back as though the provider had proven it never ran (§8.4)"
    );
    assert_eq!(
        budget.committed(),
        Cost(3.0),
        "only the three successful calls actually spent budget"
    );

    // The event has to be on the record too: an orphan the operator cannot see
    // is money quietly consumed. `doctor` reports the orphaned subtotal from
    // exactly these.
    assert!(
        events
            .events()
            .iter()
            .any(|(t, _, _)| *t == arbiter_kernel::event::EventType::CallOrphaned),
        "a held reservation must be announced with CALL_ORPHANED, never held silently"
    );
}
