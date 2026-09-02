//! `decision.synthesize` (ARCHITECTURE §5's own pipeline table: "runs the
//! decision core" · "calls no model"). Wires C1–C8's already-built, pure
//! decision engine to real pipeline artifacts for the first time: the final
//! fixpoint (now with real judge scores), outcome classification,
//! confidence, and the assembled `DecisionRecord`.

use super::controller_decide::control_flow_json;
use super::disputes_rank::{ResolveParams, resolve_and_rank};
use super::judge_evaluate::JudgeEvaluation;
use crate::event::EventType;
use crate::ids::StageName;
use crate::stage::{
    ControlFlow, CostEstimate, FailurePolicy, Key, Parallelism, RunContext, Stage, StageContext,
    StageError, StopReason, idempotency_key,
};
use crate::store::Artifact;
use arbiter_core::config::{
    AttachmentParams, ConfidenceWeights, DisputeWeights, GraphParams, Thresholds, Weights,
};
use arbiter_core::decision::confidence::{PenaltyInputs, confidence as compute_confidence};
use arbiter_core::decision::outcome::{self, OutcomeInputs};
use arbiter_core::decision::{attachment, controller, record, synthesize};
use arbiter_core::{
    AttachSource, CanonicalClaim, ClaimId, ModelId, Polarity, PolicyVersion, RunId, Scorecard,
};
use std::collections::{BTreeMap, BTreeSet};

/// Mirrors INTERFACES §9's `Completeness`, but with `arbiter-kernel`'s own
/// `StopReason`/`StageName` rather than `arbiter-core`'s — that pure crate
/// cannot depend on kernel types (D1), which is exactly why `record.rs`'s
/// own D18 note left this field out of `DecisionRecord` for G9 to add
/// *somewhere*. It lands here, on the kernel-level wrapper this stage
/// actually returns, rather than being smuggled into the core type
/// (PLAN_DEVIATIONS.md D41).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completeness {
    Complete,
    Truncated {
        reason: StopReason,
        missing_stages: Vec<StageName>,
    },
}

/// `Stage::In`: the judge's final evaluation, the original question text,
/// and the round loop's own final decision. `question`/`final_control`
/// are, like `judge.evaluate`'s `exchanges`, the eventual executor's to
/// supply (D39/D40's own precedent) — nothing upstream of this stage
/// carries the question text or the terminal `ControlFlow` forward as part
/// of an artifact chain yet.
#[derive(Debug, Clone, PartialEq)]
pub struct SynthesizeInput {
    pub run_id: RunId,
    pub question: String,
    pub judged: JudgeEvaluation,
    pub final_control: ControlFlow,
}

