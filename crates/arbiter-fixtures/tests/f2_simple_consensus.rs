//! F2's own fixture — `simple_consensus`, ARCHITECTURE §18's CI suite:
//! "happy path, all confidence terms populated."
//!
//! The one fixture that genuinely needs the full pipeline, wired directly
//! against `arbiter-kernel`'s stages the same way `arbiter-cli::orchestrator`
//! does — `arbiter-fixtures` cannot depend on `arbiter-cli` (the dependency
//! rule), so this is F1's harness design in its fullest form: every stage
//! constructed and run in ARCHITECTURE §5's own pipeline order, using only
//! kernel/core/providers. A single-model, single-judge, standard-depth
//! (`max_rounds` 1) scenario keeps the scripting tractable while still
//! exercising every stage for real: `positions.generate` → `claims.extract`
//! → `claims.normalize` → `options.cluster` → `relations.analyze` →
//! `disputes.rank` → the round loop (`challenge.plan` → `challenge.run` →
//! `rebuttal.run` → `controller.decide`, exiting on `RoundLimit` at round 1)
//! → `judge.evaluate` → `decision.synthesize`.

use arbiter_core::{ModelId, ProviderId};
use arbiter_fixtures::harness::RecordingSink;
use arbiter_fixtures::harness::{policy, template};
use arbiter_kernel::budget::BudgetLedger;
use arbiter_kernel::cache::ResponseCache;
use arbiter_kernel::event::EventType;
use arbiter_kernel::stage::{
    CancellationToken, ControlFlow, DeterministicRng, ProviderRegistry, Stage, StageContext,
    StopReason,
};
use arbiter_kernel::stages::challenge_plan::ChallengePlan;
use arbiter_kernel::stages::challenge_run::ChallengeRun;
use arbiter_kernel::stages::claims_extract::ClaimsExtract;
use arbiter_kernel::stages::claims_normalize::ClaimsNormalize;
use arbiter_kernel::stages::controller_decide::{ControllerDecide, DecideInput};
use arbiter_kernel::stages::decision_synthesize::{
    Completeness, DecisionSynthesize, SynthesizeInput,
};
use arbiter_kernel::stages::disputes_rank::{DisputesRank, RankInput};
use arbiter_kernel::stages::judge_evaluate::{JudgeEvaluate, JudgeInput};
use arbiter_kernel::stages::options_cluster::{ClusterInput, OptionsCluster};
use arbiter_kernel::stages::positions_generate::{PositionsGenerate, Question};
use arbiter_kernel::stages::rebuttal_run::{RebuttalOutcome, RebuttalRun};
use arbiter_kernel::stages::relations_analyze::{AnalyzeInput, RelationsAnalyze};
use arbiter_kernel::store::Cost;
use arbiter_providers::mock::MockProvider;
use std::time::{Duration, Instant};

fn mock(id: &str) -> MockProvider {
    MockProvider::new(
        ProviderId::new(id),
        arbiter_kernel::provider::ProviderCapabilities {
            structured_output: false,
            streaming: false,
            idempotency: None,
        },
    )
}

