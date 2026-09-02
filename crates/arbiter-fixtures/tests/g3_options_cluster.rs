//! F2 — `options.cluster` (G3), ARCHITECTURE §18's CI suite.

use arbiter_core::claim::{ClaimLifecycle, ClaimMember, EvidenceKind, Grounding, TextSpan};
use arbiter_core::{CanonicalClaim, ClaimId, ModelId, OptionId, PositionId, ProviderId};
use arbiter_fixtures::harness::{RecordingSink, template};
use arbiter_kernel::budget::BudgetLedger;
use arbiter_kernel::cache::ResponseCache;
use arbiter_kernel::stage::{
    CancellationToken, DeterministicRng, ProviderRegistry, Stage, StageContext,
};
use arbiter_kernel::stages::claims_normalize::NormalizedClaims;
use arbiter_kernel::stages::options_cluster::{ClusterInput, OptionsCluster};
use arbiter_kernel::stages::positions_generate::{Position, Positions};
use arbiter_kernel::store::Cost;
use arbiter_providers::mock::MockProvider;
use std::time::{Duration, Instant};

fn position(id: &str, model: &str, text: &str) -> Position {
    Position {
        id: PositionId::new(id),
        model: ModelId::new(model),
        provider: ProviderId::new("mock"),
        text: text.to_string(),
    }
}

fn stage() -> OptionsCluster {
    OptionsCluster::new(
        template("options.cluster", "{{positions}}", &["positions"]),
        template(
            "options.attach",
            "{{claims}} {{options}}",
            &["claims", "options"],
        ),
        (ModelId::new("model-a"), ProviderId::new("mock")),
        Cost(0.01),
    )
}

fn mock() -> MockProvider {
    MockProvider::new(
        ProviderId::new("mock"),
        arbiter_kernel::provider::ProviderCapabilities {
            structured_output: false,
            streaming: false,
            idempotency: None,
        },
    )
}

fn claim_from(id: &str, text: &str, position_id: &str, model: &str) -> CanonicalClaim {
    let member = ClaimMember::new(
        ClaimId::new(id),
        ModelId::new(model),
        ProviderId::new("mock"),
        PositionId::new(position_id),
        text,
        Grounding::DirectQuote {
            span: TextSpan {
                start: 0,
                end: text.len(),
                quote: text.to_string(),
            },
        },
    );
    CanonicalClaim {
        id: ClaimId::new(id),
        text: text.to_string(),
        kind: EvidenceKind::Fact,
        lifecycle: ClaimLifecycle::Proposed,
        members: vec![member],
    }
}

/// `option_clustering`: "5 recommendations → 3 options, attachment matrix,
/// stable ids." Five independently-generated positions cluster into three
/// groups; every option's id is deterministically minted from its group's
/// survivor position id (`opt_<position id>`, `OptionsCluster::cluster_positions`'s
/// own scheme) rather than from a counter or the clustering call's own
/// order — running the identical input through the stage twice must
/// therefore mint the exact same three ids both times.
#[tokio::test]
async fn option_clustering() {
    let cluster_response = || {
        serde_json::json!([
            {"members": ["#1", "#2"], "label": "Adopt a modular monolith", "confidence": 0.9},
            {"members": ["#3", "#4"], "label": "Adopt microservices", "confidence": 0.9},
            {"members": ["#5"], "label": "Do nothing yet", "confidence": 0.8}
        ])
    };

    let positions = Positions(vec![
        position("pos_1", "model-a", "We should adopt a modular monolith."),
        position("pos_2", "model-b", "A modular monolith is the right call."),
        position("pos_3", "model-c", "We should adopt microservices."),
        position("pos_4", "model-d", "Microservices are the way to go."),
        position("pos_5", "model-e", "Let's do nothing yet."),
    ]);
    let claims = NormalizedClaims(vec![claim_from(
        "claim_1",
        "our team has 8 developers",
        "pos_1",
        "model-a",
    )]);

    let mut ids_by_run = Vec::new();
    for _ in 0..2 {
        let provider = mock();
        provider.script_text(cluster_response().to_string());
        provider.script_text(serde_json::json!([]).to_string()); // classifier adds nothing extra
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(provider));
        let budget = BudgetLedger::unbounded();
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

        let out = stage()
            .run(
                ClusterInput {
                    positions: positions.clone(),
                    claims: claims.clone(),
                },
                &c,
            )
            .await
            .unwrap();

        assert_eq!(
            out.options.len(),
            3,
            "five positions must cluster into exactly three options"
        );
        assert!(
            out.direct_matrix
                .cells
                .contains_key(&(ClaimId::new("claim_1"), OptionId::new("opt_pos_1"))),
            "the authored claim must attach to its own position's option in the direct matrix"
        );
        ids_by_run.push(out.options.iter().map(|o| o.id.clone()).collect::<Vec<_>>());
    }

    assert_eq!(
        ids_by_run[0], ids_by_run[1],
        "identical input must mint identical option ids across separate runs"
    );
    assert!(ids_by_run[0].contains(&OptionId::new("opt_pos_1")));
    assert!(ids_by_run[0].contains(&OptionId::new("opt_pos_3")));
    assert!(ids_by_run[0].contains(&OptionId::new("opt_pos_5")));
}

