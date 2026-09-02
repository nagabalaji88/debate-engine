//! The `StageGraph` executor L1 is what actually builds it (PLAN_DEVIATIONS.md
//! D39/D42) — every G1–G9 stage wired together in ARCHITECTURE §5's own
//! pipeline order, including the one controlled loop
//! (`challenge.plan → challenge.run → rebuttal.run → controller.decide`,
//! INTERFACES §11). `disputes.rank` runs exactly once, before the loop;
//! `controller.decide` re-resolves the graph itself each iteration via the
//! same `resolve_and_rank` `disputes.rank` uses (D39) — this executor never
//! re-invokes the `DisputesRank` stage a second time.

use crate::run_handle::RunHandle;
use arbiter_core::{ModelId, ProviderId, RunId};
use arbiter_kernel::bounds::{self, Bounds};
use arbiter_kernel::budget::BudgetLedger;
use arbiter_kernel::cache::ResponseCache;
use arbiter_kernel::ids::StageName;
use arbiter_kernel::prompt::{PromptPack, PromptTemplate};
use arbiter_kernel::stage::{
    CancellationToken, ControlFlow, DeterministicRng, ProviderRegistry, Stage, StageContext,
};
use arbiter_kernel::stages::challenge_plan::ChallengePlan;
use arbiter_kernel::stages::challenge_run::ChallengeRun;
use arbiter_kernel::stages::claims_extract::ClaimsExtract;
use arbiter_kernel::stages::claims_normalize::ClaimsNormalize;
use arbiter_kernel::stages::controller_decide::{ControllerDecide, DecideInput};
use arbiter_kernel::stages::decision_synthesize::{
    DecisionSynthesize, SynthesizeInput, SynthesizedDecision,
};
use arbiter_kernel::stages::disputes_rank::{DisputesRank, RankInput};
use arbiter_kernel::stages::judge_evaluate::{JudgeEvaluate, JudgeInput};
use arbiter_kernel::stages::options_cluster::{ClusterInput, OptionsCluster};
use arbiter_kernel::stages::positions_generate::{PositionsGenerate, Question};
use arbiter_kernel::stages::rebuttal_run::{RebuttalOutcome, RebuttalRun};
use arbiter_kernel::stages::relations_analyze::{AnalyzeInput, RelationsAnalyze};
use arbiter_kernel::store::Cost;
use std::time::{Duration, Instant};

/// Flat per-call cost estimates. No real per-token pricing table exists
/// anywhere in this workspace (P4, deferred) — every stage's own precedent
/// (D31 and onward) is a config-supplied flat estimate, never invented
/// per-provider math; this executor's own literals follow the same
/// precedent rather than a new one (PLAN_DEVIATIONS.md D42).
const CALL_COST: Cost = Cost(0.01);
const EXCHANGE_COST: Cost = Cost(0.02);
const JUDGE_RESERVATION: Cost = Cost(0.05);
const MAX_CHALLENGES_PER_MODEL: usize = 2;
const MAX_PARALLELISM: usize = 4;

pub struct PipelineConfig {
    pub run_id: RunId,
    pub question: String,
    pub panel: Vec<(ModelId, ProviderId)>,
    pub judges: Vec<(ModelId, ProviderId)>,
    pub bounds: Bounds,
    pub policy: arbiter_core::Policy,
    pub rng_seed: u64,
    pub engine_version: String,
}

fn template(pack: &PromptPack, stage: &str) -> PromptTemplate {
    pack.template(&StageName::new(stage))
        .cloned()
        .unwrap_or_else(|| panic!("prompt pack is missing required template '{stage}'"))
}

fn take_error(handle: &RunHandle) -> anyhow::Result<()> {
    if let Some(e) = handle.take_error() {
        anyhow::bail!("store write failed: {e}");
    }
    Ok(())
}