impl Artifact for SynthesizeInput {
    fn artifact_type(&self) -> &'static str {
        "synthesize_input.v1"
    }
    fn content_hash(&self) -> String {
        let combined = format!(
            "{}\u{1}{}\u{1}{}",
            self.run_id.as_str(),
            self.question,
            self.judged.content_hash(),
        );
        format!("blake3:{}", blake3::hash(combined.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "run_id": self.run_id.as_str(),
            "question": self.question,
            "judged": self.judged.to_json(),
            "final_control": control_flow_json(&self.final_control),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SynthesizedDecision {
    pub record: arbiter_core::DecisionRecord,
    pub completeness: Completeness,
}

impl Artifact for SynthesizedDecision {
    fn artifact_type(&self) -> &'static str {
        "synthesized_decision.v1"
    }
    fn content_hash(&self) -> String {
        let record_json = serde_json::to_string(&self.record).expect("record serializes");
        let completeness_json = match &self.completeness {
            Completeness::Complete => "complete".to_string(),
            Completeness::Truncated {
                reason,
                missing_stages,
            } => format!(
                "truncated:{reason:?}:{}",
                missing_stages
                    .iter()
                    .map(|s| s.as_str().to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        };
        let combined = format!("{record_json}\u{1}{completeness_json}");
        format!("blake3:{}", blake3::hash(combined.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        let completeness = match &self.completeness {
            Completeness::Complete => serde_json::json!({"status": "complete"}),
            Completeness::Truncated {
                reason,
                missing_stages,
            } => serde_json::json!({
                "status": "truncated",
                "reason": format!("{reason:?}"),
                "missing_stages": missing_stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            }),
        };
        serde_json::json!({
            "record": self.record,
            "completeness": completeness,
        })
    }
}

#[derive(Debug)]
pub struct DecisionSynthesize {
    weights: Weights,
    graph: GraphParams,
    thresholds: Thresholds,
    attachment_params: AttachmentParams,
    confidence_weights: ConfidenceWeights,
    policy_version: PolicyVersion,
    engine_version: String,
}

impl DecisionSynthesize {
    pub fn new(
        weights: Weights,
        graph: GraphParams,
        thresholds: Thresholds,
        attachment_params: AttachmentParams,
        confidence_weights: ConfidenceWeights,
        policy_version: PolicyVersion,
        engine_version: impl Into<String>,
    ) -> Self {
        Self {
            weights,
            graph,
            thresholds,
            attachment_params,
            confidence_weights,
            policy_version,
            engine_version: engine_version.into(),
        }
    }
}

/// "estimated tokens for the exchange ÷ remaining budget" has no meaning
/// here -- this stage spends nothing and ranks no disputes, it only reuses
/// `resolve_and_rank`'s fixpoint/propagation/classification/flips
/// machinery. `0.0` is not a real resolution-cost estimate, just an unused
/// input `dispute_priority` never gets read for.
const UNUSED_RESOLUTION_COST: f64 = 0.0;

impl Stage for DecisionSynthesize {
    type In = SynthesizeInput;
    type Out = SynthesizedDecision;

    fn name(&self) -> StageName {
        StageName::new("decision.synthesize")
    }

    fn parallelism(&self) -> Parallelism {
        Parallelism::Serial
    }

    fn failure_policy(&self) -> FailurePolicy {
        FailurePolicy::Fatal
    }

    fn idempotency_key(&self, input: &Self::In, ctx: &RunContext) -> Key {
        idempotency_key(&self.name(), ctx, &[input.content_hash()])
    }

    fn cost_estimate(&self, _input: &Self::In) -> CostEstimate {
        CostEstimate {
            calls: 0,
            tokens: 0,
            cost: crate::store::Cost(0.0),
        }
    }

    async fn run(&self, input: Self::In, ctx: &StageContext<'_>) -> Result<Self::Out, StageError> {
        let stage_name = self.name();
        let resolved_graph = &input.judged.resolved;
        ctx.events.emit(
            EventType::StageStarted,
            &stage_name,
            serde_json::json!({"claims": resolved_graph.claims.0.len()}),
        );

        let claims: &[CanonicalClaim] = &resolved_graph.claims.0;
        let relations = &resolved_graph.relations.0;
        let claims_by_id: BTreeMap<ClaimId, &CanonicalClaim> =
            claims.iter().map(|c| (c.id.clone(), c)).collect();

        let params = ResolveParams {
            weights: &self.weights,
            graph: &self.graph,
            thresholds: &self.thresholds,
            attachment_params: &self.attachment_params,
            dispute_weights: &DisputeWeights::default(),
        };
        // The only caller of `resolve_and_rank` with real judge scores to
        // give it (D41) -- every earlier stage runs before `judge.evaluate`
        // does.
        let resolved = resolve_and_rank(
            claims,
            relations,
            &resolved_graph.options,
            UNUSED_RESOLUTION_COST,
            &input.judged.scores_by_model,
            &params,
        );
        if !resolved.fixpoint_converged {
            ctx.events.emit(
                EventType::FixpointNotConverged,
                &stage_name,
                serde_json::json!({
                    "max_delta": resolved.fixpoint_max_delta,
                    "iterations": resolved.fixpoint_iterations,
                }),
            );
        }

        let live_options: Vec<_> = resolved_graph
            .options
            .options
            .iter()
            .filter(|o| !o.retired)
            .cloned()
            .collect();
        let scores = attachment::score_options(
            &live_options,
            &resolved.propagated_matrix,
            &resolved.standing,
        );
        let top1_id = synthesize::ranked(&scores).first().map(|o| o.id.clone());

        let evidence_mass = top1_id
            .as_ref()
            .map(|id| {
                let decisive = synthesize::decisive_claims(id, &resolved.propagated_matrix);
                synthesize::mean_standing(&decisive, &resolved.standing)
            })
            .unwrap_or(0.0);
        let live_dissent_against_top1 = top1_id
            .as_ref()
            .map(|id| {
                controller::has_live_dissent_against(
                    id,
                    &resolved.propagated_matrix,
                    &resolved.standing,
                    self.thresholds.dissent,
                )
            })
            .unwrap_or(false);

        let all_option_ids: Vec<_> = live_options.iter().map(|o| o.id.clone()).collect();
        let critical = synthesize::decision_critical_claims(
            all_option_ids.iter(),
            &resolved.propagated_matrix,
        );
        let unresolved_critical_ratio =
            synthesize::unresolved_or_disputed_ratio(&critical, &resolved.classified);
        let assumption_dependency_ratio =
            synthesize::assumption_dependency_ratio(&critical, &claims_by_id);

        // "Truncated" (INTERFACES §9): cut short by a hard bound or an
        // interruption, not a genuine adaptive stop -- `Converged`,
        // `RoundLimit` and `NoNewInformation` are all the controller
        // *deciding* the debate was done, one way or another, never an
        // external cutoff (PLAN_DEVIATIONS.md D41).
        let stop_reason = match &input.final_control {
            ControlFlow::Stop(reason) => Some(*reason),
            ControlFlow::Continue { .. } => None,
        };
        let truncated = matches!(
            stop_reason,
            Some(
                StopReason::BudgetExhausted
                    | StopReason::TokenLimit
                    | StopReason::Deadline
                    | StopReason::Cancelled
                    | StopReason::ProviderFailure
            )
        );

        let outcome_inputs = OutcomeInputs {
            evidence_mass,
            unresolved_critical_ratio,
            live_dissent_against_top1,
            truncated,
        };
        let outcome = outcome::classify(&scores, &outcome_inputs, &self.thresholds);

        // The winning option's own authoring model(s) supply the judge
        // signal `confidence()`'s `judge_score`/`judge_dispersion` read --
        // "how good was the judged case for *this* decision", not a
        // debate-wide average across every position (D41).
        let judges_for_confidence: Vec<Scorecard> = top1_id
            .as_ref()
            .and_then(|id| {
                let winning_models: BTreeSet<ModelId> = resolved_graph
                    .options
                    .direct_matrix
                    .cells
                    .iter()
                    .filter(|((_, opt), cell)| {
                        opt == id
                            && cell.source == AttachSource::Authored
                            && cell.polarity == Polarity::Supports
                    })
                    .filter_map(|((claim_id, _), _)| claims_by_id.get(claim_id))
                    .flat_map(|c| c.members.iter().map(|m| m.model.clone()))
                    .collect();
                winning_models
                    .iter()
                    .find_map(|m| input.judged.per_judge_scores.get(m))
                    .cloned()
            })
            .unwrap_or_default();

        let penalty_inputs = PenaltyInputs {
            unresolved_critical_ratio,
            assumption_dependency_ratio,
            truncated,
            fixpoint_converged: resolved.fixpoint_converged,
        };
        let breakdown = compute_confidence(
            evidence_mass,
            &scores,
            &judges_for_confidence,
            &penalty_inputs,
            &self.confidence_weights,
        );
        let explain = record::explain_confidence(&breakdown, &self.confidence_weights);

        let inputs_hash = input.content_hash();
        let decision_record = record::build(
            input.run_id.clone(),
            self.policy_version.clone(),
            input.question.clone(),
            outcome,
            scores,
            explain,
            &resolved.classified,
            &resolved.flips,
            self.engine_version.clone(),
            inputs_hash,
        );

        // No stage-execution tracking exists anywhere in this codebase yet
        // (D39's own scope note) -- `missing_stages` cannot be anything
        // other than empty without inventing that tracking here, which
        // this task's own scope does not ask for.
        let completeness = match stop_reason {
            Some(reason) if truncated => Completeness::Truncated {
                reason,
                missing_stages: Vec::new(),
            },
            _ => Completeness::Complete,
        };

        ctx.events.emit(
            EventType::DecisionSynthesized,
            &stage_name,
            serde_json::json!({
                "outcome": format!("{:?}", decision_record.outcome),
                "confidence": decision_record.confidence.total,
                "truncated": truncated,
            }),
        );
        ctx.events.emit(
            EventType::StageCompleted,
            &stage_name,
            serde_json::json!({}),
        );

        Ok(SynthesizedDecision {
            record: decision_record,
            completeness,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetLedger;
    use crate::cache::ResponseCache;
    use crate::stage::{CancellationToken, DeterministicRng, EventSink, ProviderRegistry};
    use crate::stages::claims_normalize::NormalizedClaims;
    use crate::stages::disputes_rank::RankedDisputes;
    use crate::stages::options_cluster::ClusteredOptions;
    use crate::stages::relations_analyze::AnalyzedRelations;
    use arbiter_core::decision::attachment::{Attachment, AttachmentMatrix};
    use arbiter_core::{
        ClaimLifecycle, ClaimMember, DecisionOption, EvidenceKind, Grounding, OptionId, PositionId,
        ProviderId, TextSpan,
    };
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;

    #[derive(Debug, Default)]
    struct RecordingSink {
        emitted: StdMutex<Vec<(EventType, serde_json::Value)>>,
    }
    impl EventSink for RecordingSink {
        fn emit(&self, event_type: EventType, _stage: &StageName, payload: serde_json::Value) {
            self.emitted.lock().unwrap().push((event_type, payload));
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

    fn stage_ctx<'a>(
        registry: &'a ProviderRegistry,
        budget: &'a BudgetLedger,
        cache: &'a ResponseCache,
        sink: &'a RecordingSink,
    ) -> StageContext<'a> {
        StageContext {
            providers: registry,
            budget,
            events: sink,
            cache,
            deadline: Instant::now() + std::time::Duration::from_secs(30),
            cancel: CancellationToken::new(),
            round: 1,
            rng: DeterministicRng::seeded(1),
        }
    }

    #[tokio::test]
    async fn an_undisputed_well_evidenced_option_reads_consensus_and_complete() {
        let option = DecisionOption::new(OptionId::new("opt_a"), "Do the thing");
        let claims = vec![claim("c1", "a strong supporting fact", "model-a")];
        let mut matrix = AttachmentMatrix::default();
        matrix.cells.insert(
            (ClaimId::new("c1"), option.id.clone()),
            Attachment {
                polarity: arbiter_core::Polarity::Supports,
                confidence: 1.0,
                source: AttachSource::Authored,
            },
        );

        let resolved = RankedDisputes {
            claims: NormalizedClaims(claims),
            relations: AnalyzedRelations(vec![]),
            options: ClusteredOptions {
                options: vec![option.clone()],
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

        let input = SynthesizeInput {
            run_id: RunId::new("run_1"),
            question: "what should we build?".to_string(),
            judged: JudgeEvaluation {
                resolved,
                scores_by_model,
                per_judge_scores,
            },
            final_control: ControlFlow::Stop(StopReason::Converged),
        };

        let registry = ProviderRegistry::default();
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let out = stage().run(input, &ctx).await.unwrap();
        assert_eq!(out.completeness, Completeness::Complete);
        assert_eq!(out.record.outcome, arbiter_core::Outcome::Consensus);
        assert_eq!(
            out.record.recommendation.as_ref().unwrap().option_id,
            OptionId::new("opt_a")
        );
        assert!(out.record.confidence.total > 0.0);
    }

    #[tokio::test]
    async fn no_options_at_all_is_insufficient_evidence_with_no_recommendation() {
        let resolved = RankedDisputes {
            claims: NormalizedClaims(vec![]),
            relations: AnalyzedRelations(vec![]),
            options: ClusteredOptions {
                options: vec![],
                direct_matrix: AttachmentMatrix::default(),
            },
            standing: BTreeMap::new(),
            propagated_matrix: AttachmentMatrix::default(),
            ranked: vec![],
        };
        let input = SynthesizeInput {
            run_id: RunId::new("run_1"),
            question: "q".to_string(),
            judged: JudgeEvaluation {
                resolved,
                scores_by_model: BTreeMap::new(),
                per_judge_scores: BTreeMap::new(),
            },
            final_control: ControlFlow::Stop(StopReason::RoundLimit),
        };

        let registry = ProviderRegistry::default();
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let out = stage().run(input, &ctx).await.unwrap();
        assert_eq!(
            out.record.outcome,
            arbiter_core::Outcome::InsufficientEvidence
        );
        assert!(out.record.recommendation.is_none());
    }

    #[tokio::test]
    async fn round_limit_is_not_truncated() {
        let resolved = RankedDisputes {
            claims: NormalizedClaims(vec![]),
            relations: AnalyzedRelations(vec![]),
            options: ClusteredOptions {
                options: vec![],
                direct_matrix: AttachmentMatrix::default(),
            },
            standing: BTreeMap::new(),
            propagated_matrix: AttachmentMatrix::default(),
            ranked: vec![],
        };
        let input = SynthesizeInput {
            run_id: RunId::new("run_1"),
            question: "q".to_string(),
            judged: JudgeEvaluation {
                resolved,
                scores_by_model: BTreeMap::new(),
                per_judge_scores: BTreeMap::new(),
            },
            final_control: ControlFlow::Stop(StopReason::RoundLimit),
        };

        let registry = ProviderRegistry::default();
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let out = stage().run(input, &ctx).await.unwrap();
        assert_eq!(out.completeness, Completeness::Complete);
    }

    #[tokio::test]
    async fn budget_exhausted_is_truncated_and_carries_the_reason() {
        let resolved = RankedDisputes {
            claims: NormalizedClaims(vec![]),
            relations: AnalyzedRelations(vec![]),
            options: ClusteredOptions {
                options: vec![],
                direct_matrix: AttachmentMatrix::default(),
            },
            standing: BTreeMap::new(),
            propagated_matrix: AttachmentMatrix::default(),
            ranked: vec![],
        };
        let input = SynthesizeInput {
            run_id: RunId::new("run_1"),
            question: "q".to_string(),
            judged: JudgeEvaluation {
                resolved,
                scores_by_model: BTreeMap::new(),
                per_judge_scores: BTreeMap::new(),
            },
            final_control: ControlFlow::Stop(StopReason::BudgetExhausted),
        };

        let registry = ProviderRegistry::default();
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let out = stage().run(input, &ctx).await.unwrap();
        assert_eq!(
            out.completeness,
            Completeness::Truncated {
                reason: StopReason::BudgetExhausted,
                missing_stages: vec![],
            }
        );
    }

    #[tokio::test]
    async fn a_disputed_option_with_a_live_attacker_is_not_consensus() {
        let option = DecisionOption::new(OptionId::new("opt_a"), "Do the thing");
        let claims = vec![
            claim("c1", "a supporting fact", "model-a"),
            claim("c2", "a strong rebuttal", "model-b"),
        ];
        let mut matrix = AttachmentMatrix::default();
        matrix.cells.insert(
            (ClaimId::new("c1"), option.id.clone()),
            Attachment {
                polarity: arbiter_core::Polarity::Supports,
                confidence: 1.0,
                source: AttachSource::Authored,
            },
        );
        matrix.cells.insert(
            (ClaimId::new("c2"), option.id.clone()),
            Attachment {
                polarity: arbiter_core::Polarity::Opposes,
                confidence: 1.0,
                source: AttachSource::Classified,
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
        scores_by_model.insert(ModelId::new("model-b"), flat_scorecard("model-b", 0.9));

        let input = SynthesizeInput {
            run_id: RunId::new("run_1"),
            question: "q".to_string(),
            judged: JudgeEvaluation {
                resolved,
                scores_by_model,
                per_judge_scores: BTreeMap::new(),
            },
            final_control: ControlFlow::Stop(StopReason::Converged),
        };

        let registry = ProviderRegistry::default();
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let out = stage().run(input, &ctx).await.unwrap();
        assert_ne!(out.record.outcome, arbiter_core::Outcome::Consensus);
    }
}
