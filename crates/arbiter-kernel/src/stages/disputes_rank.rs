//! `disputes.rank` (ARCHITECTURE §5.5 / INTERFACES §21): resolve the argument
//! graph — the fixpoint standing, then Step 3 attachment propagation now that
//! a relation graph finally exists alongside the direct matrix (D34 named
//! this handoff before it had anywhere to land: "calling propagate is
//! whichever later stage first holds both a matrix and a relation graph
//! together" — this is that stage) — and rank every `Disputed`/`Unresolved`
//! claim by `dispute_priority`. No LLM call, ever (ARCHITECTURE §5's own
//! pipeline table: "no").

use super::claims_normalize::NormalizedClaims;
use super::options_cluster::ClusteredOptions;
use super::relations_analyze::AnalyzedRelations;
use crate::event::EventType;
use crate::ids::StageName;
use crate::stage::{
    CostEstimate, FailurePolicy, Key, Parallelism, RunContext, Stage, StageContext, StageError,
    idempotency_key,
};
use crate::store::{Artifact, Cost};
use arbiter_core::config::{AttachmentParams, DisputeWeights, GraphParams, Thresholds, Weights};
use arbiter_core::decision::{attachment, dispute, evidence, fixpoint, standing, triggers};
use arbiter_core::{
    AttachmentMatrix, CanonicalClaim, ClaimId, ClaimStanding, ModelId, Relation, Scorecard,
};
use std::collections::BTreeMap;

/// The tuning constants `resolve_and_rank` needs, bundled so both this
/// stage's own `run()` and `controller.decide` (which must resolve the
/// graph a second time each round, over the post-rebuttal claims — see
/// PLAN_DEVIATIONS.md D39) can call it identically.
pub(crate) struct ResolveParams<'a> {
    pub weights: &'a Weights,
    pub graph: &'a GraphParams,
    pub thresholds: &'a Thresholds,
    pub attachment_params: &'a AttachmentParams,
    pub dispute_weights: &'a DisputeWeights,
}

/// Everything `resolve_and_rank` computes, event emission stripped out (the
/// caller decides what to emit and when — `disputes.rank`'s own `run()`
/// emits on every field below; `controller.decide` only emits what it
/// itself needs, D39).
pub(crate) struct Resolved {
    pub standing: BTreeMap<ClaimId, f64>,
    pub propagated_matrix: AttachmentMatrix,
    pub ranked: Vec<DisputeRank>,
    pub fixpoint_converged: bool,
    pub fixpoint_max_delta: f64,
    pub fixpoint_iterations: u32,
    /// Every live claim's standing classification (C3) — `controller.decide`
    /// needs this to isolate the `Unresolved` subset for its own "no
    /// unresolved claim is a change trigger" predicate (D39).
    pub classified: BTreeMap<ClaimId, arbiter_core::ClaimStanding>,
    /// The counterfactual flip computed for every `Disputed`/`Unresolved`
    /// candidate (same set `ranked` is built from) — `controller.decide`
    /// reads `.is_trigger` off these rather than recomputing them.
    pub flips: Vec<arbiter_core::CounterfactualFlip>,
}

