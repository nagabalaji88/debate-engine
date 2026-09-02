//! `controller.decide` (ARCHITECTURE §5.5): re-resolve the argument graph
//! over this round's post-rebuttal claims, evaluate the two computed stop
//! predicates (`Converged`, `NoNewInformation`) against the hard bounds
//! (`Cancelled` / `Deadline` / `RoundLimit` / `BudgetExhausted`), and decide
//! `ControlFlow::Continue { round, focus }` or `ControlFlow::Stop(reason)`.
//! No LLM call, ever — same "no" as `disputes.rank`/`challenge.plan`
//! (ARCHITECTURE §5's own pipeline table).
//!
//! **Scope note (PLAN_DEVIATIONS.md D39):** INTERFACES §11's "the executor
//! re-instantiates the round subgraph (`challenge.plan → challenge.run →
//! rebuttal.run → controller.decide`)" describes an *executor* — something
//! that actually drives the loop by re-invoking stages — that does not exist
//! anywhere in this codebase yet (no `StageGraph` runner has been built; L1–L4
//! and the CLI wiring are what would consume `ControlFlow::Continue` to
//! literally loop). This task's own scope is the stage itself: given this
//! round's artifacts, decide correctly and produce what a future executor
//! would need to act on that decision. Building the executor is out of
//! scope here.

use super::claims_normalize::NormalizedClaims;
use super::disputes_rank::{RankedDisputes, ResolveParams, resolve_and_rank};
use super::rebuttal_run::RebuttalsRun;
use crate::event::EventType;
use crate::ids::StageName;
use crate::stage::{
    ControlFlow, CostEstimate, FailurePolicy, Key, Parallelism, RunContext, Stage, StageContext,
    StageError, StopReason, idempotency_key,
};
use crate::store::{Artifact, Cost};
use arbiter_core::config::{AttachmentParams, DisputeWeights, GraphParams, Thresholds, Weights};
use arbiter_core::decision::{attachment, controller};
use arbiter_core::{ClaimId, ClaimStanding};
use std::collections::{BTreeMap, BTreeSet};

/// Combined input: this round's post-rebuttal claims (`rebuttals`) plus the
/// previous round's already-resolved graph (`previous`), needed for the
/// `NoNewInformation` predicate's own round-over-round deltas. Same
/// multi-artifact `Stage::In` gap D34/D35/D36 already established a pattern
/// for.
#[derive(Debug, Clone, PartialEq)]
pub struct DecideInput {
    pub rebuttals: RebuttalsRun,
    pub previous: RankedDisputes,
}

