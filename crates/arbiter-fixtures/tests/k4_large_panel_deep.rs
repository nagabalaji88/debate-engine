//! F2 — `large_panel_deep` (K4), ARCHITECTURE §18's CI suite: "7 models ×
//! deep depth: controller cuts challenge count, stays under the cap."

use arbiter_core::claim::{ClaimLifecycle, ClaimMember, EvidenceKind, Grounding, TextSpan};
use arbiter_core::decision::attachment::AttachmentMatrix;
use arbiter_core::relation::{Relation, RelationKind};
use arbiter_core::{CanonicalClaim, ClaimId, ModelId, PositionId, ProviderId};
use arbiter_fixtures::harness::RecordingSink;
use arbiter_kernel::budget::BudgetLedger;
use arbiter_kernel::cache::ResponseCache;
use arbiter_kernel::stage::{
    CancellationToken, DeterministicRng, ProviderRegistry, Stage, StageContext,
};
use arbiter_kernel::stages::challenge_plan::ChallengePlan;
use arbiter_kernel::stages::claims_normalize::NormalizedClaims;
use arbiter_kernel::stages::disputes_rank::{DisputeRank, RankedDisputes};
use arbiter_kernel::stages::options_cluster::ClusteredOptions;
use arbiter_kernel::stages::relations_analyze::AnalyzedRelations;
use arbiter_kernel::store::Cost;
use std::time::{Duration, Instant};

fn quoted(text: &str) -> Grounding {
    Grounding::DirectQuote {
        span: TextSpan {
            start: 0,
            end: text.len(),
            quote: text.to_string(),
        },
    }
}

fn claim(id: &str, model: &str) -> CanonicalClaim {
    let text = format!("claim from {model}");
    let member = ClaimMember::new(
        ClaimId::new(id),
        ModelId::new(model),
        ProviderId::new(format!("{model}-provider")),
        PositionId::new(format!("pos_{model}")),
        &text,
        quoted(&text),
    );
    CanonicalClaim {
        id: ClaimId::new(id),
        text,
        kind: EvidenceKind::Fact,
        lifecycle: ClaimLifecycle::Defended,
        members: vec![member],
    }
}

/// Seven models (`m0`..`m6`), each authoring one claim that contradicts the
/// next model's claim in a ring (`m0` attacks `m1`'s claim, `m1` attacks
/// `m2`'s, ..., `m6` attacks `m0`'s) -- seven live, equally-plausible
/// candidate challenges, far more than a tight budget can afford. At deep
/// depth (`max_rounds` 3, ARCHITECTURE §5.5's own ceiling) with a
/// deliberately small remaining budget, `challenge.plan`'s money-derived
/// sizing must plan strictly fewer challenges than there are candidates,
/// while never exceeding the derived `challenge_budget` and never letting
/// any one challenger exceed `max_challenges_per_model`.
#[tokio::test]
async fn large_panel_deep() {
    let models: Vec<String> = (0..7).map(|i| format!("m{i}")).collect();
    let claims: Vec<CanonicalClaim> = models
        .iter()
        .map(|m| claim(&format!("claim_{m}"), m))
        .collect();

    // Ring of contradictions: claim_i's attacker is claim_(i+1).
    let relations: Vec<Relation> = (0..7)
        .map(|i| Relation {
            from: ClaimId::new(format!("claim_m{}", (i + 1) % 7)),
            to: ClaimId::new(format!("claim_m{i}")),
            kind: RelationKind::Contradicts,
            confidence: 0.9,
        })
        .collect();

    let mut standing = std::collections::BTreeMap::new();
    for m in &models {
        standing.insert(ClaimId::new(format!("claim_{m}")), 0.6);
    }

    // Every claim is a live, equally-ranked dispute candidate.
    let ranked: Vec<DisputeRank> = (0..7)
        .map(|i| DisputeRank {
            claim_id: ClaimId::new(format!("claim_m{i}")),
            priority: 0.9,
            contested_mass: 0.6,
            decision_leverage: 0.5,
            evidence_gap: 0.3,
            resolution_cost: 0.1,
        })
        .collect();

    let resolved = RankedDisputes {
        claims: NormalizedClaims(claims),
        relations: AnalyzedRelations(relations),
        options: ClusteredOptions {
            options: vec![],
            direct_matrix: AttachmentMatrix::default(),
        },
        standing,
        propagated_matrix: AttachmentMatrix::default(),
        ranked,
    };

    // Deep depth (max_rounds = 3), a tight remaining budget: round_budget =
    // 1.05 / 3 = 0.35, challenge_budget = 0.35 - 0.05 (judge reservation) =
    // 0.30, which affords exactly 3 exchanges at 0.10 each -- fewer than
    // the 7 available candidates.
    let stage = ChallengePlan::new(3, Cost(0.10), Cost(0.05), 2);
    let budget = BudgetLedger::new(Some(Cost(1.05)));
    let providers = ProviderRegistry::new();
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

    let out = stage.run(resolved, &c).await.unwrap();

    assert!(
        out.pairs.len() < 7,
        "the controller must cut the challenge count well below the 7 available candidates, got {}",
        out.pairs.len()
    );
    assert!(
        !out.pairs.is_empty(),
        "the budget must still afford at least one challenge"
    );

    let total_cost: f64 = out.pairs.iter().map(|p| p.estimated_cost.0).sum();
    assert!(
        total_cost <= 0.30 + 1e-9,
        "planned spend ({total_cost}) must stay under the derived challenge_budget (0.30)"
    );

    let mut per_challenger: std::collections::BTreeMap<ModelId, usize> =
        std::collections::BTreeMap::new();
    for pair in &out.pairs {
        *per_challenger.entry(pair.challenger.clone()).or_insert(0) += 1;
    }
    assert!(
        per_challenger.values().all(|&n| n <= 2),
        "no single challenger may exceed max_challenges_per_model (2)"
    );
}
