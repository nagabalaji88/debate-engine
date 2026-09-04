//! `options.cluster` (INTERFACES §20, "the last algorithmic gap"): derives
//! the candidate recommendations (`DecisionOption`s) from positions, and
//! attaches claims to them.
//!
//! Only INTERFACES §20's Steps 1–2 happen here. Step 3 (deterministic
//! propagation through the relation graph) is already fully implemented and
//! tested in `arbiter_core::decision::attachment::propagate` — but it takes
//! `relations: &[Relation]`, which do not exist until `relations.analyze`
//! runs, one stage *after* this one in the pipeline
//! (`options.cluster → relations.analyze`). So this stage's own output is
//! the **direct** matrix only (`Authored`/`Classified` cells); calling
//! `propagate` on it is whichever later stage first has both a matrix and a
//! relation graph in hand to call it with — outside this task's scope.

use super::claims_normalize::NormalizedClaims;
use super::positions_generate::Positions;
use crate::event::EventType;
use crate::ids::{CallId, ReservationId, StageName};
use crate::prompt::PromptTemplate;
use crate::provider::ProviderRequest;
use crate::stage::{
    CostEstimate, FailurePolicy, Key, Parallelism, RunContext, Stage, StageContext, StageError,
    idempotency_key,
};
use crate::store::{Artifact, Cost};
use arbiter_core::decision::attachment::{AttachSource, Attachment, AttachmentMatrix, Polarity};
use arbiter_core::{ClaimId, DecisionOption, ModelId, OptionId, ProviderId};
use serde::Deserialize;
use std::collections::BTreeMap;

/// This stage's own combined input — `options.cluster` is the first stage
/// needing more than one upstream artifact (`positions: &[Position]` for
/// clustering, `claims: &[CanonicalClaim]` for attachment; INTERFACES §20's
/// own `OptionClusterer` trait takes them as two separate method arguments,
/// not one `Stage::In`). K3's `Stage` trait has exactly one associated `In`
/// type (INTERFACES §6, copied verbatim) with no provision for a
/// multi-artifact stage — neither spec file addresses this gap since neither
/// gives the executor's own wiring. Resolved with a small combining wrapper
/// rather than changing `Stage`'s shape itself (PLAN_DEVIATIONS.md D34).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterInput {
    pub positions: Positions,
    pub claims: NormalizedClaims,
}

impl Artifact for ClusterInput {
    fn artifact_type(&self) -> &'static str {
        "options_cluster_input.v1"
    }
    fn content_hash(&self) -> String {
        let combined = format!(
            "{}\u{1}{}\u{1}{}",
            self.artifact_type(),
            self.positions.content_hash(),
            self.claims.content_hash()
        );
        format!("blake3:{}", blake3::hash(combined.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"positions": self.positions.to_json(), "claims": self.claims.to_json()})
    }
}

/// `options.cluster`'s output: the clustered options, and the *direct*
/// attachment matrix (`Authored`/`Classified` cells only — see this module's
/// own doc comment for why `Propagated` cells are not this stage's job).
#[derive(Debug, Clone, PartialEq)]
pub struct ClusteredOptions {
    pub options: Vec<DecisionOption>,
    pub direct_matrix: AttachmentMatrix,
}

