//! F2 — `budget_exceeded` (K4), ARCHITECTURE §18's CI suite: "cap hit
//! mid-round → truncated decision, penalty applied."

use arbiter_core::config::{AttachmentParams, ConfidenceWeights, GraphParams, Thresholds, Weights};
use arbiter_core::decision::attachment::{Attachment, AttachmentMatrix};
use arbiter_core::{
    AttachSource, CanonicalClaim, ClaimId, ClaimLifecycle, ClaimMember, DecisionOption,
    EvidenceKind, Grounding, ModelId, OptionId, Polarity, PolicyVersion, PositionId, ProviderId,
    RunId, Scorecard, TextSpan,
};
use arbiter_fixtures::harness::RecordingSink;
use arbiter_kernel::budget::BudgetLedger;
use arbiter_kernel::cache::ResponseCache;
use arbiter_kernel::stage::{
    CancellationToken, ControlFlow, DeterministicRng, ProviderRegistry, Stage, StageContext,
    StopReason,
};
use arbiter_kernel::stages::claims_normalize::NormalizedClaims;
use arbiter_kernel::stages::decision_synthesize::{
    Completeness, DecisionSynthesize, SynthesizeInput,
};
use arbiter_kernel::stages::disputes_rank::RankedDisputes;
use arbiter_kernel::stages::judge_evaluate::JudgeEvaluation;
use arbiter_kernel::stages::options_cluster::ClusteredOptions;
use arbiter_kernel::stages::relations_analyze::AnalyzedRelations;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

fn flat_scorecard(model: &str, v: f64) -> Scorecard {
    Scorecard {
        model: ModelId::new(model),
        factual_correctness: v,
        logical_reasoning: v,
        evidence_quality: v,
        problem_relevance: v,
        assumption_quality: v,
        counterargument_handling: v,
        risk_awareness: v,
        practicality: v,
        clarity: v,
    }
}

fn quoted(text: &str) -> Grounding {
    Grounding::DirectQuote {
        span: TextSpan {
            start: 0,
            end: text.len(),
            quote: text.to_string(),
        },
    }
}

fn claim(id: &str, text: &str, model: &str) -> CanonicalClaim {
    let member = ClaimMember::new(
        ClaimId::new(id),
        ModelId::new(model),
        ProviderId::new(format!("{model}-provider")),
        PositionId::new(format!("pos_{model}")),
        text,
        quoted(text),
    );
    CanonicalClaim {
        id: ClaimId::new(id),
        text: text.to_string(),
        kind: EvidenceKind::Fact,
        lifecycle: ClaimLifecycle::Defended,
        members: vec![member],
    }
}

fn input_for(final_control: ControlFlow) -> SynthesizeInput {
    let option = DecisionOption::new(OptionId::new("opt_a"), "Do the thing");
    let claims = vec![claim("c1", "a strong supporting fact", "model-a")];
    let mut matrix = AttachmentMatrix::default();
    matrix.cells.insert(
        (ClaimId::new("c1"), option.id.clone()),
        Attachment {
            polarity: Polarity::Supports,
            confidence: 1.0,
            source: AttachSource::Authored,
        },
    );

    let resolved = RankedDisputes {
        claims: NormalizedClaims(claims),
        relations: AnalyzedRelations(vec![]),
        options: ClusteredOptions {
            options: vec![option],
            direct_matrix: matrix,
        },
        standing: BTreeMap::new(),
        propagated_matrix: AttachmentMatrix::default(),
        ranked: vec![],
    };
    let mut scores_by_model = BTreeMap::new();
    scores_by_model.insert(ModelId::new("model-a"), flat_scorecard("model-a", 0.9));
    let mut per_judge_scores = BTreeMap::new();
    per_judge_scores.insert(
        ModelId::new("model-a"),
        vec![flat_scorecard("model-a", 0.9)],
    );

    SynthesizeInput {
        run_id: RunId::new("run_1"),
        question: "what should we build?".to_string(),
        judged: JudgeEvaluation {
            resolved,
            scores_by_model,
            per_judge_scores,
        },
        final_control,
    }
}

fn stage() -> DecisionSynthesize {
    DecisionSynthesize::new(
        Weights::default(),
        GraphParams::default(),
        Thresholds::default(),
        AttachmentParams::default(),
        ConfidenceWeights::default(),
        PolicyVersion::new("argument-v1"),
        "0.1.0",
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

/// `budget_exceeded`: "cap hit mid-round → truncated decision, penalty
/// applied." The exact same well-evidenced scenario that reads `Complete`
/// under a normal `Converged` stop reads `Truncated { reason:
/// BudgetExhausted, .. }` when the round loop stopped because the budget
/// ran out mid-round instead — and `decision.synthesize`'s own truncation
/// flag (D41) feeds `PenaltyInputs::truncated`, which must lower the final
/// confidence relative to the otherwise-identical complete run, not just
/// flip a status label nothing else reacts to.
#[tokio::test]
async fn budget_exceeded() {
    let registry = ProviderRegistry::new();
    let budget = BudgetLedger::unbounded();
    let cache = ResponseCache::new();
    let events = RecordingSink::new();
    let c = ctx(&registry, &budget, &cache, &events);

    let complete = stage()
        .run(input_for(ControlFlow::Stop(StopReason::Converged)), &c)
        .await
        .unwrap();
    assert_eq!(complete.completeness, Completeness::Complete);

    let truncated = stage()
        .run(
            input_for(ControlFlow::Stop(StopReason::BudgetExhausted)),
            &c,
        )
        .await
        .unwrap();
    assert_eq!(
        truncated.completeness,
        Completeness::Truncated {
            reason: StopReason::BudgetExhausted,
            missing_stages: vec![]
        }
    );

    let truncation_entry = truncated
        .record
        .confidence
        .penalties
        .iter()
        .find(|p| p.name == "truncation")
        .expect("a truncation penalty entry must be present in the explained breakdown");
    assert!(
        truncation_entry.contribution < 0.0,
        "a budget-exhausted stop must actually apply the truncation penalty, not just record the reason"
    );
    assert!(
        truncated.record.confidence.total < complete.record.confidence.total,
        "the truncated decision's confidence must read strictly lower than the otherwise-identical complete run"
    );
}