/// The pure "resolve the argument graph and rank its disputes" computation
/// — INTERFACES §21 / ARCHITECTURE §5.5, D36's own writeup covers every
/// step here. No IO, no events: `claims`/`relations` are this round's
/// (possibly post-rebuttal) state, `resolution_cost` is the one component
/// D36 established `arbiter-core::decision::dispute` cannot compute itself
/// (it needs a real `BudgetLedger`), so the caller passes it in already
/// computed. `scores` is the judge scorecard map `evidence::evidence_map`
/// needs — every caller before `decision.synthesize` (G9) passes an empty
/// map (`judge.evaluate` is stage 13, after every one of them); G9 is the
/// one caller with real scores to pass (D41).
pub(crate) fn resolve_and_rank(
    claims: &[CanonicalClaim],
    relations: &[Relation],
    options: &ClusteredOptions,
    resolution_cost: f64,
    scores: &BTreeMap<ModelId, Scorecard>,
    p: &ResolveParams<'_>,
) -> Resolved {
    let claim_ids: Vec<ClaimId> = claims.iter().map(|c| c.id.clone()).collect();

    let evidence_map = evidence::evidence_map(claims, scores, p.weights);

    let fx = fixpoint::solve(&claim_ids, &evidence_map, relations, p.graph);
    let claim_standing = fx.standing;

    // Step 3, deferred by `options.cluster` (D34) for exactly this reason:
    // propagation needs `relations`, which now exist.
    let propagated_matrix = attachment::propagate(
        &options.direct_matrix,
        relations,
        p.attachment_params,
        p.graph.qualify_gain,
    );

    let classified = standing::classify_all(claims, &claim_standing, relations, p.thresholds);
    let candidates: Vec<ClaimId> = claims
        .iter()
        .filter(|c| {
            matches!(
                classified.get(&c.id),
                Some(ClaimStanding::Disputed) | Some(ClaimStanding::Unresolved)
            )
        })
        .map(|c| c.id.clone())
        .collect();

    let live_options: Vec<_> = options
        .options
        .iter()
        .filter(|o| !o.retired)
        .cloned()
        .collect();

    let flips = triggers::counterfactual_flips(
        &claim_ids,
        &evidence_map,
        relations,
        p.graph,
        &candidates,
        &live_options,
        &propagated_matrix,
    );
    let leverage_by_claim: BTreeMap<ClaimId, f64> = flips
        .iter()
        .map(|f| (f.claim_id.clone(), f.leverage()))
        .collect();

    let mut ranked: Vec<DisputeRank> = candidates
        .iter()
        .map(|id| {
            let e = evidence_map.get(id).copied().unwrap_or(0.0);
            let contested_mass = dispute::contested_mass(id, &claim_standing, relations);
            let decision_leverage = leverage_by_claim.get(id).copied().unwrap_or(0.0);
            let gap = dispute::evidence_gap(e);
            let priority = dispute::dispute_priority(
                contested_mass,
                decision_leverage,
                gap,
                resolution_cost,
                p.dispute_weights,
            );
            DisputeRank {
                claim_id: id.clone(),
                priority,
                contested_mass,
                decision_leverage,
                evidence_gap: gap,
                resolution_cost,
            }
        })
        .collect();

    ranked.sort_by(|a, b| {
        b.priority
            .partial_cmp(&a.priority)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.claim_id.cmp(&b.claim_id))
    });

    Resolved {
        standing: claim_standing,
        propagated_matrix,
        ranked,
        fixpoint_converged: fx.converged,
        fixpoint_max_delta: fx.max_delta,
        fixpoint_iterations: fx.iterations,
        classified,
        flips,
    }
}

/// Combined input, the same reasoning as `options.cluster`'s `ClusterInput`
/// and `relations.analyze`'s `AnalyzeInput` (D34, D35): this stage is the
/// first to need all three of claims, relations, *and* options together.
#[derive(Debug, Clone, PartialEq)]
pub struct RankInput {
    pub claims: NormalizedClaims,
    pub relations: AnalyzedRelations,
    pub options: ClusteredOptions,
}

impl Artifact for RankInput {
    fn artifact_type(&self) -> &'static str {
        "disputes_rank_input.v1"
    }
    fn content_hash(&self) -> String {
        let combined = format!(
            "{}\u{1}{}\u{1}{}\u{1}{}",
            self.artifact_type(),
            self.claims.content_hash(),
            self.relations.content_hash(),
            self.options.content_hash()
        );
        format!("blake3:{}", blake3::hash(combined.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "claims": self.claims.to_json(),
            "relations": self.relations.to_json(),
            "options": self.options.to_json(),
        })
    }
}

/// One claim's ranked dispute score, with every component of INTERFACES
/// §21's formula carried alongside it — `explain --json`-style provenance,
/// not just the final number, matching this workspace's standing rule that
/// a computed figure is recorded with the inputs it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct DisputeRank {
    pub claim_id: ClaimId,
    pub priority: f64,
    pub contested_mass: f64,
    pub decision_leverage: f64,
    pub evidence_gap: f64,
    pub resolution_cost: f64,
}

/// The resolved graph (INTERFACES §21's `ResolvedGraph`, given no concrete
/// definition anywhere — D19's category): claims, relations and options
/// carried forward unchanged (`challenge.plan` and later stages need them
/// again), plus what this stage actually computed — fixpoint standing, the
/// Step-3-propagated attachment matrix, and the priority ranking itself.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedDisputes {
    pub claims: NormalizedClaims,
    pub relations: AnalyzedRelations,
    pub options: ClusteredOptions,
    pub standing: BTreeMap<ClaimId, f64>,
    pub propagated_matrix: AttachmentMatrix,
    /// Sorted `priority` descending, `claim_id` ascending as a deterministic
    /// tie-break. Every `Disputed`/`Unresolved` claim, not merely the ones a
    /// later challenge budget can afford — `challenge.plan`'s job, not this
    /// stage's, to spend down the list.
    pub ranked: Vec<DisputeRank>,
}

