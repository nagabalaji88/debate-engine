//! F2 — `judge_failure` (G8), ARCHITECTURE §18's CI suite: "invalid judge
//! JSON → retry → judge term degrades."
//!
//! `judge.evaluate` (`arbiter-kernel/src/stages/judge_evaluate.rs`) makes
//! exactly one call per judge with no per-judge retry loop anywhere in its
//! own `run` — an unparseable response is simply skipped (see this file's
//! own `an_unparseable_judge_response_degrades_without_scoring_anyone`).
//! ARCHITECTURE §18's "retry" wording has no literal counterpart in this
//! stage's real behavior (logged under D47); what genuinely happens, and
//! what this fixture proves instead, is the degrade: a model still gets
//! scored from its remaining valid judges rather than the whole evaluation
//! failing.

use arbiter_core::claim::{ClaimLifecycle, ClaimMember, EvidenceKind, Grounding, TextSpan};
use arbiter_core::decision::attachment::AttachmentMatrix;
use arbiter_core::{CanonicalClaim, ClaimId, ModelId, PositionId, ProviderId};
use arbiter_fixtures::harness::{RecordingSink, template};
use arbiter_kernel::budget::BudgetLedger;
use arbiter_kernel::cache::ResponseCache;
use arbiter_kernel::event::EventType;
use arbiter_kernel::stage::{
    CancellationToken, DeterministicRng, ProviderRegistry, Stage, StageContext,
};
use arbiter_kernel::stages::claims_normalize::NormalizedClaims;
use arbiter_kernel::stages::disputes_rank::RankedDisputes;
use arbiter_kernel::stages::judge_evaluate::{JudgeEvaluate, JudgeInput};
use arbiter_kernel::stages::options_cluster::ClusteredOptions;
use arbiter_kernel::stages::positions_generate::{Position, Positions};
use arbiter_kernel::stages::relations_analyze::AnalyzedRelations;
use arbiter_kernel::store::Cost;
use arbiter_providers::mock::MockProvider;
use std::collections::BTreeMap;
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

fn resolved() -> RankedDisputes {
    RankedDisputes {
        claims: NormalizedClaims(vec![CanonicalClaim {
            id: ClaimId::new("c1"),
            text: "a supporting fact".to_string(),
            kind: EvidenceKind::Fact,
            lifecycle: ClaimLifecycle::Defended,
            members: vec![ClaimMember::new(
                ClaimId::new("c1"),
                ModelId::new("model-a"),
                ProviderId::new("model-a-provider"),
                PositionId::new("pos_a"),
                "a supporting fact",
                quoted("a supporting fact"),
            )],
        }]),
        relations: AnalyzedRelations(vec![]),
        options: ClusteredOptions {
            options: vec![],
            direct_matrix: AttachmentMatrix::default(),
        },
        standing: BTreeMap::new(),
        propagated_matrix: AttachmentMatrix::default(),
        ranked: vec![],
    }
}

/// `judge_failure`: judge-1 returns unparseable JSON, judge-2 returns a
/// real scorecard for the same model. The evaluation still completes
/// (`Ok`, never a `StageError`), the model still gets scored -- but from
/// one contributing judge, not two: the judge term degrades gracefully
/// rather than the whole round failing on a single bad response.
#[tokio::test]
async fn judge_failure() {
    let judge1 = MockProvider::new(
        ProviderId::new("judge-provider-1"),
        arbiter_kernel::provider::ProviderCapabilities {
            structured_output: false,
            streaming: false,
            idempotency: None,
        },
    );
    judge1.script_text("not a scorecard array at all");
    let judge2 = MockProvider::new(
        ProviderId::new("judge-provider-2"),
        arbiter_kernel::provider::ProviderCapabilities {
            structured_output: false,
            streaming: false,
            idempotency: None,
        },
    );
    judge2.script_text(
        serde_json::json!([
            {"pseudonym": "A", "factual_correctness": 0.8, "logical_reasoning": 0.8,
             "evidence_quality": 0.8, "problem_relevance": 0.8, "assumption_quality": 0.8,
             "counterargument_handling": 0.8, "risk_awareness": 0.8, "practicality": 0.8,
             "clarity": 0.8}
        ])
        .to_string(),
    );

    let mut providers = ProviderRegistry::new();
    providers.register(Box::new(judge1));
    providers.register(Box::new(judge2));
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

    let positions = Positions(vec![Position {
        id: PositionId::new("pos_a"),
        model: ModelId::new("model-a"),
        provider: ProviderId::new("model-a-provider"),
        text: "Do the thing.".to_string(),
    }]);
    let input = JudgeInput {
        positions,
        resolved: resolved(),
        exchanges: vec![],
    };

    let stage = JudgeEvaluate::new(
        template("judge.evaluate", "{{dossiers}}", &["dossiers"]),
        vec![
            (ModelId::new("judge-1"), ProviderId::new("judge-provider-1")),
            (ModelId::new("judge-2"), ProviderId::new("judge-provider-2")),
        ],
        Cost(0.10),
    );

    let out = stage
        .run(input, &c)
        .await
        .expect("one bad judge response must not fail the whole evaluation");

    let mean = out
        .scores_by_model
        .get(&ModelId::new("model-a"))
        .expect("the model must still be scored");
    assert!(
        (mean.factual_correctness - 0.8).abs() < 1e-9,
        "the mean must come from the one valid judge alone"
    );

    let per_judge = out.per_judge_scores.get(&ModelId::new("model-a")).unwrap();
    assert_eq!(
        per_judge.len(),
        1,
        "only the valid judge's scorecard contributes: the term genuinely degraded"
    );
    assert_eq!(
        events.count(EventType::JudgeScored),
        1,
        "only one JUDGE_SCORED event: the failed judge scored no one"
    );
}
