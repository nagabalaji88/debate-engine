//! F2 — `adaptive_stop` (G7), ARCHITECTURE §18's CI suite: "controller
//! stops early on no-new-information."

use arbiter_core::config::{AttachmentParams, DisputeWeights, GraphParams, Thresholds, Weights};
use arbiter_core::decision::attachment::AttachmentMatrix;
use arbiter_fixtures::harness::RecordingSink;
use arbiter_kernel::budget::BudgetLedger;
use arbiter_kernel::cache::ResponseCache;
use arbiter_kernel::stage::{
    CancellationToken, ControlFlow, DeterministicRng, ProviderRegistry, Stage, StageContext,
    StopReason,
};
use arbiter_kernel::stages::claims_normalize::NormalizedClaims;
use arbiter_kernel::stages::controller_decide::{ControllerDecide, DecideInput};
use arbiter_kernel::stages::disputes_rank::{RankInput, RankedDisputes};
use arbiter_kernel::stages::options_cluster::ClusteredOptions;
use arbiter_kernel::stages::rebuttal_run::RebuttalsRun;
use arbiter_kernel::stages::relations_analyze::AnalyzedRelations;
use arbiter_kernel::store::Cost;
use std::time::{Duration, Instant};

fn empty_ranked() -> RankedDisputes {
    RankedDisputes {
        claims: NormalizedClaims(vec![]),
        relations: AnalyzedRelations(vec![]),
        options: ClusteredOptions {
            options: vec![],
            direct_matrix: AttachmentMatrix::default(),
        },
        standing: Default::default(),
        propagated_matrix: AttachmentMatrix::default(),
        ranked: vec![],
    }
}

/// With no options at all, `converged()` returns `false` unconditionally
/// (`rank_by_share` on an empty score list has no top option to check
/// dissent/margin against) -- isolating the `NoNewInformation` branch from
/// `Converged` cleanly, without needing to construct a scenario that is
/// simultaneously claim-rich and margin-clear. Zero claims in both the
/// current round and the previous round trivially satisfies both of
/// `no_new_information`'s own conditions: zero new claim ids, and zero
/// standing delta (an empty `fold` default) — both comfortably under any
/// positive threshold.
#[tokio::test]
async fn adaptive_stop() {
    let stage = ControllerDecide::new(
        Weights::default(),
        GraphParams::default(),
        Thresholds::default(),
        AttachmentParams::default(),
        DisputeWeights::default(),
        Cost(0.01),
        5,    // max_rounds -- well above ctx.round below, so RoundLimit never wins first
        1.2,  // converged_margin_factor
        1,    // min_new_claims
        0.05, // min_standing_delta
    );

    let input = DecideInput {
        rebuttals: RebuttalsRun {
            next_round_input: RankInput {
                claims: NormalizedClaims(vec![]),
                relations: AnalyzedRelations(vec![]),
                options: ClusteredOptions {
                    options: vec![],
                    direct_matrix: AttachmentMatrix::default(),
                },
            },
            outcomes: vec![],
        },
        previous: empty_ranked(),
    };

    let providers = ProviderRegistry::new();
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

    let out = stage.run(input, &c).await.unwrap();

    assert_eq!(
        out.control,
        ControlFlow::Stop(StopReason::NoNewInformation),
        "zero new claims and zero standing movement must stop the round early, before RoundLimit is ever reached"
    );
    assert!(
        !out.converged,
        "this scenario must reach NoNewInformation on its own, not via Converged"
    );
    assert!(out.no_new_information);
    assert_eq!(out.new_claim_count, 0);
    assert_eq!(out.max_standing_delta, 0.0);
}