/// `budget`/`cache` are the caller's, not constructed here — `arbiter run`
/// (L1) passes fresh ones, but `resume`/`replay` (L3) need to seed both from
/// what a prior process already persisted (a `ResponseCache` rehydrated from
/// `cache_entries`, a `BudgetLedger` capped at what is actually left of the
/// hard cap) before a single stage runs, which only the caller has read
/// (PLAN_DEVIATIONS.md D44). Threading them through is the only change from
/// this function's own L1 shape -- every stage call below is untouched.
pub async fn run_pipeline(
    cfg: &PipelineConfig,
    pack: &PromptPack,
    providers: &ProviderRegistry,
    handle: &RunHandle,
    budget: &BudgetLedger,
    cache: &ResponseCache,
) -> anyhow::Result<SynthesizedDecision> {
    let sink = handle.sink();
    let deadline = Instant::now() + Duration::from_secs(cfg.bounds.max_wall_time_secs);

    let ctx = |round: u32| StageContext {
        providers,
        budget,
        events: &sink,
        cache,
        deadline,
        cancel: CancellationToken::new(),
        round,
        rng: DeterministicRng::seeded(cfg.rng_seed),
    };

    let cfg_ = &cfg.policy.config;

    // positions.generate
    let positions_stage = PositionsGenerate::new(
        cfg.panel.clone(),
        template(pack, "positions.generate"),
        CALL_COST,
        MAX_PARALLELISM,
    );
    let positions = positions_stage
        .run(
            Question {
                text: cfg.question.clone(),
            },
            &ctx(1),
        )
        .await
        .map_err(|e| anyhow::anyhow!("positions.generate: {e}"))?;
    take_error(handle)?;
    handle.put_artifact(&positions)?;

    // claims.extract
    let repair_cap = bounds::repair_budget(
        cfg.bounds.usable_cap(1),
        bounds::DEFAULT_REPAIR_BUDGET_FRACTION,
    );
    let repair_model = cfg
        .panel
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("panel must have at least one member"))?;
    let claims_extract_stage = ClaimsExtract::new(
        template(pack, "claims.extract"),
        template(pack, "claims.repair"),
        repair_model.clone(),
        CALL_COST,
        repair_cap,
        MAX_PARALLELISM,
    );
    let extracted = claims_extract_stage
        .run(positions.clone(), &ctx(1))
        .await
        .map_err(|e| anyhow::anyhow!("claims.extract: {e}"))?;
    take_error(handle)?;
    handle.put_artifact(&extracted)?;

    // claims.normalize
    let normalize_stage = ClaimsNormalize::new(
        template(pack, "claims.group"),
        repair_model.clone(),
        CALL_COST,
    );
    let normalized = normalize_stage
        .run(extracted, &ctx(1))
        .await
        .map_err(|e| anyhow::anyhow!("claims.normalize: {e}"))?;
    take_error(handle)?;
    handle.put_artifact(&normalized)?;

    // options.cluster
    let cluster_stage = OptionsCluster::new(
        template(pack, "options.cluster"),
        template(pack, "options.attach"),
        repair_model.clone(),
        CALL_COST,
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
        .map_err(|e| anyhow::anyhow!("options.cluster: {e}"))?;
    take_error(handle)?;
    handle.put_artifact(&clustered)?;

    // relations.analyze
    let relations_stage = RelationsAnalyze::new(
        template(pack, "relations.classify"),
        repair_model,
        CALL_COST,
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
        .map_err(|e| anyhow::anyhow!("relations.analyze: {e}"))?;
    take_error(handle)?;
    handle.put_artifact(&relations)?;

    // disputes.rank -- runs exactly once, before the round loop (D39).
    let disputes_stage = DisputesRank::new(
        cfg_.weights.clone(),
        cfg_.graph.clone(),
        cfg_.thresholds.clone(),
        cfg_.attachment.clone(),
        cfg_.dispute.clone(),
        EXCHANGE_COST,
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
        .map_err(|e| anyhow::anyhow!("disputes.rank: {e}"))?;
    take_error(handle)?;
    handle.put_artifact(&resolved)?;

    // The round loop: challenge.plan -> challenge.run -> rebuttal.run ->
    // controller.decide, re-entering while ControlFlow::Continue.
    let challenge_plan_stage = ChallengePlan::new(
        cfg.bounds.max_rounds,
        EXCHANGE_COST,
        JUDGE_RESERVATION,
        MAX_CHALLENGES_PER_MODEL,
    );
    let challenge_run_stage = ChallengeRun::new(
        template(pack, "challenge.issue"),
        CALL_COST,
        MAX_PARALLELISM,
    );
    let rebuttal_run_stage = RebuttalRun::new(
        template(pack, "rebuttal.respond"),
        CALL_COST,
        MAX_PARALLELISM,
    );
    let controller_stage = ControllerDecide::new(
        cfg_.weights.clone(),
        cfg_.graph.clone(),
        cfg_.thresholds.clone(),
        cfg_.attachment.clone(),
        cfg_.dispute.clone(),
        EXCHANGE_COST,
        cfg.bounds.max_rounds,
        bounds::DEFAULT_CONVERGED_MARGIN_FACTOR,
        bounds::DEFAULT_MIN_NEW_CLAIMS,
        bounds::DEFAULT_MIN_STANDING_DELTA,
    );

    let mut round = 1u32;
    let mut all_exchanges: Vec<RebuttalOutcome> = Vec::new();
    let final_control;
    // A defensive belt-and-suspenders cap on top of `controller.decide`'s own
    // `round >= max_rounds` check -- that check already guarantees
    // termination, this just refuses to loop past the spec's own absolute
    // ceiling under any circumstance.
    let hard_cap = bounds::HARD_ROUND_CEILING + 1;

    loop {
        if round > hard_cap {
            anyhow::bail!("round loop exceeded the hard ceiling without stopping");
        }
        let planned = challenge_plan_stage
            .run(resolved.clone(), &ctx(round))
            .await
            .map_err(|e| anyhow::anyhow!("challenge.plan: {e}"))?;
        take_error(handle)?;
        handle.put_artifact(&planned)?;

        let issued = challenge_run_stage
            .run(planned, &ctx(round))
            .await
            .map_err(|e| anyhow::anyhow!("challenge.run: {e}"))?;
        take_error(handle)?;
        handle.put_artifact(&issued)?;

        let rebuttals = rebuttal_run_stage
            .run(issued, &ctx(round))
            .await
            .map_err(|e| anyhow::anyhow!("rebuttal.run: {e}"))?;
        take_error(handle)?;
        handle.put_artifact(&rebuttals)?;
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
            .map_err(|e| anyhow::anyhow!("controller.decide: {e}"))?;
        take_error(handle)?;
        handle.put_artifact(&decision)?;

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

    // judge.evaluate
    let judge_stage = JudgeEvaluate::new(
        template(pack, "judge.evaluate"),
        cfg.judges.clone(),
        CALL_COST,
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
        .map_err(|e| anyhow::anyhow!("judge.evaluate: {e}"))?;
    take_error(handle)?;
    handle.put_artifact(&judged)?;

    // decision.synthesize
    let synthesize_stage = DecisionSynthesize::new(
        cfg_.weights.clone(),
        cfg_.graph.clone(),
        cfg_.thresholds.clone(),
        cfg_.attachment.clone(),
        cfg_.confidence.clone(),
        cfg.policy.version.clone(),
        cfg.engine_version.clone(),
    );
    let synthesized = synthesize_stage
        .run(
            SynthesizeInput {
                run_id: cfg.run_id.clone(),
                question: cfg.question.clone(),
                judged,
                final_control,
            },
            &ctx(round),
        )
        .await
        .map_err(|e| anyhow::anyhow!("decision.synthesize: {e}"))?;
    take_error(handle)?;
    handle.put_artifact(&synthesized)?;

    Ok(synthesized)
}