impl Artifact for DecideInput {
    fn artifact_type(&self) -> &'static str {
        "decide_input.v1"
    }
    fn content_hash(&self) -> String {
        let combined = format!(
            "{}\u{1}{}",
            self.rebuttals.content_hash(),
            self.previous.content_hash()
        );
        format!("blake3:{}", blake3::hash(combined.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "rebuttals": self.rebuttals.to_json(),
            "previous": self.previous.to_json(),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ControllerDecision {
    pub control: ControlFlow,
    /// This round's freshly resolved graph — `challenge.plan`'s input on a
    /// `Continue`, or what `judge.evaluate`/`decision.synthesize` reads on a
    /// `Stop`.
    pub resolved: RankedDisputes,
    /// The record §5.5 asks for even when these predicates don't gate
    /// anything (standard depth, `RoundLimit` wins regardless).
    pub converged: bool,
    pub no_new_information: bool,
    pub new_claim_count: usize,
    pub max_standing_delta: f64,
}

pub(crate) fn control_flow_json(control: &ControlFlow) -> serde_json::Value {
    match control {
        ControlFlow::Continue { round, focus } => serde_json::json!({
            "kind": "continue",
            "round": round,
            "focus": focus.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        }),
        ControlFlow::Stop(reason) => serde_json::json!({
            "kind": "stop",
            "reason": format!("{reason:?}"),
        }),
    }
}

impl Artifact for ControllerDecision {
    fn artifact_type(&self) -> &'static str {
        "controller_decision.v1"
    }
    fn content_hash(&self) -> String {
        let combined = format!(
            "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
            control_flow_json(&self.control),
            self.resolved.content_hash(),
            self.converged,
            self.no_new_information,
            self.max_standing_delta,
        );
        format!("blake3:{}", blake3::hash(combined.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "control": control_flow_json(&self.control),
            "resolved": self.resolved.to_json(),
            "converged": self.converged,
            "no_new_information": self.no_new_information,
            "new_claim_count": self.new_claim_count,
            "max_standing_delta": self.max_standing_delta,
        })
    }
}

#[derive(Debug)]
pub struct ControllerDecide {
    weights: Weights,
    graph: GraphParams,
    thresholds: Thresholds,
    attachment_params: AttachmentParams,
    dispute_weights: DisputeWeights,
    estimated_cost_per_exchange: Cost,
    max_rounds: u32,
    converged_margin_factor: f64,
    min_new_claims: usize,
    min_standing_delta: f64,
}

impl ControllerDecide {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        weights: Weights,
        graph: GraphParams,
        thresholds: Thresholds,
        attachment_params: AttachmentParams,
        dispute_weights: DisputeWeights,
        estimated_cost_per_exchange: Cost,
        max_rounds: u32,
        converged_margin_factor: f64,
        min_new_claims: usize,
        min_standing_delta: f64,
    ) -> Self {
        Self {
            weights,
            graph,
            thresholds,
            attachment_params,
            dispute_weights,
            estimated_cost_per_exchange,
            max_rounds,
            converged_margin_factor,
            min_new_claims,
            min_standing_delta,
        }
    }
}

impl Stage for ControllerDecide {
    type In = DecideInput;
    type Out = ControllerDecision;

    fn name(&self) -> StageName {
        StageName::new("controller.decide")
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
            cost: Cost(0.0),
        }
    }

    async fn run(&self, input: Self::In, ctx: &StageContext<'_>) -> Result<Self::Out, StageError> {
        let stage_name = self.name();
        let round_input = input.rebuttals.next_round_input;
        ctx.events.emit(
            EventType::StageStarted,
            &stage_name,
            serde_json::json!({"claims": round_input.claims.0.len()}),
        );

        let resolution_cost = match ctx.budget.remaining() {
            None => 0.0,
            Some(remaining) if remaining.0 > 0.0 => {
                (self.estimated_cost_per_exchange.0 / remaining.0).clamp(0.0, 1.0)
            }
            Some(_) => 1.0,
        };
        let params = ResolveParams {
            weights: &self.weights,
            graph: &self.graph,
            thresholds: &self.thresholds,
            attachment_params: &self.attachment_params,
            dispute_weights: &self.dispute_weights,
        };
        // No judge has run yet at this point in the pipeline either --
        // `judge.evaluate` only runs once the round loop exits.
        let resolved = resolve_and_rank(
            &round_input.claims.0,
            &round_input.relations.0,
            &round_input.options,
            resolution_cost,
            &BTreeMap::new(),
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

        // NoNewInformation: new claim ids since the previous round, and the
        // largest per-claim standing swing among claims present in both.
        let previous_ids: BTreeSet<ClaimId> = input
            .previous
            .claims
            .0
            .iter()
            .map(|c| c.id.clone())
            .collect();
        let new_claim_count = round_input
            .claims
            .0
            .iter()
            .filter(|c| !previous_ids.contains(&c.id))
            .count();
        let max_standing_delta = round_input
            .claims
            .0
            .iter()
            .filter_map(|c| {
                let prev = input.previous.standing.get(&c.id)?;
                let now = resolved.standing.get(&c.id)?;
                Some((now - prev).abs())
            })
            .fold(0.0_f64, f64::max);
        let no_new_information = controller::no_new_information(
            new_claim_count,
            max_standing_delta,
            self.min_new_claims,
            self.min_standing_delta,
        );

        // Converged: top option's dissent, margin, and unresolved triggers.
        let live_options: Vec<_> = round_input
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
        let unresolved_claims: Vec<ClaimId> = resolved
            .classified
            .iter()
            .filter(|(_, s)| **s == ClaimStanding::Unresolved)
            .map(|(id, _)| id.clone())
            .collect();
        let is_converged = controller::converged(
            &scores,
            &resolved.propagated_matrix,
            &resolved.standing,
            &unresolved_claims,
            &resolved.flips,
            self.thresholds.dissent,
            self.thresholds.min_margin,
            self.converged_margin_factor,
        );

        // Hard bounds first (never exceedable), then the computed
        // predicates -- "at standard depth the controller exits on
        // RoundLimit, by construction" (§5.5): both predicates above are
        // still computed and recorded regardless of which branch wins.
        let control = if ctx.cancel.is_cancelled() {
            ControlFlow::Stop(StopReason::Cancelled)
        } else if std::time::Instant::now() >= ctx.deadline {
            ControlFlow::Stop(StopReason::Deadline)
        } else if ctx.round >= self.max_rounds {
            ControlFlow::Stop(StopReason::RoundLimit)
        } else if matches!(ctx.budget.remaining(), Some(remaining) if remaining.0 <= 0.0) {
            ControlFlow::Stop(StopReason::BudgetExhausted)
        } else if is_converged {
            ControlFlow::Stop(StopReason::Converged)
        } else if no_new_information {
            ControlFlow::Stop(StopReason::NoNewInformation)
        } else {
            ControlFlow::Continue {
                round: ctx.round + 1,
                focus: resolved.ranked.iter().map(|r| r.claim_id.clone()).collect(),
            }
        };

        ctx.events.emit(
            EventType::ControllerDecided,
            &stage_name,
            serde_json::json!({
                "control": control_flow_json(&control),
                "converged": is_converged,
                "no_new_information": no_new_information,
                "new_claim_count": new_claim_count,
                "max_standing_delta": max_standing_delta,
            }),
        );

        let resolved_out = RankedDisputes {
            claims: NormalizedClaims(round_input.claims.0),
            relations: round_input.relations,
            options: round_input.options,
            standing: resolved.standing,
            propagated_matrix: resolved.propagated_matrix,
            ranked: resolved.ranked,
        };

        ctx.events.emit(
            EventType::StageCompleted,
            &stage_name,
            serde_json::json!({"stopped": matches!(control, ControlFlow::Stop(_))}),
        );

        Ok(ControllerDecision {
            control,
            resolved: resolved_out,
            converged: is_converged,
            no_new_information,
            new_claim_count,
            max_standing_delta,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetLedger;
    use crate::cache::ResponseCache;
    use crate::stage::{CancellationToken, DeterministicRng, EventSink, ProviderRegistry};
    use crate::stages::disputes_rank::RankInput;
    use crate::stages::options_cluster::ClusteredOptions;
    use crate::stages::relations_analyze::AnalyzedRelations;
    use arbiter_core::{
        AttachmentMatrix, CanonicalClaim, ClaimLifecycle, ClaimMember, EvidenceKind, Grounding,
        ModelId, PositionId, ProviderId, TextSpan,
    };
    use std::collections::BTreeMap;
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

    fn claim(id: &str, text: &str, model: &str, lifecycle: ClaimLifecycle) -> CanonicalClaim {
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
            lifecycle,
            members: vec![member],
        }
    }

    fn empty_resolved(
        claims: Vec<CanonicalClaim>,
        standing: BTreeMap<ClaimId, f64>,
    ) -> RankedDisputes {
        RankedDisputes {
            claims: NormalizedClaims(claims),
            relations: AnalyzedRelations(vec![]),
            options: ClusteredOptions {
                options: vec![],
                direct_matrix: AttachmentMatrix::default(),
            },
            standing,
            propagated_matrix: AttachmentMatrix::default(),
            ranked: vec![],
        }
    }

    fn stage(max_rounds: u32) -> ControllerDecide {
        ControllerDecide::new(
            Weights::default(),
            GraphParams::default(),
            Thresholds::default(),
            AttachmentParams::default(),
            DisputeWeights::default(),
            Cost(0.05),
            max_rounds,
            1.5,
            2,
            0.05,
        )
    }

    fn stage_ctx<'a>(
        registry: &'a ProviderRegistry,
        budget: &'a BudgetLedger,
        cache: &'a ResponseCache,
        sink: &'a RecordingSink,
        round: u32,
    ) -> StageContext<'a> {
        StageContext {
            providers: registry,
            budget,
            events: sink,
            cache,
            deadline: Instant::now() + std::time::Duration::from_secs(30),
            cancel: CancellationToken::new(),
            round,
            rng: DeterministicRng::seeded(1),
        }
    }

    #[tokio::test]
    async fn round_limit_wins_even_when_the_graph_has_converged() {
        // A single, undisputed, high-standing claim: nothing to dispute, no
        // dissent, would read Converged on its own merits -- but at
        // max_rounds=1 and round=1, RoundLimit must win regardless (§5.5:
        // "by construction").
        let claims = vec![claim(
            "c1",
            "an agreed fact",
            "model-a",
            ClaimLifecycle::Defended,
        )];
        let mut standing = BTreeMap::new();
        standing.insert(ClaimId::new("c1"), 0.9);
        let previous = empty_resolved(claims.clone(), standing);

        let rebuttals = RebuttalsRun {
            next_round_input: RankInput {
                claims: NormalizedClaims(claims),
                relations: AnalyzedRelations(vec![]),
                options: ClusteredOptions {
                    options: vec![],
                    direct_matrix: AttachmentMatrix::default(),
                },
            },
            outcomes: vec![],
        };

        let registry = ProviderRegistry::default();
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink, 1);

        let out = stage(1)
            .run(
                DecideInput {
                    rebuttals,
                    previous,
                },
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.control, ControlFlow::Stop(StopReason::RoundLimit));
    }

    #[tokio::test]
    async fn a_deep_run_with_room_left_continues_with_a_focus_list() {
        // Two claims, one contradicting the other -- genuinely disputed, no
        // way this reads Converged or NoNewInformation; at round 1 of 3
        // there is room left, so it must Continue.
        let claims = vec![
            claim(
                "c1",
                "the primary claim",
                "model-a",
                ClaimLifecycle::Proposed,
            ),
            claim(
                "c2",
                "a contradicting claim",
                "model-b",
                ClaimLifecycle::Proposed,
            ),
        ];
        let relations = vec![arbiter_core::Relation {
            from: ClaimId::new("c2"),
            to: ClaimId::new("c1"),
            kind: arbiter_core::RelationKind::Contradicts,
            confidence: 0.9,
        }];
        let mut standing = BTreeMap::new();
        standing.insert(ClaimId::new("c1"), 0.5);
        standing.insert(ClaimId::new("c2"), 0.5);
        let previous = empty_resolved(claims.clone(), standing);

        let rebuttals = RebuttalsRun {
            next_round_input: RankInput {
                claims: NormalizedClaims(claims),
                relations: AnalyzedRelations(relations),
                options: ClusteredOptions {
                    options: vec![],
                    direct_matrix: AttachmentMatrix::default(),
                },
            },
            outcomes: vec![],
        };

        let registry = ProviderRegistry::default();
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink, 1);

        let out = stage(3)
            .run(
                DecideInput {
                    rebuttals,
                    previous,
                },
                &ctx,
            )
            .await
            .unwrap();
        match out.control {
            ControlFlow::Continue { round, focus } => {
                assert_eq!(round, 2);
                assert!(
                    !focus.is_empty(),
                    "genuinely disputed claims must be in focus"
                );
            }
            other => panic!("expected Continue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancellation_wins_over_every_other_predicate() {
        let claims = vec![claim("c1", "a claim", "model-a", ClaimLifecycle::Proposed)];
        let mut standing = BTreeMap::new();
        standing.insert(ClaimId::new("c1"), 0.5);
        let previous = empty_resolved(claims.clone(), standing);
        let rebuttals = RebuttalsRun {
            next_round_input: RankInput {
                claims: NormalizedClaims(claims),
                relations: AnalyzedRelations(vec![]),
                options: ClusteredOptions {
                    options: vec![],
                    direct_matrix: AttachmentMatrix::default(),
                },
            },
            outcomes: vec![],
        };

        let registry = ProviderRegistry::default();
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink, 1);
        ctx.cancel.cancel();

        let out = stage(3)
            .run(
                DecideInput {
                    rebuttals,
                    previous,
                },
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.control, ControlFlow::Stop(StopReason::Cancelled));
    }

    #[tokio::test]
    async fn an_exhausted_budget_stops_the_run_before_round_limit_is_even_relevant() {
        let claims = vec![claim("c1", "a claim", "model-a", ClaimLifecycle::Proposed)];
        let mut standing = BTreeMap::new();
        standing.insert(ClaimId::new("c1"), 0.5);
        let previous = empty_resolved(claims.clone(), standing);
        let rebuttals = RebuttalsRun {
            next_round_input: RankInput {
                claims: NormalizedClaims(claims),
                relations: AnalyzedRelations(vec![]),
                options: ClusteredOptions {
                    options: vec![],
                    direct_matrix: AttachmentMatrix::default(),
                },
            },
            outcomes: vec![],
        };

        let registry = ProviderRegistry::default();
        let budget = BudgetLedger::new(Some(Cost(0.0)));
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink, 1);

        let out = stage(3)
            .run(
                DecideInput {
                    rebuttals,
                    previous,
                },
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.control, ControlFlow::Stop(StopReason::BudgetExhausted));
    }
}
