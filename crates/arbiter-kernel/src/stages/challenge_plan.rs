//! `challenge.plan` (ARCHITECTURE §5.5 / INTERFACES §21): spend a
//! money-derived challenge budget on the highest-priority disputes
//! `disputes.rank` already ranked. No LLM call, ever — this stage only
//! *selects* pairs; `challenge.run` (G6) is the one that issues them.
//!
//! ```text
//! round_budget     = remaining_budget ÷ remaining_rounds
//! challenge_budget = round_budget − judge_reservation      (never negative)
//! for each dispute, top-down until challenge_budget is spent:
//!     defender   = the claim's author(s)
//!     challenger = the model whose claim most strongly contradicts it
//!                  (relation confidence × attacker standing)
//!     skip if challenger is a defender, or already at max_challenges_per_model
//! ```

use super::disputes_rank::RankedDisputes;
use crate::event::EventType;
use crate::ids::StageName;
use crate::stage::{
    CostEstimate, FailurePolicy, Key, Parallelism, RunContext, Stage, StageContext, StageError,
    idempotency_key,
};
use crate::store::{Artifact, Cost};
use arbiter_core::{ClaimId, ModelId, ProviderId, RelationKind};
use std::collections::{BTreeMap, BTreeSet};

/// One selected challenge: `challenger` will argue against `claim_id`,
/// picked because `attacking_claim_id` — one of `claim_id`'s live
/// contradictors — most strongly disagreed with it.
#[derive(Debug, Clone, PartialEq)]
pub struct ChallengePair {
    pub claim_id: ClaimId,
    pub priority: f64,
    /// Every model that asserted the defended claim (`CanonicalClaim::asserted_by`)
    /// — INTERFACES §21 says "the claim's author" (singular), but a
    /// `CanonicalClaim` can carry members from several models at once
    /// (ARCHITECTURE §5.2: normalization merges equivalent claims across
    /// models). Read literally, singular, on a merged claim would be
    /// arbitrary; read as "every model that would be self-challenging",
    /// which is the property the rule actually protects, it generalises
    /// cleanly to the set (PLAN_DEVIATIONS.md D37).
    pub defenders: Vec<ModelId>,
    pub challenger: ModelId,
    pub challenger_provider: ProviderId,
    pub attacking_claim_id: ClaimId,
    pub estimated_cost: Cost,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChallengePlanned {
    /// Carried forward unchanged — `challenge.run` needs the claim texts,
    /// relations and standing again, and this is the artifact that already
    /// holds all of it (same "resolved graph, carried forward" shape as
    /// `disputes.rank`'s own output).
    pub resolved: RankedDisputes,
    /// In selection order (highest priority first among the ones actually
    /// affordable and pairable) — never all-pairs, per ARCHITECTURE §5's own
    /// table entry for this stage.
    pub pairs: Vec<ChallengePair>,
}

impl Artifact for ChallengePlanned {
    fn artifact_type(&self) -> &'static str {
        "challenge_planned.v1"
    }
    fn content_hash(&self) -> String {
        let pair_rows: Vec<serde_json::Value> = self
            .pairs
            .iter()
            .map(|p| {
                serde_json::json!({
                    "claim_id": p.claim_id.as_str(),
                    "priority": p.priority,
                    "defenders": p.defenders.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
                    "challenger": p.challenger.as_str(),
                    "challenger_provider": p.challenger_provider.as_str(),
                    "attacking_claim_id": p.attacking_claim_id.as_str(),
                    "estimated_cost": p.estimated_cost.0,
                })
            })
            .collect();
        let combined = format!(
            "{}\u{1}{}",
            self.resolved.content_hash(),
            serde_json::to_string(&pair_rows).expect("pairs serialize"),
        );
        format!("blake3:{}", blake3::hash(combined.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "resolved": self.resolved.to_json(),
            "pairs": self.pairs.iter().map(|p| serde_json::json!({
                "claim_id": p.claim_id.as_str(),
                "priority": p.priority,
                "defenders": p.defenders.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
                "challenger": p.challenger.as_str(),
                "challenger_provider": p.challenger_provider.as_str(),
                "attacking_claim_id": p.attacking_claim_id.as_str(),
                "estimated_cost": p.estimated_cost.0,
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug)]
pub struct ChallengePlan {
    /// Hard ceiling on `--depth`'s rounds (ARCHITECTURE §5.5: 1 standard, 3
    /// deep, 6 hard ceiling) — needed for `remaining_rounds`, which is not a
    /// field `StageContext`/`RunContext` carries (neither is given a
    /// concrete definition anywhere that includes it, D19's category), so
    /// it is baked into this stage like every other tuning constant.
    max_rounds: u32,
    /// A flat, config-provided per-exchange cost estimate, same "no real
    /// per-token pricing" precedent D31 set for `positions.generate`.
    estimated_cost_per_exchange: Cost,
    /// "reserves the judge's share first" (ARCHITECTURE §5.5) — a flat
    /// estimate of `judge.evaluate`'s own eventual spend, same reasoning.
    judge_reservation_estimate: Cost,
    /// Default 2 (ARCHITECTURE §5.5).
    max_challenges_per_model: usize,
}

impl ChallengePlan {
    pub fn new(
        max_rounds: u32,
        estimated_cost_per_exchange: Cost,
        judge_reservation_estimate: Cost,
        max_challenges_per_model: usize,
    ) -> Self {
        Self {
            max_rounds,
            estimated_cost_per_exchange,
            judge_reservation_estimate,
            max_challenges_per_model,
        }
    }
}

impl Stage for ChallengePlan {
    type In = RankedDisputes;
    type Out = ChallengePlanned;

    fn name(&self) -> StageName {
        StageName::new("challenge.plan")
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
        ctx.events.emit(
            EventType::StageStarted,
            &stage_name,
            serde_json::json!({"candidates": input.ranked.len()}),
        );

        let remaining_budget = ctx.budget.remaining().map(|c| c.0).unwrap_or(f64::INFINITY);
        let remaining_rounds = (self.max_rounds.saturating_sub(ctx.round) + 1).max(1) as f64;
        let round_budget = remaining_budget / remaining_rounds;
        let challenge_budget = (round_budget - self.judge_reservation_estimate.0).max(0.0);

        let claims_by_id: BTreeMap<ClaimId, &arbiter_core::CanonicalClaim> =
            input.claims.0.iter().map(|c| (c.id.clone(), c)).collect();

        let mut spent = 0.0;
        let mut per_model_count: BTreeMap<ModelId, usize> = BTreeMap::new();
        let mut pairs = Vec::new();

        for rank in &input.ranked {
            if spent + self.estimated_cost_per_exchange.0 > challenge_budget {
                break;
            }
            let Some(claim) = claims_by_id.get(&rank.claim_id) else {
                continue;
            };
            let defenders: BTreeSet<ModelId> = claim.asserted_by().into_iter().collect();

            let mut attackers: Vec<&arbiter_core::Relation> = input
                .relations
                .0
                .iter()
                .filter(|r| r.kind == RelationKind::Contradicts && r.to == rank.claim_id)
                .collect();
            attackers.sort_by(|a, b| {
                let sa = a.confidence * input.standing.get(&a.from).copied().unwrap_or(0.0);
                let sb = b.confidence * input.standing.get(&b.from).copied().unwrap_or(0.0);
                sb.partial_cmp(&sa)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.from.cmp(&b.from))
            });

            let mut chosen: Option<(ModelId, ProviderId, ClaimId)> = None;
            'attackers: for r in &attackers {
                let Some(attacker_claim) = claims_by_id.get(&r.from) else {
                    continue;
                };
                for model in attacker_claim.asserted_by() {
                    if defenders.contains(&model) {
                        continue;
                    }
                    if per_model_count.get(&model).copied().unwrap_or(0)
                        >= self.max_challenges_per_model
                    {
                        continue;
                    }
                    let Some(provider) = attacker_claim
                        .members
                        .iter()
                        .find(|m| m.model == model)
                        .map(|m| m.provider.clone())
                    else {
                        continue;
                    };
                    chosen = Some((model, provider, r.from.clone()));
                    break 'attackers;
                }
            }

            let Some((challenger, challenger_provider, attacking_claim_id)) = chosen else {
                continue;
            };
            *per_model_count.entry(challenger.clone()).or_insert(0) += 1;
            spent += self.estimated_cost_per_exchange.0;
            pairs.push(ChallengePair {
                claim_id: rank.claim_id.clone(),
                priority: rank.priority,
                defenders: defenders.into_iter().collect(),
                challenger,
                challenger_provider,
                attacking_claim_id,
                estimated_cost: self.estimated_cost_per_exchange,
            });
        }

        ctx.events.emit(
            EventType::StageCompleted,
            &stage_name,
            serde_json::json!({
                "planned": pairs.len(),
                "challenge_budget": challenge_budget,
                "spent": spent,
            }),
        );

        Ok(ChallengePlanned {
            resolved: input,
            pairs,
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
    use crate::stages::disputes_rank::DisputeRank;
    use crate::stages::options_cluster::ClusteredOptions;
    use crate::stages::relations_analyze::AnalyzedRelations;
    use arbiter_core::{
        AttachmentMatrix, CanonicalClaim, ClaimLifecycle, ClaimMember, EvidenceKind, Grounding,
        PositionId, Relation, TextSpan,
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
            lifecycle: ClaimLifecycle::Proposed,
            members: vec![member],
        }
    }

    fn resolved(
        claims: Vec<CanonicalClaim>,
        relations: Vec<Relation>,
        standing: BTreeMap<ClaimId, f64>,
        ranked: Vec<DisputeRank>,
    ) -> RankedDisputes {
        RankedDisputes {
            claims: NormalizedClaims(claims),
            relations: AnalyzedRelations(relations),
            options: ClusteredOptions {
                options: vec![],
                direct_matrix: AttachmentMatrix::default(),
            },
            standing,
            propagated_matrix: AttachmentMatrix::default(),
            ranked,
        }
    }

    fn ctx<'a>(
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
    async fn a_defended_claim_is_never_challenged_by_its_own_author() {
        // c1's only contradictor (c2) is asserted by the same model that
        // asserts c1 -- self-challenge must be skipped, and with no other
        // attacker available, no pair is planned for c1 at all.
        let claims = vec![
            claim("c1", "claim one", "model-a"),
            claim("c2", "claim two", "model-a"),
        ];
        let relations = vec![Relation {
            from: ClaimId::new("c2"),
            to: ClaimId::new("c1"),
            kind: RelationKind::Contradicts,
            confidence: 0.9,
        }];
        let mut standing = BTreeMap::new();
        standing.insert(ClaimId::new("c1"), 0.5);
        standing.insert(ClaimId::new("c2"), 0.8);
        let ranked = vec![DisputeRank {
            claim_id: ClaimId::new("c1"),
            priority: 0.9,
            contested_mass: 0.8,
            decision_leverage: 0.1,
            evidence_gap: 0.2,
            resolution_cost: 0.0,
        }];

        let registry = ProviderRegistry::default();
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let stage_ctx = ctx(&registry, &budget, &cache, &sink, 1);

        let stage = ChallengePlan::new(1, Cost(0.05), Cost(0.10), 2);
        let out = stage
            .run(resolved(claims, relations, standing, ranked), &stage_ctx)
            .await
            .unwrap();
        assert!(out.pairs.is_empty());
    }

    #[tokio::test]
    async fn the_strongest_cross_model_attacker_is_chosen_as_challenger() {
        let claims = vec![
            claim("defended", "the defended claim", "model-a"),
            claim("weak_attacker", "a weak objection", "model-b"),
            claim("strong_attacker", "a strong objection", "model-c"),
        ];
        let relations = vec![
            Relation {
                from: ClaimId::new("weak_attacker"),
                to: ClaimId::new("defended"),
                kind: RelationKind::Contradicts,
                confidence: 0.9,
            },
            Relation {
                from: ClaimId::new("strong_attacker"),
                to: ClaimId::new("defended"),
                kind: RelationKind::Contradicts,
                confidence: 0.9,
            },
        ];
        let mut standing = BTreeMap::new();
        standing.insert(ClaimId::new("defended"), 0.5);
        standing.insert(ClaimId::new("weak_attacker"), 0.2); // score 0.18
        standing.insert(ClaimId::new("strong_attacker"), 0.9); // score 0.81
        let ranked = vec![DisputeRank {
            claim_id: ClaimId::new("defended"),
            priority: 0.9,
            contested_mass: 0.5,
            decision_leverage: 0.1,
            evidence_gap: 0.2,
            resolution_cost: 0.0,
        }];

        let registry = ProviderRegistry::default();
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let stage_ctx = ctx(&registry, &budget, &cache, &sink, 1);

        let stage = ChallengePlan::new(1, Cost(0.05), Cost(0.10), 2);
        let out = stage
            .run(resolved(claims, relations, standing, ranked), &stage_ctx)
            .await
            .unwrap();
        assert_eq!(out.pairs.len(), 1);
        assert_eq!(out.pairs[0].challenger, ModelId::new("model-c"));
        assert_eq!(
            out.pairs[0].attacking_claim_id,
            ClaimId::new("strong_attacker")
        );
    }

    #[tokio::test]
    async fn a_model_already_at_the_per_model_cap_is_skipped_in_favor_of_the_next() {
        // Two disputes, both best-attacked by model-b; the cap (1) forces the
        // second dispute to fall back to its next-best (distinct) attacker.
        let claims = vec![
            claim("d1", "first defended claim", "model-a"),
            claim("d2", "second defended claim", "model-a"),
            claim("attacker_b1", "b's first objection", "model-b"),
            claim("attacker_b2", "b's second objection", "model-b"),
            claim("attacker_c", "c's objection", "model-c"),
        ];
        let relations = vec![
            Relation {
                from: ClaimId::new("attacker_b1"),
                to: ClaimId::new("d1"),
                kind: RelationKind::Contradicts,
                confidence: 0.9,
            },
            Relation {
                from: ClaimId::new("attacker_b2"),
                to: ClaimId::new("d2"),
                kind: RelationKind::Contradicts,
                confidence: 0.9,
            },
            Relation {
                from: ClaimId::new("attacker_c"),
                to: ClaimId::new("d2"),
                kind: RelationKind::Contradicts,
                confidence: 0.5,
            },
        ];
        let mut standing = BTreeMap::new();
        for id in ["d1", "d2", "attacker_b1", "attacker_b2", "attacker_c"] {
            standing.insert(ClaimId::new(id), 0.9);
        }
        let ranked = vec![
            DisputeRank {
                claim_id: ClaimId::new("d1"),
                priority: 0.9,
                contested_mass: 0.5,
                decision_leverage: 0.1,
                evidence_gap: 0.2,
                resolution_cost: 0.0,
            },
            DisputeRank {
                claim_id: ClaimId::new("d2"),
                priority: 0.8,
                contested_mass: 0.5,
                decision_leverage: 0.1,
                evidence_gap: 0.2,
                resolution_cost: 0.0,
            },
        ];

        let registry = ProviderRegistry::default();
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let stage_ctx = ctx(&registry, &budget, &cache, &sink, 1);

        // max_challenges_per_model = 1 -- model-b can only be picked once.
        let stage = ChallengePlan::new(1, Cost(0.05), Cost(0.10), 1);
        let out = stage
            .run(resolved(claims, relations, standing, ranked), &stage_ctx)
            .await
            .unwrap();
        assert_eq!(out.pairs.len(), 2);
        assert_eq!(out.pairs[0].challenger, ModelId::new("model-b"));
        assert_eq!(
            out.pairs[1].challenger,
            ModelId::new("model-c"),
            "model-b is already at its cap of 1, so d2 falls back to model-c"
        );
    }

    #[tokio::test]
    async fn the_challenge_budget_reserves_the_judges_share_first_and_stops_spending() {
        let claims = vec![
            claim("d1", "first", "model-a"),
            claim("a1", "attacker one", "model-b"),
            claim("d2", "second", "model-c"),
            claim("a2", "attacker two", "model-d"),
        ];
        let relations = vec![
            Relation {
                from: ClaimId::new("a1"),
                to: ClaimId::new("d1"),
                kind: RelationKind::Contradicts,
                confidence: 0.9,
            },
            Relation {
                from: ClaimId::new("a2"),
                to: ClaimId::new("d2"),
                kind: RelationKind::Contradicts,
                confidence: 0.9,
            },
        ];
        let mut standing = BTreeMap::new();
        for id in ["d1", "a1", "d2", "a2"] {
            standing.insert(ClaimId::new(id), 0.9);
        }
        let ranked = vec![
            DisputeRank {
                claim_id: ClaimId::new("d1"),
                priority: 0.9,
                contested_mass: 0.5,
                decision_leverage: 0.1,
                evidence_gap: 0.2,
                resolution_cost: 0.0,
            },
            DisputeRank {
                claim_id: ClaimId::new("d2"),
                priority: 0.8,
                contested_mass: 0.5,
                decision_leverage: 0.1,
                evidence_gap: 0.2,
                resolution_cost: 0.0,
            },
        ];

        let registry = ProviderRegistry::default();
        // $0.20 total, one round: round_budget = 0.20. Judge reserves 0.10,
        // leaving 0.10 -- exactly one $0.05 exchange fits comfortably, a
        // second brings cumulative spend to 0.10 (still <= 0.10, so it also
        // fits) -- tightened below to actually force a cutoff.
        let budget = BudgetLedger::new(Some(Cost(0.16)));
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let stage_ctx = ctx(&registry, &budget, &cache, &sink, 1);

        // round_budget = 0.16, judge_reservation = 0.10 -> challenge_budget = 0.06.
        // One $0.05 exchange fits; a second would need cumulative 0.10 > 0.06.
        let stage = ChallengePlan::new(1, Cost(0.05), Cost(0.10), 2);
        let out = stage
            .run(resolved(claims, relations, standing, ranked), &stage_ctx)
            .await
            .unwrap();
        assert_eq!(
            out.pairs.len(),
            1,
            "only the higher-priority dispute fits inside the judge-reserved budget"
        );
        assert_eq!(out.pairs[0].claim_id, ClaimId::new("d1"));
    }

    #[tokio::test]
    async fn remaining_rounds_divides_the_budget_evenly_across_rounds_left() {
        let claims = vec![
            claim("d1", "first", "model-a"),
            claim("a1", "attacker", "model-b"),
        ];
        let relations = vec![Relation {
            from: ClaimId::new("a1"),
            to: ClaimId::new("d1"),
            kind: RelationKind::Contradicts,
            confidence: 0.9,
        }];
        let mut standing = BTreeMap::new();
        standing.insert(ClaimId::new("d1"), 0.9);
        standing.insert(ClaimId::new("a1"), 0.9);
        let ranked = vec![DisputeRank {
            claim_id: ClaimId::new("d1"),
            priority: 0.9,
            contested_mass: 0.5,
            decision_leverage: 0.1,
            evidence_gap: 0.2,
            resolution_cost: 0.0,
        }];

        let registry = ProviderRegistry::default();
        // $3.00 available, max_rounds=3, currently round 1 -> remaining_rounds=3
        // -> round_budget=1.00 -> minus judge 0.10 -> challenge_budget=0.90,
        // comfortably affording one $0.05 exchange.
        let budget = BudgetLedger::new(Some(Cost(3.00)));
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let stage_ctx = ctx(&registry, &budget, &cache, &sink, 1);

        let stage = ChallengePlan::new(3, Cost(0.05), Cost(0.10), 2);
        let out = stage
            .run(resolved(claims, relations, standing, ranked), &stage_ctx)
            .await
            .unwrap();
        assert_eq!(out.pairs.len(), 1);
    }
}