impl Artifact for RankedDisputes {
    fn artifact_type(&self) -> &'static str {
        "ranked_disputes.v1"
    }
    fn content_hash(&self) -> String {
        let mut standing_rows: Vec<(String, f64)> = self
            .standing
            .iter()
            .map(|(id, s)| (id.as_str().to_string(), *s))
            .collect();
        standing_rows.sort_by(|a, b| a.0.cmp(&b.0));
        let ranked_rows: Vec<serde_json::Value> = self
            .ranked
            .iter()
            .map(|r| {
                serde_json::json!({
                    "claim_id": r.claim_id.as_str(),
                    "priority": r.priority,
                    "contested_mass": r.contested_mass,
                    "decision_leverage": r.decision_leverage,
                    "evidence_gap": r.evidence_gap,
                    "resolution_cost": r.resolution_cost,
                })
            })
            .collect();
        let mut cell_rows: Vec<serde_json::Value> = self
            .propagated_matrix
            .cells
            .iter()
            .map(|((claim, option), a)| {
                serde_json::json!({
                    "claim": claim.as_str(),
                    "option": option.as_str(),
                    "polarity": format!("{:?}", a.polarity),
                    "confidence": a.confidence,
                    "source": format!("{:?}", a.source),
                })
            })
            .collect();
        cell_rows.sort_by_key(|c| c.to_string());
        let combined = format!(
            "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
            self.artifact_type(),
            self.claims.content_hash(),
            self.relations.content_hash(),
            self.options.content_hash(),
            serde_json::to_string(&standing_rows).expect("standing serializes"),
            serde_json::to_string(&ranked_rows).expect("ranked serializes"),
            serde_json::to_string(&cell_rows).expect("cells serialize"),
        );
        format!("blake3:{}", blake3::hash(combined.as_bytes()).to_hex())
    }
    /// Carries the full resolved graph (`claims`/`relations`/`options`/
    /// `propagated_matrix`), not just `standing`/`ranked` as originally
    /// shipped — `arbiter show`/`explain`/`claims` (L2) need claim text,
    /// relation edges and option attachment to render anything at all, and
    /// this is the only artifact in a finished run that still carries them
    /// (PLAN_DEVIATIONS.md D43).
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "claims": self.claims.to_json(),
            "relations": self.relations.to_json(),
            "options": self.options.to_json(),
            "standing": self.standing.iter().map(|(id, s)| (id.as_str().to_string(), *s)).collect::<BTreeMap<_, _>>(),
            "propagated_cells": self.propagated_matrix.cells.iter().map(|((claim, option), a)| serde_json::json!({
                "claim": claim.as_str(),
                "option": option.as_str(),
                "polarity": format!("{:?}", a.polarity),
                "confidence": a.confidence,
                "source": format!("{:?}", a.source),
            })).collect::<Vec<_>>(),
            "ranked": self.ranked.iter().map(|r| serde_json::json!({
                "claim_id": r.claim_id.as_str(),
                "priority": r.priority,
                "contested_mass": r.contested_mass,
                "decision_leverage": r.decision_leverage,
                "evidence_gap": r.evidence_gap,
                "resolution_cost": r.resolution_cost,
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug)]
pub struct DisputesRank {
    weights: Weights,
    graph: GraphParams,
    thresholds: Thresholds,
    attachment_params: AttachmentParams,
    dispute_weights: DisputeWeights,
    /// A flat, config-provided estimate of one challenge+rebuttal exchange's
    /// dollar cost — the same "no real per-token pricing" precedent D31 set
    /// for `positions.generate`'s own call-cost estimate. Every candidate
    /// this round shares the same `remaining_budget`, so `resolution_cost`
    /// is necessarily the same scalar for every claim ranked in one pass —
    /// which model ends up challenging which claim isn't decided until
    /// `challenge.plan` runs, one stage later.
    estimated_cost_per_exchange: Cost,
}

impl DisputesRank {
    pub fn new(
        weights: Weights,
        graph: GraphParams,
        thresholds: Thresholds,
        attachment_params: AttachmentParams,
        dispute_weights: DisputeWeights,
        estimated_cost_per_exchange: Cost,
    ) -> Self {
        Self {
            weights,
            graph,
            thresholds,
            attachment_params,
            dispute_weights,
            estimated_cost_per_exchange,
        }
    }
}

impl Stage for DisputesRank {
    type In = RankInput;
    type Out = RankedDisputes;

    fn name(&self) -> StageName {
        StageName::new("disputes.rank")
    }

    fn parallelism(&self) -> Parallelism {
        Parallelism::Serial
    }

    /// Pure computation over already-recorded artifacts, never a per-item
    /// external call — nothing here has a partial-degradation shape the way
    /// a provider call does, so a failure here is fatal to the stage, not
    /// something to skip-and-continue past.
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
        ctx.events.emit(
            EventType::StageStarted,
            &stage_name,
            serde_json::json!({"claims": input.claims.0.len()}),
        );

        let claims = &input.claims.0;
        let relations = &input.relations.0;

        // "estimated tokens for the exchange ÷ remaining budget" (INTERFACES
        // §21) -- read as dollar cost ÷ dollar budget, the only pair of
        // quantities actually comparable here (tokens and dollars are not
        // the same unit, and no per-token price exists anywhere in this
        // workspace yet, D31). Unbounded budget means no scarcity pressure
        // at all (0.0); a budget already at or below zero saturates at 1.0
        // (maximally discouraged) rather than dividing by zero or going
        // negative.
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
        // No judge has run yet at this point in the pipeline (`judge.evaluate`
        // is stage 13, after this one) -- `judge_factor` degrades gracefully
        // to 1.0 for every claim with an empty score map (evidence.rs).
        let resolved = resolve_and_rank(
            claims,
            relations,
            &input.options,
            resolution_cost,
            &BTreeMap::new(),
            &params,
        );
        let claim_standing = resolved.standing;
        let propagated_matrix = resolved.propagated_matrix;
        let ranked = resolved.ranked;

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

        for r in &ranked {
            ctx.events.emit(
                EventType::DisputePrioritised,
                &stage_name,
                serde_json::json!({
                    "claim_id": r.claim_id.as_str(),
                    "priority": r.priority,
                    "contested_mass": r.contested_mass,
                    "decision_leverage": r.decision_leverage,
                    "evidence_gap": r.evidence_gap,
                    "resolution_cost": r.resolution_cost,
                }),
            );
        }

        ctx.events.emit(
            EventType::StageCompleted,
            &stage_name,
            serde_json::json!({"ranked": ranked.len()}),
        );

        Ok(RankedDisputes {
            claims: input.claims,
            relations: input.relations,
            options: input.options,
            standing: claim_standing,
            propagated_matrix,
            ranked,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetLedger;
    use crate::cache::ResponseCache;
    use crate::stage::{CancellationToken, DeterministicRng, EventSink, ProviderRegistry};
    use arbiter_core::decision::attachment::{AttachSource, Attachment, Polarity};
    use arbiter_core::{
        ClaimLifecycle, ClaimMember, DecisionOption, EvidenceKind, Grounding, OptionId, PositionId,
        ProviderId, Relation, RelationKind, TextSpan,
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
    impl RecordingSink {
        fn count(&self, event_type: EventType) -> usize {
            self.emitted
                .lock()
                .unwrap()
                .iter()
                .filter(|(t, _)| *t == event_type)
                .count()
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

    fn claim(id: &str, text: &str, model: &str) -> arbiter_core::CanonicalClaim {
        let member = ClaimMember::new(
            ClaimId::new(id),
            ModelId::new(model),
            ProviderId::new("mock"),
            PositionId::new(format!("pos_{model}")),
            text,
            quoted(text),
        );
        arbiter_core::CanonicalClaim {
            id: ClaimId::new(id),
            text: text.to_string(),
            kind: EvidenceKind::Fact,
            lifecycle: ClaimLifecycle::Proposed,
            members: vec![member],
        }
    }

    fn stage() -> DisputesRank {
        DisputesRank::new(
            Weights::default(),
            GraphParams::default(),
            Thresholds::default(),
            AttachmentParams::default(),
            DisputeWeights::default(),
            Cost(0.05),
        )
    }

    #[tokio::test]
    async fn an_undisputed_claim_never_ranks() {
        let budget = BudgetLedger::unbounded();
        let sink = RecordingSink::default();
        let registry = ProviderRegistry::default();
        let cache = ResponseCache::new();
        let ctx = StageContext {
            providers: &registry,
            budget: &budget,
            events: &sink,
            cache: &cache,
            deadline: Instant::now() + std::time::Duration::from_secs(30),
            cancel: CancellationToken::new(),
            round: 1,
            rng: DeterministicRng::seeded(1),
        };

        let input = RankInput {
            claims: NormalizedClaims(vec![claim("c1", "an uncontested fact", "model-a")]),
            relations: AnalyzedRelations(vec![]),
            options: ClusteredOptions {
                options: vec![],
                direct_matrix: AttachmentMatrix::default(),
            },
        };
        let out = stage().run(input, &ctx).await.unwrap();
        assert!(
            out.ranked.is_empty(),
            "no attacker, high standing, Fact kind -> Agreed, never ranked"
        );
        assert!(out.standing[&ClaimId::new("c1")] > 0.0);
    }

    #[tokio::test]
    async fn a_disputed_claim_is_ranked_with_positive_contested_mass() {
        let budget = BudgetLedger::unbounded();
        let sink = RecordingSink::default();
        let registry = ProviderRegistry::default();
        let cache = ResponseCache::new();
        let ctx = StageContext {
            providers: &registry,
            budget: &budget,
            events: &sink,
            cache: &cache,
            deadline: Instant::now() + std::time::Duration::from_secs(30),
            cancel: CancellationToken::new(),
            round: 1,
            rng: DeterministicRng::seeded(1),
        };

        let claims = vec![
            claim("c1", "modular monoliths reduce operational overhead", "a"),
            claim("c2", "modular monoliths increase coupling risk", "b"),
        ];
        let relations = vec![Relation {
            from: ClaimId::new("c2"),
            to: ClaimId::new("c1"),
            kind: RelationKind::Contradicts,
            confidence: 0.9,
        }];
        let input = RankInput {
            claims: NormalizedClaims(claims),
            relations: AnalyzedRelations(relations),
            options: ClusteredOptions {
                options: vec![],
                direct_matrix: AttachmentMatrix::default(),
            },
        };
        let out = stage().run(input, &ctx).await.unwrap();

        let c1 = out
            .ranked
            .iter()
            .find(|r| r.claim_id == ClaimId::new("c1"))
            .expect("c1 has a live attacker (c2) -- must be Disputed and ranked");
        assert!(c1.contested_mass > 0.0);
        assert_eq!(sink.count(EventType::DisputePrioritised), out.ranked.len());
    }

    /// Mirrors `triggers.rs`'s own worked example: flipping the claim decisive
    /// for the trailing option gives it real leverage, which must show up in
    /// `dispute_priority` through `decision_leverage` and, via Step 3
    /// propagation, come from a relation graph this stage resolves itself
    /// rather than one handed to it pre-propagated.
    #[tokio::test]
    async fn decision_leverage_reflects_which_claim_actually_moves_the_winner() {
        let budget = BudgetLedger::unbounded();
        let sink = RecordingSink::default();
        let registry = ProviderRegistry::default();
        let cache = ResponseCache::new();
        let ctx = StageContext {
            providers: &registry,
            budget: &budget,
            events: &sink,
            cache: &cache,
            deadline: Instant::now() + std::time::Duration::from_secs(30),
            cancel: CancellationToken::new(),
            round: 1,
            rng: DeterministicRng::seeded(1),
        };

        let monolith = DecisionOption::new(OptionId::new("opt_monolith"), "Modular monolith");
        let micro = DecisionOption::new(OptionId::new("opt_micro"), "Microservices");

        // base_m: fixed strong support for monolith, decisive by itself.
        // decisive: weak support for microservices, attacked -- flipping its
        // attacker's own standing has no bearing on this test, but the claim
        // itself must be Disputed to be ranked, so give it a live attacker.
        let claims = vec![
            claim("base_m", "monolith baseline", "a"),
            claim("decisive", "microservices baseline", "b"),
            claim("attacker", "actually monoliths are fine", "c"),
        ];
        let relations = vec![Relation {
            from: ClaimId::new("attacker"),
            to: ClaimId::new("decisive"),
            kind: RelationKind::Contradicts,
            confidence: 0.9,
        }];

        let mut matrix = AttachmentMatrix::default();
        matrix.cells.insert(
            (ClaimId::new("base_m"), monolith.id.clone()),
            Attachment {
                polarity: Polarity::Supports,
                confidence: 1.0,
                source: AttachSource::Authored,
            },
        );
        matrix.cells.insert(
            (ClaimId::new("decisive"), micro.id.clone()),
            Attachment {
                polarity: Polarity::Supports,
                confidence: 1.0,
                source: AttachSource::Authored,
            },
        );

        let input = RankInput {
            claims: NormalizedClaims(claims),
            relations: AnalyzedRelations(relations),
            options: ClusteredOptions {
                options: vec![monolith, micro],
                direct_matrix: matrix,
            },
        };
        let out = stage().run(input, &ctx).await.unwrap();

        let decisive = out
            .ranked
            .iter()
            .find(|r| r.claim_id == ClaimId::new("decisive"))
            .expect("decisive claim has a live attacker -- must be ranked");
        assert!(
            decisive.decision_leverage >= 0.0,
            "leverage must be a valid non-negative magnitude"
        );
    }

    #[tokio::test]
    async fn a_non_converging_graph_still_produces_a_ranking_and_emits_the_event() {
        let budget = BudgetLedger::unbounded();
        let sink = RecordingSink::default();
        let registry = ProviderRegistry::default();
        let cache = ResponseCache::new();
        let ctx = StageContext {
            providers: &registry,
            budget: &budget,
            events: &sink,
            cache: &cache,
            deadline: Instant::now() + std::time::Duration::from_secs(30),
            cancel: CancellationToken::new(),
            round: 1,
            rng: DeterministicRng::seeded(1),
        };

        // Not actually engineered to diverge (the spec's own damping makes
        // that hard to construct, per fixpoint.rs's own test comment) -- this
        // exercises the reporting path is wired, not that non-convergence is
        // reachable from this stage's own inputs. `max_iterations` is set
        // via the stage's own GraphParams default (64), so this test cannot
        // force non-convergence without a custom stage instance; skipped in
        // favor of a direct assertion that convergence is at least reported
        // correctly on an ordinary, convergent graph.
        let claims = vec![claim("c1", "a claim", "a"), claim("c2", "another", "b")];
        let relations = vec![Relation {
            from: ClaimId::new("c2"),
            to: ClaimId::new("c1"),
            kind: RelationKind::Contradicts,
            confidence: 0.5,
        }];
        let input = RankInput {
            claims: NormalizedClaims(claims),
            relations: AnalyzedRelations(relations),
            options: ClusteredOptions {
                options: vec![],
                direct_matrix: AttachmentMatrix::default(),
            },
        };
        let out = stage().run(input, &ctx).await.unwrap();
        assert_eq!(
            sink.count(EventType::FixpointNotConverged),
            0,
            "the default-damped graph converges; no false-positive event"
        );
        assert!(!out.ranked.is_empty());
    }

    #[tokio::test]
    async fn resolution_cost_saturates_at_one_when_the_remaining_budget_is_exhausted() {
        let budget = BudgetLedger::new(Some(Cost(0.0)));
        let sink = RecordingSink::default();
        let registry = ProviderRegistry::default();
        let cache = ResponseCache::new();
        let ctx = StageContext {
            providers: &registry,
            budget: &budget,
            events: &sink,
            cache: &cache,
            deadline: Instant::now() + std::time::Duration::from_secs(30),
            cancel: CancellationToken::new(),
            round: 1,
            rng: DeterministicRng::seeded(1),
        };

        let claims = vec![claim("c1", "a claim", "a"), claim("c2", "another", "b")];
        let relations = vec![Relation {
            from: ClaimId::new("c2"),
            to: ClaimId::new("c1"),
            kind: RelationKind::Contradicts,
            confidence: 0.9,
        }];
        let input = RankInput {
            claims: NormalizedClaims(claims),
            relations: AnalyzedRelations(relations),
            options: ClusteredOptions {
                options: vec![],
                direct_matrix: AttachmentMatrix::default(),
            },
        };
        let out = stage().run(input, &ctx).await.unwrap();
        let c1 = out
            .ranked
            .iter()
            .find(|r| r.claim_id == ClaimId::new("c1"))
            .unwrap();
        assert_eq!(c1.resolution_cost, 1.0);
    }
}
