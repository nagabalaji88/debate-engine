//! `rebuttal.run` (ARCHITECTURE §5's own table: "defend / modify / withdraw →
//! versioned claim deltas"). For each issued challenge, asks the defended
//! claim's own asserting model to respond, then applies the outcome as a
//! `ClaimLifecycle` transition — `Defended`, `Modified{version}`, or
//! `Withdrawn` (ARCHITECTURE §6.1).
//!
//! Output feeds straight back into `disputes.rank`'s own input shape
//! (`RankInput`): the pipeline's controlled loop
//! (`challenge.plan → challenge.run → rebuttal.run → controller.decide`,
//! INTERFACES §11) re-enters `disputes.rank` for the next round with exactly
//! this shape, so building it here rather than a bespoke type means the next
//! round needs no adapter.

use super::challenge_run::ChallengesIssued;
use super::disputes_rank::RankInput;
use crate::event::EventType;
use crate::ids::{CallId, ReservationId, StageName};
use crate::prompt::PromptTemplate;
use crate::provider::ProviderRequest;
use crate::stage::{
    CostEstimate, FailurePolicy, Key, Parallelism, RunContext, Stage, StageContext, StageError,
    idempotency_key,
};
use crate::store::{Artifact, CacheKey, CachedResponse, Cost};
use arbiter_core::{
    ClaimId, ClaimLifecycle, ClaimMember, Grounding, ModelId, PositionId, ProviderId,
};
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuttalKind {
    Defend,
    Modify,
    Withdraw,
}

fn parse_outcome(s: &str) -> Option<RebuttalKind> {
    match s {
        "defend" => Some(RebuttalKind::Defend),
        "modify" => Some(RebuttalKind::Modify),
        "withdraw" => Some(RebuttalKind::Withdraw),
        _ => None,
    }
}