impl Artifact for ClusteredOptions {
    fn artifact_type(&self) -> &'static str {
        "clustered_options.v1"
    }
    fn content_hash(&self) -> String {
        let mut option_ids: Vec<&str> = self.options.iter().map(|o| o.id.as_str()).collect();
        option_ids.sort_unstable();
        let mut cells: Vec<serde_json::Value> = self
            .direct_matrix
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
        // BTreeMap iteration is already key-ordered, but that order depends
        // on ClaimId/OptionId's own Ord, not this JSON's string form -- sort
        // explicitly so the hash is self-evidently canonical.
        cells.sort_by_key(|a| a.to_string());
        let text = format!(
            "{}\u{1}{}",
            self.artifact_type(),
            serde_json::to_string(&(option_ids, cells)).expect("options serialize")
        );
        format!("blake3:{}", blake3::hash(text.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "options": self.options.iter().map(|o| serde_json::json!({
                "id": o.id.as_str(),
                "label": o.label,
                "version": o.version.as_str(),
                "retired": o.retired,
            })).collect::<Vec<_>>(),
            "cells": self.direct_matrix.cells.iter().map(|((claim, option), a)| serde_json::json!({
                "claim": claim.as_str(),
                "option": option.as_str(),
                "polarity": format!("{:?}", a.polarity),
                "confidence": a.confidence,
                "source": format!("{:?}", a.source),
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawOptionGroup {
    members: Vec<String>,
    label: String,
    #[allow(dead_code)]
    confidence: f64,
}

#[derive(Debug, Deserialize)]
struct RawAttachCell {
    claim: String,
    option: String,
    polarity: String,
    confidence: f64,
}

#[derive(Debug)]
pub struct OptionsCluster {
    cluster_template: PromptTemplate,
    attach_template: PromptTemplate,
    model: (ModelId, ProviderId),
    estimated_cost_per_call: Cost,
    max_claims_per_batch: usize,
}

impl OptionsCluster {
    pub fn new(
        cluster_template: PromptTemplate,
        attach_template: PromptTemplate,
        model: (ModelId, ProviderId),
        estimated_cost_per_call: Cost,
    ) -> Self {
        Self {
            cluster_template,
            attach_template,
            model,
            estimated_cost_per_call,
            max_claims_per_batch: 60,
        }
    }

    async fn call(
        &self,
        template: &PromptTemplate,
        vars: BTreeMap<String, String>,
        ctx: &StageContext<'_>,
        call_label: &str,
    ) -> Option<String> {
        let stage_name = self.name();
        let rendered = template.render(&vars).ok()?;
        let prompt_hash = template.prompt_hash(&rendered).to_string();
        let (model, provider_id) = self.model.clone();

        let cache_key = crate::store::CacheKey {
            provider: provider_id.clone(),
            model: model.clone(),
            params: "{}".to_string(),
            prompt_hash: prompt_hash.clone(),
        };
        if let Some(cached) = ctx.cache.get(&cache_key)
            && let Some(text) = cached.inline
        {
            return Some(text);
        }

        let reservation_id = ReservationId::new(format!(
            "res_{}_{}_{}_{}",
            stage_name,
            call_label,
            provider_id.as_str(),
            model.as_str()
        ));
        let guard = ctx
            .budget
            .reserve(reservation_id.clone(), self.estimated_cost_per_call)
            .ok()?;
        ctx.events.emit(
            EventType::BudgetReserved,
            &stage_name,
            serde_json::json!({"reservation_id": reservation_id.as_str(), "estimate": self.estimated_cost_per_call.0}),
        );

        let provider = ctx.providers.get(&provider_id)?;
        let call_id = CallId::new(format!(
            "call_{}_{}_{}_{}",
            stage_name,
            call_label,
            provider_id.as_str(),
            model.as_str()
        ));
        let request = ProviderRequest {
            model: model.clone(),
            prompt: rendered,
            params: "{}".to_string(),
            idempotency_key: None,
            reservation: reservation_id.clone(),
        };
        ctx.events.emit(
            EventType::CallStarted,
            &stage_name,
            serde_json::json!({
                "call_id": call_id.as_str(),
                "prompt_hash": prompt_hash,
                "reservation_id": reservation_id.as_str(),
                "estimate": self.estimated_cost_per_call.0,
            }),
        );
        guard.mark_sent();

        let response = match provider.call(request).await {
            Ok(r) => r,
            Err(e) => {
                // The reservation is released by the guard's Drop, but the
                // event and the provider's own message have to be raised
                // here or this call vanishes from the record entirely.
                super::emit_budget_released(
                    ctx,
                    &self.name(),
                    &reservation_id,
                    self.estimated_cost_per_call,
                    &e.to_string(),
                );
                return None;
            }
        };
        if let Some(request_id) = &response.request_id {
            ctx.events.emit(
                EventType::CallRequestId,
                &stage_name,
                serde_json::json!({"call_id": call_id.as_str(), "request_id": request_id}),
            );
            guard.mark_acknowledged();
        }
        let actual_cost = self.estimated_cost_per_call;
        guard.commit(actual_cost);
        let response_hash = format!("blake3:{}", blake3::hash(response.text.as_bytes()).to_hex());
        ctx.events.emit(
            EventType::CallCompleted,
            &stage_name,
            serde_json::json!({"call_id": call_id.as_str(), "response_hash": response_hash, "actual_cost": actual_cost.0}),
        );

        ctx.cache.put(
            cache_key,
            crate::store::CachedResponse {
                response_hash,
                size_bytes: response.text.len() as u64,
                inline: Some(response.text.clone()),
            },
        );

        Some(response.text)
    }

    /// Step 1: cluster positions into options. Every position resolves to
    /// exactly one option (its own, if the model returns no usable
    /// response) — "no option is ever invented" is preserved by construction
    /// since every option's label traces back to at least one real position.
    async fn cluster_positions(
        &self,
        positions: &Positions,
        ctx: &StageContext<'_>,
    ) -> Vec<(DecisionOption, Vec<usize>)> {
        let block = positions
            .0
            .iter()
            .enumerate()
            .map(|(i, p)| format!("#{} {}", i + 1, p.text))
            .collect::<Vec<_>>()
            .join("\n\n");

        let mut vars = BTreeMap::new();
        vars.insert("positions".to_string(), block);

        let fallback = || {
            // No usable clustering response: every position becomes its own
            // option, labelled from its own text -- degraded recall, never a
            // lost or invented option.
            positions
                .0
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let id = OptionId::new(format!("opt_{}", p.id.as_str()));
                    (DecisionOption::new(id, p.text.clone()), vec![i])
                })
                .collect::<Vec<_>>()
        };

        let Some(response) = self
            .call(&self.cluster_template, vars, ctx, "options.cluster")
            .await
        else {
            return fallback();
        };
        let Ok(groups) = serde_json::from_str::<Vec<RawOptionGroup>>(&response) else {
            return fallback();
        };

        let mut assigned: Vec<bool> = vec![false; positions.0.len()];
        let mut result = Vec::new();
        for group in groups {
            let members: Vec<usize> = group
                .members
                .iter()
                .filter_map(|m| parse_ref(m, positions.0.len()))
                .filter(|&i| !assigned[i])
                .collect();
            if members.is_empty() {
                continue;
            }
            for &i in &members {
                assigned[i] = true;
            }
            let survivor = positions.0[members[0]].id.clone();
            let id = OptionId::new(format!("opt_{}", survivor.as_str()));
            result.push((DecisionOption::new(id, group.label), members));
        }
        // Any position the response never mentioned still becomes its own
        // option -- "no option is ever invented", and no position is ever
        // silently dropped either.
        for (i, p) in positions.0.iter().enumerate() {
            if !assigned[i] {
                let id = OptionId::new(format!("opt_{}", p.id.as_str()));
                result.push((DecisionOption::new(id, p.text.clone()), vec![i]));
            }
        }
        result
    }

    /// Step 2: seed `Authored` cells from each claim's own position's option,
    /// then run the batched classifier over (claims × options), whose
    /// answer for a given pair overrides the seed (INTERFACES §20: "may be
    /// revised by the classifier"). A `neutral` classifier answer removes any
    /// seeded cell for that exact pair too -- the classifier looked at that
    /// specific pair and found nothing there, which is itself a revision.
    async fn attach(
        &self,
        claims: &NormalizedClaims,
        options: &[(DecisionOption, Vec<usize>, arbiter_core::PositionId)],
        ctx: &StageContext<'_>,
    ) -> AttachmentMatrix {
        let mut matrix = AttachmentMatrix::default();

        // Seed: a claim is Authored toward the option its own position's
        // recommendation clustered into.
        let position_to_option: BTreeMap<&str, &OptionId> = options
            .iter()
            .map(|(opt, _, position_id)| (position_id.as_str(), &opt.id))
            .collect();
        for claim in &claims.0 {
            for member in &claim.members {
                if let Some(&option_id) = position_to_option.get(member.position.as_str()) {
                    matrix.cells.insert(
                        (claim.id.clone(), option_id.clone()),
                        Attachment {
                            polarity: Polarity::Supports,
                            confidence: 1.0,
                            source: AttachSource::Authored,
                        },
                    );
                }
            }
        }

        if claims.0.is_empty() || options.is_empty() {
            return matrix;
        }

        let options_block = options
            .iter()
            .enumerate()
            .map(|(i, (opt, ..))| format!("#{} {}", i + 1, opt.label))
            .collect::<Vec<_>>()
            .join("\n");

        for chunk in claims.0.chunks(self.max_claims_per_batch) {
            let claims_block = chunk
                .iter()
                .enumerate()
                .map(|(i, c)| format!("#{} {}", i + 1, c.text))
                .collect::<Vec<_>>()
                .join("\n");

            let mut vars = BTreeMap::new();
            vars.insert("claims".to_string(), claims_block);
            vars.insert("options".to_string(), options_block.clone());

            let Some(response) = self
                .call(&self.attach_template, vars, ctx, "options.attach")
                .await
            else {
                continue;
            };
            let Ok(cells) = serde_json::from_str::<Vec<RawAttachCell>>(&response) else {
                continue;
            };

            for cell in cells {
                let (Some(local_claim), Some(local_option)) = (
                    parse_ref(&cell.claim, chunk.len()),
                    parse_ref(&cell.option, options.len()),
                ) else {
                    continue;
                };
                let claim_id: ClaimId = chunk[local_claim].id.clone();
                let option_id: OptionId = options[local_option].0.id.clone();
                let key = (claim_id, option_id);

                match cell.polarity.as_str() {
                    "supports" => {
                        matrix.cells.insert(
                            key,
                            Attachment {
                                polarity: Polarity::Supports,
                                confidence: cell.confidence,
                                source: AttachSource::Classified,
                            },
                        );
                    }
                    "opposes" => {
                        matrix.cells.insert(
                            key,
                            Attachment {
                                polarity: Polarity::Opposes,
                                confidence: cell.confidence,
                                source: AttachSource::Classified,
                            },
                        );
                    }
                    "neutral" => {
                        matrix.cells.remove(&key);
                    }
                    _ => {}
                }
            }
        }

        matrix
    }
}

fn parse_ref(r: &str, n: usize) -> Option<usize> {
    let idx: usize = r.strip_prefix('#')?.parse().ok()?;
    if idx == 0 || idx > n {
        return None;
    }
    Some(idx - 1)
}

impl Stage for OptionsCluster {
    type In = ClusterInput;
    type Out = ClusteredOptions;

    fn name(&self) -> StageName {
        StageName::new("options.cluster")
    }

    fn parallelism(&self) -> Parallelism {
        Parallelism::Serial
    }

    fn failure_policy(&self) -> FailurePolicy {
        FailurePolicy::DegradeWithEvent
    }

    fn idempotency_key(&self, input: &Self::In, ctx: &RunContext) -> Key {
        idempotency_key(&self.name(), ctx, &[input.content_hash()])
    }

    fn cost_estimate(&self, input: &Self::In) -> CostEstimate {
        let attach_batches = input
            .claims
            .0
            .len()
            .div_ceil(self.max_claims_per_batch)
            .max(1) as u32;
        let calls = 1 + attach_batches; // one cluster call + attach batches
        CostEstimate {
            calls,
            tokens: 0,
            cost: Cost(self.estimated_cost_per_call.0 * calls as f64),
        }
    }

    async fn run(&self, input: Self::In, ctx: &StageContext<'_>) -> Result<Self::Out, StageError> {
        ctx.events.emit(
            EventType::StageStarted,
            &self.name(),
            serde_json::json!({"positions": input.positions.0.len(), "claims": input.claims.0.len()}),
        );

        let clustered = self.cluster_positions(&input.positions, ctx).await;
        let options: Vec<(DecisionOption, Vec<usize>, arbiter_core::PositionId)> = clustered
            .into_iter()
            .map(|(opt, member_indices)| {
                let position_id = input.positions.0[member_indices[0]].id.clone();
                (opt, member_indices, position_id)
            })
            .collect();

        // Every position in a group maps to that option -- not just the
        // survivor -- so a claim from *any* grouped position seeds Authored
        // toward the shared option.
        let mut expanded: Vec<(DecisionOption, Vec<usize>, arbiter_core::PositionId)> = Vec::new();
        for (opt, member_indices, _) in &options {
            for &member_idx in member_indices {
                expanded.push((
                    opt.clone(),
                    vec![member_idx],
                    input.positions.0[member_idx].id.clone(),
                ));
            }
        }

        let matrix = self.attach(&input.claims, &expanded, ctx).await;
        let decision_options: Vec<DecisionOption> =
            options.into_iter().map(|(o, _, _)| o).collect();

        ctx.events.emit(
            EventType::StageCompleted,
            &self.name(),
            serde_json::json!({"options": decision_options.len(), "cells": matrix.cells.len()}),
        );

        Ok(ClusteredOptions {
            options: decision_options,
            direct_matrix: matrix,
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
    use arbiter_core::{ClaimLifecycle, EvidenceKind, Grounding, PositionId, TextSpan};
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;

    // ---- scripted test scaffolding, matching the sibling stage modules ----

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
        fn script_text(&self, text: impl Into<String>) {
            self.script.lock().unwrap().push_back(Ok(ProviderResponse {
                text: text.into(),
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

    fn cluster_template() -> PromptTemplate {
        PromptTemplate {
            stage: StageName::new("options.cluster"),
            body: "{{positions}}".to_string(),
            variables: crate::prompt::VariableSchema::new(["positions"]),
        }
    }

    fn attach_template() -> PromptTemplate {
        PromptTemplate {
            stage: StageName::new("options.attach"),
            body: "{{claims}} {{options}}".to_string(),
            variables: crate::prompt::VariableSchema::new(["claims", "options"]),
        }
    }

    fn position(id: &str, model: &str, text: &str) -> super::super::positions_generate::Position {
        super::super::positions_generate::Position {
            id: PositionId::new(id),
            model: ModelId::new(model),
            provider: ProviderId::new("mock"),
            text: text.to_string(),
        }
    }

    fn claim_from(
        id: &str,
        text: &str,
        position_id: &str,
        model: &str,
    ) -> arbiter_core::CanonicalClaim {
        let member = arbiter_core::ClaimMember::new(
            ClaimId::new(id),
            ModelId::new(model),
            ProviderId::new("mock"),
            PositionId::new(position_id),
            text,
            Grounding::DirectQuote {
                span: TextSpan {
                    start: 0,
                    end: text.len(),
                    quote: text.to_string(),
                },
            },
        );
        arbiter_core::CanonicalClaim {
            id: ClaimId::new(id),
            text: text.to_string(),
            kind: EvidenceKind::Fact,
            lifecycle: ClaimLifecycle::Proposed,
            members: vec![member],
        }
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

    fn stage() -> OptionsCluster {
        OptionsCluster::new(
            cluster_template(),
            attach_template(),
            (ModelId::new("model-a"), ProviderId::new("mock")),
            Cost(0.01),
        )
    }

    #[tokio::test]
    async fn two_positions_cluster_into_one_option() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([
            {"members": ["#1", "#2"], "label": "Adopt a modular monolith", "confidence": 0.9}
        ]));
        // No claims below, so attach() short-circuits before any second call.
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let input = ClusterInput {
            positions: Positions(vec![
                position("pos_a", "model-a", "We should adopt a modular monolith."),
                position("pos_b", "model-b", "A modular monolith is the right call."),
            ]),
            claims: NormalizedClaims(vec![]),
        };
        let out = stage().run(input, &ctx).await.unwrap();

        assert_eq!(out.options.len(), 1);
        assert_eq!(out.options[0].label, "Adopt a modular monolith");
    }

    #[tokio::test]
    async fn an_unparseable_cluster_response_gives_every_position_its_own_option() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_text("not json");
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let input = ClusterInput {
            positions: Positions(vec![
                position("pos_a", "model-a", "Adopt microservices."),
                position("pos_b", "model-b", "Keep the monolith."),
            ]),
            claims: NormalizedClaims(vec![]),
        };
        let out = stage().run(input, &ctx).await.unwrap();

        assert_eq!(
            out.options.len(),
            2,
            "no option must be invented or merged when clustering fails -- each position stands alone"
        );
    }

    #[tokio::test]
    async fn a_position_the_response_never_mentions_still_gets_its_own_option() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        // Only mentions #1; #2 is never referenced. No claims below, so
        // attach() short-circuits before any second call.
        mock.script_json(serde_json::json!([
            {"members": ["#1"], "label": "Adopt microservices", "confidence": 0.9}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let input = ClusterInput {
            positions: Positions(vec![
                position("pos_a", "model-a", "Adopt microservices."),
                position("pos_b", "model-b", "Keep the monolith."),
            ]),
            claims: NormalizedClaims(vec![]),
        };
        let out = stage().run(input, &ctx).await.unwrap();

        assert_eq!(
            out.options.len(),
            2,
            "an unmentioned position must not be dropped"
        );
    }

    #[tokio::test]
    async fn a_claim_is_authored_toward_its_own_positions_option() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([
            {"members": ["#1"], "label": "Adopt microservices", "confidence": 0.9}
        ]));
        mock.script_json(serde_json::json!([])); // classifier adds nothing
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let input = ClusterInput {
            positions: Positions(vec![position("pos_a", "model-a", "Adopt microservices.")]),
            claims: NormalizedClaims(vec![claim_from(
                "claim_1",
                "our team has 8 developers",
                "pos_a",
                "model-a",
            )]),
        };
        let out = stage().run(input, &ctx).await.unwrap();

        let option_id = out.options[0].id.clone();
        let cell = out
            .direct_matrix
            .get(&ClaimId::new("claim_1"), &option_id)
            .unwrap();
        assert_eq!(cell.polarity, Polarity::Supports);
        assert_eq!(cell.source, AttachSource::Authored);
    }

    #[tokio::test]
    async fn the_classifier_can_override_the_authored_seed() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([
            {"members": ["#1"], "label": "Adopt microservices", "confidence": 0.9}
        ]));
        // The classifier judges claim #1 actually opposes option #1, despite
        // being authored from that same position.
        mock.script_json(serde_json::json!([
            {"claim": "#1", "option": "#1", "polarity": "opposes", "confidence": 0.7}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let input = ClusterInput {
            positions: Positions(vec![position("pos_a", "model-a", "Adopt microservices.")]),
            claims: NormalizedClaims(vec![claim_from(
                "claim_1",
                "this claim actually undercuts the recommendation",
                "pos_a",
                "model-a",
            )]),
        };
        let out = stage().run(input, &ctx).await.unwrap();

        let option_id = out.options[0].id.clone();
        let cell = out
            .direct_matrix
            .get(&ClaimId::new("claim_1"), &option_id)
            .unwrap();
        assert_eq!(cell.polarity, Polarity::Opposes);
        assert_eq!(cell.source, AttachSource::Classified);
        assert!((cell.confidence - 0.7).abs() < 1e-9);
    }

    #[tokio::test]
    async fn a_neutral_classifier_answer_removes_the_authored_seed() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([
            {"members": ["#1"], "label": "Adopt microservices", "confidence": 0.9}
        ]));
        mock.script_json(serde_json::json!([
            {"claim": "#1", "option": "#1", "polarity": "neutral", "confidence": 0.9}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let input = ClusterInput {
            positions: Positions(vec![position("pos_a", "model-a", "Adopt microservices.")]),
            claims: NormalizedClaims(vec![claim_from(
                "claim_1",
                "an aside with no real bearing",
                "pos_a",
                "model-a",
            )]),
        };
        let out = stage().run(input, &ctx).await.unwrap();

        let option_id = out.options[0].id.clone();
        assert!(
            out.direct_matrix
                .get(&ClaimId::new("claim_1"), &option_id)
                .is_none(),
            "a neutral classifier answer must remove the authored seed"
        );
    }

    #[test]
    fn the_shipped_cluster_and_attach_prompts_load_and_render() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../prompts/default/v1");
        let pack = crate::prompt::PromptPack::load(&dir).unwrap();

        let cluster = pack.template(&StageName::new("options.cluster")).unwrap();
        let mut vars = BTreeMap::new();
        vars.insert("positions".to_string(), "#1 some position".to_string());
        assert!(cluster.render(&vars).unwrap().contains("#1 some position"));

        let attach = pack.template(&StageName::new("options.attach")).unwrap();
        let mut vars = BTreeMap::new();
        vars.insert("claims".to_string(), "#1 some claim".to_string());
        vars.insert("options".to_string(), "#1 some option".to_string());
        let rendered = attach.render(&vars).unwrap();
        assert!(rendered.contains("#1 some claim"));
        assert!(rendered.contains("#1 some option"));
    }
}