/// `option_emerges_midround`: "new option proposed in a rebuttal earns its
/// own cluster." At deep depth `options.cluster` re-runs each round
/// (IMPLEMENTATION_PLAN.md's own G2-G9 table); this fixture simulates round
/// 2's re-cluster after a rebuttal introduces a position genuinely
/// different from anything round 1 saw. The stage's own "no option is ever
/// invented, none ever dropped" guarantee (`cluster_positions`'s fallback
/// path) means a position the clustering response does mention as its own
/// group earns a fresh, independent option id, while the two original
/// positions' shared option is undisturbed.
#[tokio::test]
async fn option_emerges_midround() {
    // Round 1: two positions cluster into one option.
    let provider1 = mock();
    provider1.script_text(
        serde_json::json!([{"members": ["#1", "#2"], "label": "Adopt a modular monolith", "confidence": 0.9}]).to_string(),
    );
    let mut providers1 = ProviderRegistry::new();
    providers1.register(Box::new(provider1));
    let budget1 = BudgetLedger::unbounded();
    let cache1 = ResponseCache::new();
    let events1 = RecordingSink::new();
    let c1 = StageContext {
        providers: &providers1,
        budget: &budget1,
        events: &events1,
        cache: &cache1,
        deadline: Instant::now() + Duration::from_secs(30),
        cancel: CancellationToken::new(),
        round: 1,
        rng: DeterministicRng::seeded(1),
    };
    let round1_positions = Positions(vec![
        position("pos_1", "model-a", "Adopt a monolith."),
        position("pos_2", "model-b", "Monolith, agreed."),
    ]);
    let round1 = stage()
        .run(
            ClusterInput {
                positions: round1_positions.clone(),
                claims: NormalizedClaims(vec![]),
            },
            &c1,
        )
        .await
        .unwrap();
    assert_eq!(round1.options.len(), 1);

    // Round 2: the same two positions, plus a rebuttal-born position
    // proposing something genuinely new. The clustering response keeps the
    // first group intact and adds a lone group for the new entrant.
    let provider2 = mock();
    provider2.script_text(
        serde_json::json!([
            {"members": ["#1", "#2"], "label": "Adopt a modular monolith", "confidence": 0.9},
            {"members": ["#3"], "label": "Adopt a hybrid rollout instead", "confidence": 0.85}
        ])
        .to_string(),
    );
    let mut providers2 = ProviderRegistry::new();
    providers2.register(Box::new(provider2));
    let budget2 = BudgetLedger::unbounded();
    let cache2 = ResponseCache::new();
    let events2 = RecordingSink::new();
    let c2 = StageContext {
        providers: &providers2,
        budget: &budget2,
        events: &events2,
        cache: &cache2,
        deadline: Instant::now() + Duration::from_secs(30),
        cancel: CancellationToken::new(),
        round: 2,
        rng: DeterministicRng::seeded(1),
    };
    let mut round2_positions = round1_positions.0.clone();
    round2_positions.push(position(
        "pos_rebuttal",
        "model-a",
        "Actually, a hybrid rollout is safer.",
    ));
    let round2 = stage()
        .run(
            ClusterInput {
                positions: Positions(round2_positions),
                claims: NormalizedClaims(vec![]),
            },
            &c2,
        )
        .await
        .unwrap();

    assert_eq!(
        round2.options.len(),
        2,
        "the new rebuttal position must earn its own second option"
    );
    assert!(
        round2
            .options
            .iter()
            .any(|o| o.id == OptionId::new("opt_pos_rebuttal")),
        "the emergent option's id is minted from the rebuttal position's own id"
    );
    assert!(
        round2
            .options
            .iter()
            .any(|o| o.id == OptionId::new("opt_pos_1")),
        "the original option (from round 1's surviving position) must still be present, undisturbed"
    );
}