/// The `challenge received → verbatim rebuttal → lifecycle outcome` exchange
/// INTERFACES §4 says the judge's dossier needs per position, recorded here
/// where it is actually produced.
#[derive(Debug, Clone, PartialEq)]
pub struct RebuttalOutcome {
    pub claim_id: ClaimId,
    pub challenger: ModelId,
    pub defender: ModelId,
    pub defender_provider: ProviderId,
    pub challenge_text: String,
    pub rebuttal_text: String,
    pub outcome: RebuttalKind,
    /// The claim's new wording, only ever set alongside `outcome ==
    /// RebuttalKind::Modify`.
    pub revised_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RebuttalsRun {
    /// Feeds directly back into `disputes.rank` for the next round.
    pub next_round_input: RankInput,
    pub outcomes: Vec<RebuttalOutcome>,
}

impl Artifact for RebuttalsRun {
    fn artifact_type(&self) -> &'static str {
        "rebuttals_run.v1"
    }
    fn content_hash(&self) -> String {
        let rows: Vec<serde_json::Value> = self
            .outcomes
            .iter()
            .map(|o| {
                serde_json::json!({
                    "claim_id": o.claim_id.as_str(),
                    "challenger": o.challenger.as_str(),
                    "defender": o.defender.as_str(),
                    "rebuttal_text": o.rebuttal_text,
                    "outcome": format!("{:?}", o.outcome),
                })
            })
            .collect();
        let combined = format!(
            "{}\u{1}{}",
            self.next_round_input.content_hash(),
            serde_json::to_string(&rows).expect("outcomes serialize"),
        );
        format!("blake3:{}", blake3::hash(combined.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "next_round_input": self.next_round_input.to_json(),
            "outcomes": self.outcomes.iter().map(|o| serde_json::json!({
                "claim_id": o.claim_id.as_str(),
                "challenger": o.challenger.as_str(),
                "defender": o.defender.as_str(),
                "defender_provider": o.defender_provider.as_str(),
                "challenge_text": o.challenge_text,
                "rebuttal_text": o.rebuttal_text,
                "outcome": format!("{:?}", o.outcome),
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawRebuttal {
    outcome: String,
    rebuttal_text: String,
    #[serde(default)]
    revised_text: Option<String>,
}

/// `Modified{version}`'s next number: 1 for a claim modified for the first
/// time, `v + 1` for a claim already at `Modified{version: v}` from an
/// earlier round.
fn next_version(current: ClaimLifecycle) -> u32 {
    match current {
        ClaimLifecycle::Modified { version } => version + 1,
        _ => 1,
    }
}

#[derive(Debug)]
pub struct RebuttalRun {
    template: PromptTemplate,
    estimated_cost_per_call: Cost,
    max_parallelism: usize,
}

impl RebuttalRun {
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

    async fn respond_one(
        &self,
        challenge: &super::challenge_run::IssuedChallenge,
        claim_text: &str,
        defender: ModelId,
        defender_provider: ProviderId,
        ctx: &StageContext<'_>,
    ) -> Option<RebuttalOutcome> {
        if ctx.cancel.is_cancelled() {
            return None;
        }

        let mut vars = BTreeMap::new();
        vars.insert("claim".to_string(), claim_text.to_string());
        vars.insert("challenge".to_string(), challenge.challenge_text.clone());
        let rendered = self.template.render(&vars).ok()?;
        let prompt_hash = self.template.prompt_hash(&rendered).to_string();

        let cache_key = CacheKey {
            provider: defender_provider.clone(),
            model: defender.clone(),
            params: "{}".to_string(),
            prompt_hash: prompt_hash.clone(),
        };
        let response_text = if let Some(cached) = ctx.cache.get(&cache_key)
            && let Some(text) = cached.inline
        {
            text
        } else {
            let reservation_id = ReservationId::new(format!(
                "res_{}_{}_{}",
                self.name(),
                challenge.claim_id.as_str(),
                defender.as_str()
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

            let provider = ctx.providers.get(&defender_provider)?;
            let call_id = CallId::new(format!(
                "call_{}_{}_{}",
                self.name(),
                challenge.claim_id.as_str(),
                defender.as_str()
            ));
            let request = ProviderRequest {
                model: defender.clone(),
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
            let response_hash =
                format!("blake3:{}", blake3::hash(response.text.as_bytes()).to_hex());
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
            response.text
        };

        let raw: RawRebuttal = serde_json::from_str(&response_text).ok()?;
        let outcome = parse_outcome(&raw.outcome)?;

        ctx.events.emit(
            EventType::RebuttalReceived,
            &self.name(),
            serde_json::json!({
                "claim_id": challenge.claim_id.as_str(),
                "defender": defender.as_str(),
                "outcome": raw.outcome,
            }),
        );

        Some(RebuttalOutcome {
            claim_id: challenge.claim_id.clone(),
            challenger: challenge.challenger.clone(),
            defender,
            defender_provider,
            challenge_text: challenge.challenge_text.clone(),
            rebuttal_text: raw.rebuttal_text,
            outcome,
            revised_text: raw.revised_text,
        })
    }
}

impl Stage for RebuttalRun {
    type In = ChallengesIssued;
    type Out = RebuttalsRun;

    fn name(&self) -> StageName {
        StageName::new("rebuttal.run")
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
        let n = input.challenges.len() as u32;
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
            serde_json::json!({"challenges": input.challenges.len()}),
        );

        let claim_text: BTreeMap<ClaimId, String> = input
            .resolved
            .claims
            .0
            .iter()
            .map(|c| (c.id.clone(), c.text.clone()))
            .collect();
        let member_provider: BTreeMap<(ClaimId, ModelId), ProviderId> = input
            .resolved
            .claims
            .0
            .iter()
            .flat_map(|c| {
                c.members
                    .iter()
                    .map(move |m| ((c.id.clone(), m.model.clone()), m.provider.clone()))
            })
            .collect();

        let mut outcomes: Vec<RebuttalOutcome> =
            futures_util::stream::iter(input.challenges.iter().cloned())
                .map(|challenge| {
                    let claim_text_str = claim_text.get(&challenge.claim_id).cloned();
                    // "The claim's author(s)" (D37's plural reading) --
                    // rebuttal.run addresses one representative defender,
                    // the first asserting model in deterministic order,
                    // rather than one call per co-asserting model: the
                    // debate defends a claim, not each model's copy of it
                    // separately (PLAN_DEVIATIONS.md D38).
                    let defender = challenge.defenders.first().cloned();
                    let defender_provider = defender.as_ref().and_then(|d| {
                        member_provider
                            .get(&(challenge.claim_id.clone(), d.clone()))
                            .cloned()
                    });
                    async move {
                        let (claim_text_str, defender, defender_provider) =
                            (claim_text_str?, defender?, defender_provider?);
                        self.respond_one(
                            &challenge,
                            &claim_text_str,
                            defender,
                            defender_provider,
                            ctx,
                        )
                        .await
                    }
                })
                .buffer_unordered(self.max_parallelism)
                .filter_map(std::future::ready)
                .collect()
                .await;

        outcomes.sort_by(|a, b| a.claim_id.as_str().cmp(b.claim_id.as_str()));

        let by_claim: BTreeMap<ClaimId, &RebuttalOutcome> =
            outcomes.iter().map(|o| (o.claim_id.clone(), o)).collect();

        let mut claims = input.resolved.claims.clone();
        for claim in &mut claims.0 {
            let Some(outcome) = by_claim.get(&claim.id) else {
                continue;
            };
            match outcome.outcome {
                RebuttalKind::Defend => {
                    claim.lifecycle = ClaimLifecycle::Defended;
                }
                RebuttalKind::Withdraw => {
                    claim.lifecycle = ClaimLifecycle::Withdrawn;
                }
                RebuttalKind::Modify => {
                    let version = next_version(claim.lifecycle);
                    claim.lifecycle = ClaimLifecycle::Modified { version };
                    // Originals are never destroyed (ARCHITECTURE §5.2): the
                    // revision is appended as a new member rather than
                    // overwriting an existing one's `original_text`. No
                    // grounding pipeline re-runs here (that is
                    // claims.extract's own job, not this stage's, D38) --
                    // admitted at `Unsupported` weight, the same
                    // conservative floor `claims.extract` uses for anything
                    // it cannot verify.
                    let revised_text = outcome
                        .revised_text
                        .clone()
                        .unwrap_or_else(|| outcome.rebuttal_text.clone());
                    claim.members.push(ClaimMember::new(
                        claim.id.clone(),
                        outcome.defender.clone(),
                        outcome.defender_provider.clone(),
                        PositionId::new(format!("pos_rebuttal_{}_{}", claim.id.as_str(), version)),
                        revised_text,
                        Grounding::Unsupported,
                    ));
                }
            }
        }

        let next_round_input = RankInput {
            claims,
            relations: input.resolved.relations,
            options: input.resolved.options,
        };

        ctx.events.emit(
            EventType::StageCompleted,
            &self.name(),
            serde_json::json!({"outcomes": outcomes.len()}),
        );

        Ok(RebuttalsRun {
            next_round_input,
            outcomes,
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
    use crate::stages::challenge_run::IssuedChallenge;
    use crate::stages::claims_normalize::NormalizedClaims;
    use crate::stages::disputes_rank::{DisputeRank, RankedDisputes};
    use crate::stages::options_cluster::ClusteredOptions;
    use crate::stages::relations_analyze::AnalyzedRelations;
    use arbiter_core::{AttachmentMatrix, CanonicalClaim, EvidenceKind, TextSpan};
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
        fn script_json(&self, value: serde_json::Value) {
            self.script.lock().unwrap().push_back(Ok(ProviderResponse {
                text: value.to_string(),
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

    fn template() -> PromptTemplate {
        PromptTemplate {
            stage: StageName::new("rebuttal.respond"),
            body: "Claim: {{claim}}\nChallenge: {{challenge}}".to_string(),
            variables: crate::prompt::VariableSchema::new(["claim", "challenge"]),
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

    fn issued(claims: Vec<CanonicalClaim>, challenges: Vec<IssuedChallenge>) -> ChallengesIssued {
        ChallengesIssued {
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
            challenges,
        }
    }

    fn challenge() -> IssuedChallenge {
        IssuedChallenge {
            claim_id: ClaimId::new("defended"),
            defenders: vec![ModelId::new("model-a")],
            challenger: ModelId::new("model-b"),
            challenger_provider: ProviderId::new("model-b-provider"),
            attacking_claim_id: ClaimId::new("attacker"),
            challenge_text: "this overlooks X".to_string(),
        }
    }

    fn ctx<'a>(
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
    async fn a_defend_verdict_moves_the_claim_to_defended() {
        let mock = ScriptedProvider::new(ProviderId::new("model-a-provider"));
        mock.script_json(serde_json::json!({
            "outcome": "defend",
            "rebuttal_text": "the challenge misreads the claim",
        }));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let stage_ctx = ctx(&registry, &budget, &cache, &sink);

        let claims = vec![claim(
            "defended",
            "the defended claim",
            "model-a",
            ClaimLifecycle::Challenged,
        )];
        let input = issued(claims, vec![challenge()]);
        let stage = RebuttalRun::new(template(), Cost(0.05), 4);
        let out = stage.run(input, &stage_ctx).await.unwrap();

        assert_eq!(out.outcomes.len(), 1);
        assert_eq!(out.outcomes[0].outcome, RebuttalKind::Defend);
        let defended = out
            .next_round_input
            .claims
            .0
            .iter()
            .find(|c| c.id == ClaimId::new("defended"))
            .unwrap();
        assert_eq!(defended.lifecycle, ClaimLifecycle::Defended);
    }

    #[tokio::test]
    async fn a_withdraw_verdict_moves_the_claim_to_withdrawn() {
        let mock = ScriptedProvider::new(ProviderId::new("model-a-provider"));
        mock.script_json(serde_json::json!({
            "outcome": "withdraw",
            "rebuttal_text": "fair point, retracting this",
        }));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let stage_ctx = ctx(&registry, &budget, &cache, &sink);

        let claims = vec![claim(
            "defended",
            "the defended claim",
            "model-a",
            ClaimLifecycle::Challenged,
        )];
        let input = issued(claims, vec![challenge()]);
        let stage = RebuttalRun::new(template(), Cost(0.05), 4);
        let out = stage.run(input, &stage_ctx).await.unwrap();

        let defended = out
            .next_round_input
            .claims
            .0
            .iter()
            .find(|c| c.id == ClaimId::new("defended"))
            .unwrap();
        assert_eq!(defended.lifecycle, ClaimLifecycle::Withdrawn);
    }

    #[tokio::test]
    async fn a_modify_verdict_bumps_the_version_and_appends_a_member_without_destroying_originals()
    {
        let mock = ScriptedProvider::new(ProviderId::new("model-a-provider"));
        mock.script_json(serde_json::json!({
            "outcome": "modify",
            "rebuttal_text": "revising to account for the objection",
            "revised_text": "the defended claim, with a caveat",
        }));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let stage_ctx = ctx(&registry, &budget, &cache, &sink);

        let claims = vec![claim(
            "defended",
            "the defended claim",
            "model-a",
            ClaimLifecycle::Challenged,
        )];
        let original_member_count = claims[0].members.len();
        let input = issued(claims, vec![challenge()]);
        let stage = RebuttalRun::new(template(), Cost(0.05), 4);
        let out = stage.run(input, &stage_ctx).await.unwrap();

        let defended = out
            .next_round_input
            .claims
            .0
            .iter()
            .find(|c| c.id == ClaimId::new("defended"))
            .unwrap();
        assert_eq!(defended.lifecycle, ClaimLifecycle::Modified { version: 1 });
        assert_eq!(
            defended.members.len(),
            original_member_count + 1,
            "the original member must survive, plus one new one"
        );
        assert_eq!(
            defended.text, "the defended claim",
            "canonical text is not rewritten (D38)"
        );
    }

    #[tokio::test]
    async fn an_unparseable_response_leaves_the_claim_unchanged_and_produces_no_outcome() {
        let mock = ScriptedProvider::new(ProviderId::new("model-a-provider"));
        mock.script_json(serde_json::json!("not valid json for RawRebuttal"));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let stage_ctx = ctx(&registry, &budget, &cache, &sink);

        let claims = vec![claim(
            "defended",
            "the defended claim",
            "model-a",
            ClaimLifecycle::Challenged,
        )];
        let input = issued(claims, vec![challenge()]);
        let stage = RebuttalRun::new(template(), Cost(0.05), 4);
        let out = stage.run(input, &stage_ctx).await.unwrap();

        assert!(out.outcomes.is_empty());
        let defended = out
            .next_round_input
            .claims
            .0
            .iter()
            .find(|c| c.id == ClaimId::new("defended"))
            .unwrap();
        assert_eq!(defended.lifecycle, ClaimLifecycle::Challenged);
    }
}
