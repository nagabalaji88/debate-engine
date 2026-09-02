//! F2 — claims.extract grounding/repair (G2), ARCHITECTURE §18's CI suite.

use arbiter_core::claim::{EvidenceKind, Grounding};
use arbiter_core::config::Weights;
use arbiter_core::{ModelId, PositionId, ProviderId};
use arbiter_fixtures::harness::{RecordingSink, template};
use arbiter_kernel::budget::BudgetLedger;
use arbiter_kernel::cache::ResponseCache;
use arbiter_kernel::event::EventType;
use arbiter_kernel::stage::{
    CancellationToken, DeterministicRng, ProviderRegistry, Stage, StageContext,
};
use arbiter_kernel::stages::claims_extract::ClaimsExtract;
use arbiter_kernel::stages::positions_generate::{Position, Positions};
use arbiter_kernel::store::Cost;
use arbiter_providers::mock::MockProvider;
use std::time::{Duration, Instant};

fn position(text: &str) -> Position {
    Position {
        id: PositionId::new("pos_mock_model-a"),
        model: ModelId::new("model-a"),
        provider: ProviderId::new("mock"),
        text: text.to_string(),
    }
}

fn stage(repair_cap: Cost) -> ClaimsExtract {
    ClaimsExtract::new(
        template("claims.extract", "{{position_text}}", &["position_text"]),
        template(
            "claims.repair",
            "{{position_text}} {{failed_claims}}",
            &["position_text", "failed_claims"],
        ),
        (ModelId::new("model-a"), ProviderId::new("mock")),
        Cost(0.01),
        repair_cap,
        1,
    )
}

fn ctx<'a>(
    providers: &'a ProviderRegistry,
    budget: &'a BudgetLedger,
    cache: &'a ResponseCache,
    events: &'a RecordingSink,
) -> StageContext<'a> {
    StageContext {
        providers,
        budget,
        events,
        cache,
        deadline: Instant::now() + Duration::from_secs(30),
        cancel: CancellationToken::new(),
        round: 1,
        rng: DeterministicRng::seeded(1),
    }
}

/// `malformed_claim`: "schema violation → repair → accepted." A candidate
/// whose grounding declares neither a `quote` nor `derived_from` — a
/// violation of INTERFACES §2's own contract that every candidate ground
/// itself one of those two ways — parses fine as JSON (every `RawGrounding`
/// field is optional) but resolves to nothing, so the repair loop fires
/// exactly as it would for a wrong quote; a repair response that supplies a
/// real quote is accepted.
#[tokio::test]
async fn malformed_claim() {
    let mock = MockProvider::new(
        ProviderId::new("mock"),
        arbiter_kernel::provider::ProviderCapabilities {
            structured_output: false,
            streaming: false,
            idempotency: None,
        },
    );
    mock.script_text(
        serde_json::json!([{"text": "we have 8 developers", "kind": "fact", "grounding": {}}])
            .to_string(),
    );
    mock.script_text(
        serde_json::json!([{"index": "#1", "kind": "fact", "grounding": {"quote": "our team has 8 developers"}}])
            .to_string(),
    );
    let mut providers = ProviderRegistry::new();
    providers.register(Box::new(mock));
    let budget = BudgetLedger::unbounded();
    let cache = ResponseCache::new();
    let events = RecordingSink::new();
    let c = ctx(&providers, &budget, &cache, &events);

    let positions = Positions(vec![position("Our team has 8 developers on staff.")]);
    let out = stage(Cost(1.0)).run(positions, &c).await.unwrap();

    assert_eq!(out.0.len(), 1);
    assert!(
        matches!(out.0[0].members[0].grounding, Grounding::DirectQuote { .. }),
        "the schema-violating candidate must be accepted once repair supplies a real quote"
    );
    assert_eq!(out.0[0].kind, EvidenceKind::Fact);
}

/// `ungrounded_claim`: "repair fails → Unsupported at 0.15, still reaches
/// the decision." A candidate whose quote never matches the position text,
/// even after one repair attempt, is admitted as `Grounding::Unsupported`
/// (`EvidenceKind::Unverified`) rather than dropped or erroring the stage —
/// `Weights::kind_unverified` (0.15) is the exact evidence weight this
/// grounding carries forward into the confidence/outcome machinery (C5/C6),
/// so an unresolvable claim still has a well-defined path all the way to a
/// decision, never a stage failure.
#[tokio::test]
async fn ungrounded_claim() {
    let mock = MockProvider::new(
        ProviderId::new("mock"),
        arbiter_kernel::provider::ProviderCapabilities {
            structured_output: false,
            streaming: false,
            idempotency: None,
        },
    );
    mock.script_text(
        serde_json::json!([{"text": "we have 8 developers", "kind": "fact", "grounding": {"quote": "totally wrong quote"}}])
            .to_string(),
    );
    mock.script_text(
        serde_json::json!([{"index": "#1", "kind": "fact", "grounding": {"quote": "still wrong"}}])
            .to_string(),
    );
    let mut providers = ProviderRegistry::new();
    providers.register(Box::new(mock));
    let budget = BudgetLedger::unbounded();
    let cache = ResponseCache::new();
    let events = RecordingSink::new();
    let c = ctx(&providers, &budget, &cache, &events);

    let positions = Positions(vec![position("Our team has 8 developers on staff.")]);
    let result = stage(Cost(1.0)).run(positions, &c).await;

    let out = result.expect(
        "an unresolvable claim must still reach a completed stage result, never a StageError",
    );
    assert_eq!(out.0.len(), 1);
    assert_eq!(out.0[0].members[0].grounding, Grounding::Unsupported);
    assert_eq!(out.0[0].kind, EvidenceKind::Unverified);
    assert_eq!(
        Weights::default().kind_unverified,
        0.15,
        "the fixture's own name for this weight ('Unsupported at 0.15') must match the config default"
    );
    assert!(events.count(EventType::ClaimUngrounded) >= 1);
}
