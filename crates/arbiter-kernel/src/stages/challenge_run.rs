//! `challenge.run` (ARCHITECTURE §5's own table: "issue challenges in
//! parallel"). Takes `challenge.plan`'s selected pairs and actually calls
//! each chosen challenger — the first stage since `positions.generate` to
//! fan out `PerItem`, for the same reason: independent calls to independent
//! models, none waiting on any other.

use super::challenge_plan::ChallengePlanned;
use super::disputes_rank::RankedDisputes;
use crate::event::EventType;
use crate::ids::{CallId, ReservationId, StageName};
use crate::prompt::PromptTemplate;
use crate::provider::ProviderRequest;
use crate::stage::{
    CostEstimate, FailurePolicy, Key, Parallelism, RunContext, Stage, StageContext, StageError,
    idempotency_key,
};
use crate::store::{Artifact, CacheKey, CachedResponse, Cost};
use arbiter_core::{ClaimId, ClaimLifecycle, ModelId, ProviderId};
use futures_util::StreamExt;
use std::collections::{BTreeMap, BTreeSet};

/// One challenge that actually went out and got a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedChallenge {
    pub claim_id: ClaimId,
    pub defenders: Vec<ModelId>,
    pub challenger: ModelId,
    pub challenger_provider: ProviderId,
    pub attacking_claim_id: ClaimId,
    pub challenge_text: String,
}

/// `Stage::Out`. `resolved` carries `disputes.rank`'s output forward
/// unchanged (same "resolved graph, carried forward" shape D36/D37
/// established) — `rebuttal.run` needs the claim texts and standing again to
/// build its own prompts and to apply lifecycle deltas.
#[derive(Debug, Clone, PartialEq)]
pub struct ChallengesIssued {
    pub resolved: RankedDisputes,
    pub challenges: Vec<IssuedChallenge>,
}