#[tokio::test]
async fn simple_consensus() {
    let panel_provider = mock("panel-mock");
    // positions.generate
    panel_provider.script_text("We should adopt a modular monolith. Our team has 8 developers.");
    // claims.extract
    panel_provider.script_text(
        serde_json::json!([{"text": "our team has 8 developers", "kind": "fact", "grounding": {"quote": "Our team has 8 developers"}}])
            .to_string(),
    );
    // options.cluster: cluster call, then attach call
    panel_provider.script_text(
        serde_json::json!([{"members": ["#1"], "label": "Adopt a modular monolith", "confidence": 0.9}]).to_string(),
    );
    panel_provider.script_text(serde_json::json!([]).to_string());

    let judge_provider = mock("judge-mock");
    judge_provider.script_text(
        serde_json::json!([
            {"pseudonym": "A", "factual_correctness": 0.9, "logical_reasoning": 0.85,
             "evidence_quality": 0.8, "problem_relevance": 0.9, "assumption_quality": 0.75,
             "counterargument_handling": 0.7, "risk_awareness": 0.8, "practicality": 0.85,
             "clarity": 0.9}
        ])
        .to_string(),
    );

    let mut providers = ProviderRegistry::new();
    providers.register(Box::new(panel_provider));
    providers.register(Box::new(judge_provider));
    let budget = BudgetLedger::unbounded();
    let cache = ResponseCache::new();
    let events = RecordingSink::new();
    let ctx = |round: u32| StageContext {
        providers: &providers,
        budget: &budget,
        events: &events,
        cache: &cache,
        deadline: Instant::now() + Duration::from_secs(60),
        cancel: CancellationToken::new(),
        round,
        rng: DeterministicRng::seeded(1),
    };

    let panel_model = (ModelId::new("model-a"), ProviderId::new("panel-mock"));
    let cfg = policy().config;

    // positions.generate
    let positions_stage = PositionsGenerate::new(
        vec![panel_model.clone()],
        template("positions.generate", "{{question}}", &["question"]),
        Cost(0.01),
        4,
    );
    let positions = positions_stage
        .run(
            Question {
                text: "Should we adopt a modular monolith or microservices?".to_string(),
            },
            &ctx(1),
        )
        .await
        .unwrap();
    assert_eq!(positions.0.len(), 1);

    // claims.extract
    let claims_extract_stage = ClaimsExtract::new(
        template("claims.extract", "{{position_text}}", &["position_text"]),
        template(
            "claims.repair",
            "{{position_text}} {{failed_claims}}",
            &["position_text", "failed_claims"],
        ),
        panel_model.clone(),
        Cost(0.01),
        Cost(0.0),
        4,
    );
    let extracted = claims_extract_stage
        .run(positions.clone(), &ctx(1))
        .await
        .unwrap();
    assert_eq!(extracted.0.len(), 1);

    // claims.normalize (single claim: short-circuits with no provider call)
    let normalize_stage = ClaimsNormalize::new(
        template("claims.group", "{{claims}}", &["claims"]),
        panel_model.clone(),
        Cost(0.01),
    );
    let normalized = normalize_stage.run(extracted, &ctx(1)).await.unwrap();
    assert_eq!(normalized.0.len(), 1);

    // options.cluster
    let cluster_stage = OptionsCluster::new(
        template("options.cluster", "{{positions}}", &["positions"]),
        template(
            "options.attach",
            "{{claims}} {{options}}",
            &["claims", "options"],
        ),
        panel_model.clone(),
        Cost(0.01),
    );
    let clustered = cluster_stage
        .run(
            ClusterInput {
                positions: positions.clone(),
                claims: normalized.clone(),
            },
            &ctx(1),
        )
        .await
        .unwrap();
    assert_eq!(clustered.options.len(), 1);

    // relations.analyze (single claim: short-circuits with no provider call)
    let relations_stage = RelationsAnalyze::new(
        template("relations.classify", "{{claims}}", &["claims"]),
        panel_model.clone(),
        Cost(0.01),
    );
    let relations = relations_stage
        .run(
            AnalyzeInput {
                claims: normalized.clone(),
                options: clustered.clone(),
            },
            &ctx(1),
        )
        .await
        .unwrap();

    // disputes.rank -- runs once, before the round loop, calls no model.
    let disputes_stage = DisputesRank::new(
        cfg.weights.clone(),
        cfg.graph.clone(),
        cfg.thresholds.clone(),
        cfg.attachment.clone(),
        cfg.dispute.clone(),
        Cost(0.02),
    );
    let mut resolved = disputes_stage
        .run(
            RankInput {
                claims: normalized,
                relations,
                options: clustered,
            },
            &ctx(1),
        )
        .await
        .unwrap();

    // The round loop -- standard depth (max_rounds = 1) exits on RoundLimit
    // at round 1 without needing any challenge/rebuttal content.
    let challenge_plan_stage = ChallengePlan::new(1, Cost(0.02), Cost(0.05), 2);
    let challenge_run_stage = ChallengeRun::new(
        template("challenge.issue", "{{claim_text}}", &["claim_text"]),
        Cost(0.01),
        4,
    );
    let rebuttal_run_stage = RebuttalRun::new(
        template(
            "rebuttal.respond",
            "{{challenge_text}}",
            &["challenge_text"],
        ),
        Cost(0.01),
        4,
    );
    let controller_stage = ControllerDecide::new(
        cfg.weights.clone(),
        cfg.graph.clone(),
        cfg.thresholds.clone(),
        cfg.attachment.clone(),
        cfg.dispute.clone(),
        Cost(0.02),
        1,
        1.2,
        1,
        0.05,
    );

    let mut round = 1u32;
    let mut all_exchanges: Vec<RebuttalOutcome> = Vec::new();
    let final_control;
    loop {
        let planned = challenge_plan_stage
            .run(resolved.clone(), &ctx(round))
            .await
            .unwrap();
        let issued = challenge_run_stage.run(planned, &ctx(round)).await.unwrap();
        let rebuttals = rebuttal_run_stage.run(issued, &ctx(round)).await.unwrap();
        all_exchanges.extend(rebuttals.outcomes.clone());

        let decision = controller_stage
            .run(
                DecideInput {
                    rebuttals,
                    previous: resolved.clone(),
                },
                &ctx(round),
            )
            .await
            .unwrap();
        match decision.control {
            ControlFlow::Continue {
                round: next_round, ..
            } => {
                resolved = decision.resolved;
                round = next_round;
            }
            ControlFlow::Stop(reason) => {
                resolved = decision.resolved;
                final_control = ControlFlow::Stop(reason);
                break;
            }
        }
    }
    assert_eq!(final_control, ControlFlow::Stop(StopReason::RoundLimit));

    // judge.evaluate
    let judge_stage = JudgeEvaluate::new(
        template("judge.evaluate", "{{dossiers}}", &["dossiers"]),
        vec![(ModelId::new("judge-1"), ProviderId::new("judge-mock"))],
        Cost(0.10),
    );
    let judged = judge_stage
        .run(
            JudgeInput {
                positions,
                resolved,
                exchanges: all_exchanges,
            },
            &ctx(round),
        )
        .await
        .unwrap();
    assert_eq!(
        judged.scores_by_model.len(),
        1,
        "the single panel model must be scored by the judge"
    );

    // decision.synthesize -- calls no model.
    let synthesize_stage = DecisionSynthesize::new(
        cfg.weights.clone(),
        cfg.graph.clone(),
        cfg.thresholds.clone(),
        cfg.attachment.clone(),
        cfg.confidence.clone(),
        arbiter_core::PolicyVersion::new("argument-v1"),
        "0.1.0",
    );
    let synthesized = synthesize_stage
        .run(
            SynthesizeInput {
                run_id: arbiter_core::RunId::new("run_simple_consensus"),
                question: "Should we adopt a modular monolith or microservices?".to_string(),
                judged,
                final_control,
            },
            &ctx(round),
        )
        .await
        .unwrap();

    assert_eq!(
        synthesized.completeness,
        Completeness::Complete,
        "a clean RoundLimit stop must read Complete, not Truncated"
    );
    assert!(
        synthesized.record.recommendation.is_some(),
        "a single well-evidenced, undisputed option must earn a recommendation"
    );
    assert!(
        synthesized.record.confidence.total > 0.0,
        "confidence must be a real, positive number on the happy path"
    );
    assert_eq!(
        synthesized.record.confidence.dimensions.len(),
        3,
        "all three confidence dimensions must be populated"
    );
    assert_eq!(
        synthesized.record.confidence.penalties.len(),
        5,
        "all five penalty terms must be populated, even where zero"
    );

    assert!(
        events.count(EventType::StageCompleted) >= 8,
        "every stage in the pipeline must have actually run and completed"
    );
}
