//! F2 — premise cycles (owned by `claims.extract`/G2's own implementation,
//! not `relations.analyze`/G4 despite the plan's stage-table row naming G4 —
//! see D47), ARCHITECTURE §18's CI suite.
//!
//! IMPLEMENTATION_PLAN.md's ledger lists both `premise_cycle` and
//! `premise_cycle_grounded_fact` under G4, and its own G2–G9 stage table
//! (line 907) also describes "premise cycles: Kahn sort, minimum edge cut...
//! keeps its Fact weight" against `relations.analyze`. But the actual
//! mechanism — `topo_sort`/`cut_cycle_edges`, INTERFACES §2's untangle
//! protocol — lives entirely in `claims.extract`
//! (`arbiter-kernel/src/stages/claims_extract.rs`), as G2's own scope note
//! and D32 already document. This is a plan-ledger labeling mismatch, not a
//! missing feature: the functionality is built and already unit-tested
//! inside that module. These two fixtures exercise it through the same
//! public `Stage` surface F1's harness uses elsewhere.

use arbiter_core::claim::{EvidenceKind, Grounding};
use arbiter_core::{ModelId, PositionId, ProviderId};
use arbiter_fixtures::harness::{RecordingSink, template};
use arbiter_kernel::budget::BudgetLedger;
use arbiter_kernel::cache::ResponseCache;
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

/// `premise_cycle`: "circular derivation → component degraded to
/// Unsupported." Two inference candidates cite only each other as premises —
/// neither has, or transitively reaches, a real quote — and no repair budget
/// is available, so the untangle protocol falls straight to the greedy edge
/// cut. Cutting the weaker edge leaves one candidate with no premises at all
/// and the other still depending on an ungrounded candidate: both remain
/// ungrounded and are admitted as `Unsupported`/`Unverified`, not silently
/// dropped or looped forever.
#[tokio::test]
async fn premise_cycle() {
    let mock = MockProvider::new(
        ProviderId::new("mock"),
        arbiter_kernel::provider::ProviderCapabilities {
            structured_output: false,
            streaming: false,
            idempotency: None,
        },
    );
    mock.script_text(
        serde_json::json!([
            {"text": "a depends on b", "kind": "inference", "grounding": {"derived_from": ["#2"], "confidence": 0.3}},
            {"text": "b depends on a", "kind": "inference", "grounding": {"derived_from": ["#1"], "confidence": 0.5}}
        ])
        .to_string(),
    );
    let mut providers = ProviderRegistry::new();
    providers.register(Box::new(mock));
    let budget = BudgetLedger::unbounded();
    let cache = ResponseCache::new();
    let events = RecordingSink::new();
    let c = ctx(&providers, &budget, &cache, &events);

    let positions = Positions(vec![position("Our team has 8 developers on staff.")]);
    let out = stage(Cost(0.0)).run(positions, &c).await.unwrap();

    assert_eq!(
        out.0.len(),
        2,
        "both candidates survive as claims even though neither grounds"
    );
    for claim in &out.0 {
        assert_eq!(
            claim.members[0].grounding,
            Grounding::Unsupported,
            "a pure two-node cycle with no independent grounding anywhere must degrade to Unsupported"
        );
        assert_eq!(claim.kind, EvidenceKind::Unverified);
    }
}

/// `premise_cycle_grounded_fact`: "cycle member with a direct quote keeps
/// Fact weight." A three-candidate cycle where one member (`#1`) is itself
/// grounded by a real, independently-verifiable quote in the source text —
/// its grounding is resolved by exact/fuzzy quote matching *before* the
/// cycle machinery ever runs (`resolve_with_edges`'s own step 1/2), so it
/// keeps `EvidenceKind::Fact` regardless of what happens to the other two
/// candidates that cite each other (`#2`/`#3`) — grounding degradation from
/// a cycle never reaches back and downgrades an independently-quoted fact.
#[tokio::test]
async fn premise_cycle_grounded_fact() {
    let mock = MockProvider::new(
        ProviderId::new("mock"),
        arbiter_kernel::provider::ProviderCapabilities {
            structured_output: false,
            streaming: false,
            idempotency: None,
        },
    );
    mock.script_text(
        serde_json::json!([
            {"text": "base fact", "kind": "fact", "grounding": {"quote": "our team has 8 developers"}},
            {"text": "a", "kind": "inference", "grounding": {"derived_from": ["#1", "#3"], "confidence": 0.1}},
            {"text": "b", "kind": "inference", "grounding": {"derived_from": ["#2"], "confidence": 0.9}}
        ])
        .to_string(),
    );
    let mut providers = ProviderRegistry::new();
    providers.register(Box::new(mock));
    let budget = BudgetLedger::unbounded();
    let cache = ResponseCache::new();
    let events = RecordingSink::new();
    let c = ctx(&providers, &budget, &cache, &events);

    let positions = Positions(vec![position("Our team has 8 developers on staff.")]);
    let out = stage(Cost(0.0)).run(positions, &c).await.unwrap();

    assert_eq!(out.0.len(), 3);
    assert!(
        matches!(out.0[0].members[0].grounding, Grounding::DirectQuote { .. }),
        "the independently-quoted candidate keeps its DirectQuote grounding regardless of the cycle"
    );
    assert_eq!(
        out.0[0].kind,
        EvidenceKind::Fact,
        "a direct quote keeps Fact weight even while it is cited as a premise inside a cycle"
    );
    // #2/#3 cite each other; cutting #2's weaker edge (0.1, to #3) leaves
    // #2 -> #1 intact, and #1 is grounded, so both resolve as Derived.
    assert!(matches!(
        out.0[1].members[0].grounding,
        Grounding::Derived { .. }
    ));
    assert!(matches!(
        out.0[2].members[0].grounding,
        Grounding::Derived { .. }
    ));
}