impl Artifact for ChallengesIssued {
    fn artifact_type(&self) -> &'static str {
        "challenges_issued.v1"
    }
    fn content_hash(&self) -> String {
        let rows: Vec<serde_json::Value> = self
            .challenges
            .iter()
            .map(|c| {
                serde_json::json!({
                    "claim_id": c.claim_id.as_str(),
                    "challenger": c.challenger.as_str(),
                    "attacking_claim_id": c.attacking_claim_id.as_str(),
                    "challenge_text": c.challenge_text,
                })
            })
            .collect();
        let combined = format!(
            "{}\u{1}{}\u{1}{}",
            self.artifact_type(),
            self.resolved.content_hash(),
            serde_json::to_string(&rows).expect("challenges serialize"),
        );
        format!("blake3:{}", blake3::hash(combined.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "resolved": self.resolved.to_json(),
            "challenges": self.challenges.iter().map(|c| serde_json::json!({
                "claim_id": c.claim_id.as_str(),
                "defenders": c.defenders.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
                "challenger": c.challenger.as_str(),
                "challenger_provider": c.challenger_provider.as_str(),
                "attacking_claim_id": c.attacking_claim_id.as_str(),
                "challenge_text": c.challenge_text,
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug)]
pub struct ChallengeRun {
    template: PromptTemplate,
    estimated_cost_per_call: Cost,
    max_parallelism: usize,
}

impl ChallengeRun {
    pub fn new(
        template: PromptTemplate,
        estimated_cost_per_call: Cost,
        max_parallelism: usize,
    ) -> Self {
        Self {
            template,
            estimated_cost_per_call,
            max_parallelism: max_parallelism.max(1),
        }
    }

    async fn issue_one(
        &self,
        pair: &super::challenge_plan::ChallengePair,
        claim_text: &str,
        attacking_text: &str,
        ctx: &StageContext<'_>,
    ) -> Option<IssuedChallenge> {
        if ctx.cancel.is_cancelled() {
            return None;
        }

        let mut vars = BTreeMap::new();
        vars.insert("claim".to_string(), claim_text.to_string());
        vars.insert("objection".to_string(), attacking_text.to_string());
        let rendered = self.template.render(&vars).ok()?;
        let prompt_hash = self.template.prompt_hash(&rendered).to_string();

        let cache_key = CacheKey {
            provider: pair.challenger_provider.clone(),
            model: pair.challenger.clone(),
            params: "{}".to_string(),
            prompt_hash: prompt_hash.clone(),
        };
        if let Some(cached) = ctx.cache.get(&cache_key)
            && let Some(text) = cached.inline
        {
            ctx.events.emit(
                EventType::ChallengeIssued,
                &self.name(),
                serde_json::json!({
                    "claim_id": pair.claim_id.as_str(),
                    "challenger": pair.challenger.as_str(),
                    "cache_hit": true,
                }),
            );
            return Some(IssuedChallenge {
                claim_id: pair.claim_id.clone(),
                defenders: pair.defenders.clone(),
                challenger: pair.challenger.clone(),
                challenger_provider: pair.challenger_provider.clone(),
                attacking_claim_id: pair.attacking_claim_id.clone(),
                challenge_text: text,
            });
        }

        let reservation_id = ReservationId::new(format!(
            "res_{}_{}_{}",
            self.name(),
            pair.claim_id.as_str(),
            pair.challenger.as_str()
        ));
        let guard = ctx
            .budget
            .reserve(reservation_id.clone(), self.estimated_cost_per_call)
            .ok()?;
        ctx.events.emit(
            EventType::BudgetReserved,
            &self.name(),
            serde_json::json!({"reservation_id": reservation_id.as_str(), "estimate": self.estimated_cost_per_call.0}),
        );

        let provider = ctx.providers.get(&pair.challenger_provider)?;
        let call_id = CallId::new(format!(
            "call_{}_{}_{}",
            self.name(),
            pair.claim_id.as_str(),
            pair.challenger.as_str()
        ));
        let request = ProviderRequest {
            model: pair.challenger.clone(),
            prompt: rendered,
            params: "{}".to_string(),
            idempotency_key: None,
            reservation: reservation_id.clone(),
        };
        ctx.events.emit(
            EventType::CallStarted,
            &self.name(),
            serde_json::json!({
                "call_id": call_id.as_str(),
                "prompt_hash": prompt_hash,
                "reservation_id": reservation_id.as_str(),
                "estimate": self.estimated_cost_per_call.0,
            }),
        );
        guard.mark_sent();

        let response = provider.call(request).await.ok()?;
        if let Some(request_id) = &response.request_id {
            ctx.events.emit(
                EventType::CallRequestId,
                &self.name(),
                serde_json::json!({"call_id": call_id.as_str(), "request_id": request_id}),
            );
            guard.mark_acknowledged();
        }
        let actual_cost = self.estimated_cost_per_call;
        let released_remainder = guard.commit(actual_cost);
        let response_hash = format!("blake3:{}", blake3::hash(response.text.as_bytes()).to_hex());
        ctx.events.emit(
            EventType::CallCompleted,
            &self.name(),
            serde_json::json!({"call_id": call_id.as_str(), "response_hash": response_hash, "actual_cost": actual_cost.0}),
        );
        ctx.events.emit(
            EventType::BudgetCommitted,
            &self.name(),
            serde_json::json!({
                "reservation_id": reservation_id.as_str(),
                "actual_cost": actual_cost.0,
                "released_remainder": released_remainder.0,
            }),
        );

        ctx.cache.put(
            cache_key,
            CachedResponse {
                response_hash,
                size_bytes: response.text.len() as u64,
                inline: Some(response.text.clone()),
            },
        );

        ctx.events.emit(
            EventType::ChallengeIssued,
            &self.name(),
            serde_json::json!({
                "claim_id": pair.claim_id.as_str(),
                "challenger": pair.challenger.as_str(),
                "cache_hit": false,
            }),
        );

        Some(IssuedChallenge {
            claim_id: pair.claim_id.clone(),
            defenders: pair.defenders.clone(),
            challenger: pair.challenger.clone(),
            challenger_provider: pair.challenger_provider.clone(),
            attacking_claim_id: pair.attacking_claim_id.clone(),
            challenge_text: response.text,
        })
    }
}

impl Stage for ChallengeRun {
    type In = ChallengePlanned;
    type Out = ChallengesIssued;

    fn name(&self) -> StageName {
        StageName::new("challenge.run")
    }

    fn parallelism(&self) -> Parallelism {
        Parallelism::PerItem {
            max: self.max_parallelism,
        }
    }

    fn failure_policy(&self) -> FailurePolicy {
        FailurePolicy::SkipItem
    }

    fn idempotency_key(&self, input: &Self::In, ctx: &RunContext) -> Key {
        idempotency_key(&self.name(), ctx, &[input.content_hash()])
    }

    fn cost_estimate(&self, input: &Self::In) -> CostEstimate {
        let n = input.pairs.len() as u32;
        CostEstimate {
            calls: n,
            tokens: 0,
            cost: Cost(self.estimated_cost_per_call.0 * n as f64),
        }
    }

    async fn run(&self, input: Self::In, ctx: &StageContext<'_>) -> Result<Self::Out, StageError> {
        ctx.events.emit(
            EventType::StageStarted,
            &self.name(),
            serde_json::json!({"pairs": input.pairs.len()}),
        );

        let claim_text: BTreeMap<ClaimId, String> = input
            .resolved
            .claims
            .0
            .iter()
            .map(|c| (c.id.clone(), c.text.clone()))
            .collect();

        let mut challenges: Vec<IssuedChallenge> =
            futures_util::stream::iter(input.pairs.iter().cloned())
                .map(|pair| {
                    let claim_text_str =
                        claim_text.get(&pair.claim_id).cloned().unwrap_or_default();
                    let attacking_text = claim_text
                        .get(&pair.attacking_claim_id)
                        .cloned()
                        .unwrap_or_default();
                    async move {
                        self.issue_one(&pair, &claim_text_str, &attacking_text, ctx)
                            .await
                    }
                })
                .buffer_unordered(self.max_parallelism)
                .filter_map(std::future::ready)
                .collect()
                .await;

        challenges.sort_by(|a, b| {
            (a.claim_id.as_str(), a.challenger.as_str())
                .cmp(&(b.claim_id.as_str(), b.challenger.as_str()))
        });

        // A claim that actually received a challenge transitions to
        // `Challenged` now — the state exists precisely for "has an open
        // challenge, outcome not yet known" (ARCHITECTURE §6.1's lifecycle),
        // and `rebuttal.run` only ever moves it further (Defended/Modified/
        // Withdrawn); a claim whose call failed simply never reaches this
        // set and stays at whatever lifecycle it already had.
        let challenged_ids: BTreeSet<ClaimId> =
            challenges.iter().map(|c| c.claim_id.clone()).collect();
        let mut claims = input.resolved.claims.clone();
        for claim in &mut claims.0 {
            if challenged_ids.contains(&claim.id) && claim.lifecycle == ClaimLifecycle::Proposed {
                claim.lifecycle = ClaimLifecycle::Challenged;
            }
        }
        let mut resolved = input.resolved;
        resolved.claims = claims;

        ctx.events.emit(
            EventType::StageCompleted,
            &self.name(),
            serde_json::json!({"issued": challenges.len()}),
        );

        Ok(ChallengesIssued {
            resolved,
            challenges,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetLedger;
    use crate::cache::ResponseCache;
    use crate::provider::{Provider, ProviderCapabilities, ProviderError, ProviderResponse};
    use crate::stage::{CancellationToken, DeterministicRng, EventSink, ProviderRegistry};
    use crate::stages::challenge_plan::ChallengePair;
    use crate::stages::claims_normalize::NormalizedClaims;
    use crate::stages::disputes_rank::DisputeRank;
    use crate::stages::options_cluster::ClusteredOptions;
    use crate::stages::relations_analyze::AnalyzedRelations;
    use arbiter_core::{
        AttachmentMatrix, CanonicalClaim, ClaimMember, EvidenceKind, Grounding, PositionId,
        TextSpan,
    };
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;

    #[derive(Debug)]
    struct ScriptedProvider {
        id: ProviderId,
        script: StdMutex<VecDeque<Result<ProviderResponse, ProviderError>>>,
    }
    impl ScriptedProvider {
        fn new(id: ProviderId) -> Self {
            Self {
                id,
                script: StdMutex::new(VecDeque::new()),
            }
        }
        fn script_text(&self, text: &str) {
            self.script.lock().unwrap().push_back(Ok(ProviderResponse {
                text: text.to_string(),
                prompt_tokens: 0,
                completion_tokens: 0,
                request_id: None,
            }));
        }
    }
    impl Provider for ScriptedProvider {
        fn id(&self) -> ProviderId {
            self.id.clone()
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                structured_output: false,
                streaming: false,
                idempotency: None,
            }
        }
        fn call(
            &self,
            _request: ProviderRequest,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send + '_>>
        {
            Box::pin(async move {
                self.script
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| Err(ProviderError::Other("script exhausted".to_string())))
            })
        }
    }

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

    fn template() -> PromptTemplate {
        PromptTemplate {
            stage: StageName::new("challenge.issue"),
            body: "Claim: {{claim}}\nObjection: {{objection}}".to_string(),
            variables: crate::prompt::VariableSchema::new(["claim", "objection"]),
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

    fn planned(pairs: Vec<ChallengePair>, claims: Vec<CanonicalClaim>) -> ChallengePlanned {
        ChallengePlanned {
            resolved: RankedDisputes {
                claims: NormalizedClaims(claims),
                relations: AnalyzedRelations(vec![]),
                options: ClusteredOptions {
                    options: vec![],
                    direct_matrix: AttachmentMatrix::default(),
                },
                standing: BTreeMap::new(),
                propagated_matrix: AttachmentMatrix::default(),
                ranked: vec![DisputeRank {
                    claim_id: ClaimId::new("defended"),
                    priority: 0.9,
                    contested_mass: 0.5,
                    decision_leverage: 0.1,
                    evidence_gap: 0.2,
                    resolution_cost: 0.0,
                }],
            },
            pairs,
        }
    }

    fn pair() -> ChallengePair {
        ChallengePair {
            claim_id: ClaimId::new("defended"),
            priority: 0.9,
            defenders: vec![ModelId::new("model-a")],
            challenger: ModelId::new("model-b"),
            challenger_provider: ProviderId::new("model-b-provider"),
            attacking_claim_id: ClaimId::new("attacker"),
            estimated_cost: Cost(0.05),
        }
    }

    #[tokio::test]
    async fn a_successful_call_produces_an_issued_challenge_and_marks_the_claim_challenged() {
        let mock = ScriptedProvider::new(ProviderId::new("model-b-provider"));
        mock.script_text("this claim overlooks X");
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let stage_ctx = StageContext {
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
            claim("defended", "the defended claim", "model-a"),
            claim("attacker", "the attacking claim", "model-b"),
        ];
        let input = planned(vec![pair()], claims);
        let stage = ChallengeRun::new(template(), Cost(0.05), 4);
        let out = stage.run(input, &stage_ctx).await.unwrap();

        assert_eq!(out.challenges.len(), 1);
        assert_eq!(out.challenges[0].challenge_text, "this claim overlooks X");
        let defended = out
            .resolved
            .claims
            .0
            .iter()
            .find(|c| c.id == ClaimId::new("defended"))
            .unwrap();
        assert_eq!(defended.lifecycle, ClaimLifecycle::Challenged);
        assert_eq!(sink.count(EventType::ChallengeIssued), 1);
    }

    #[tokio::test]
    async fn a_provider_error_skips_the_pair_and_leaves_the_claim_lifecycle_untouched() {
        let mock = ScriptedProvider::new(ProviderId::new("model-b-provider"));
        // No response scripted: the call errors immediately.
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let stage_ctx = StageContext {
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
            claim("defended", "the defended claim", "model-a"),
            claim("attacker", "the attacking claim", "model-b"),
        ];
        let input = planned(vec![pair()], claims);
        let stage = ChallengeRun::new(template(), Cost(0.05), 4);
        let out = stage.run(input, &stage_ctx).await.unwrap();

        assert!(out.challenges.is_empty());
        let defended = out
            .resolved
            .claims
            .0
            .iter()
            .find(|c| c.id == ClaimId::new("defended"))
            .unwrap();
        assert_eq!(defended.lifecycle, ClaimLifecycle::Proposed);
    }
}
