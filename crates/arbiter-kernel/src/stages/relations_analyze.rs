//! `relations.analyze` (ARCHITECTURE §5.4 / INTERFACES §3): candidate claim
//! pairs by cheap similarity, then an LLM classifies only the candidates —
//! never all `O(n²)` pairs.
//!
//! ```text
//! claims -> T1 (lexical, always) ∪ T2 (polarity, always) -> LLM classifies candidates -> RelationKind + confidence
//! ```
//!
//! This is the stage INTERFACES §3's own worked pipeline is written for
//! ("claims -> normalise -> trigram SimHash blocking -> ..."), unlike
//! `claims.normalize`'s reuse of half of the same machinery (D33) —
//! `relations.analyze` is the one place T2 (the polarity sweep) actually
//! applies, since it is also the first stage to run *after*
//! `options.cluster`, so a claim's attachment to an option finally exists to
//! sweep.

use super::claims_normalize::NormalizedClaims;
use super::options_cluster::ClusteredOptions;
use super::similarity::top_k_pairs;
use crate::event::EventType;
use crate::ids::{CallId, ReservationId, StageName};
use crate::prompt::PromptTemplate;
use crate::provider::ProviderRequest;
use crate::stage::{
    CostEstimate, FailurePolicy, Key, Parallelism, RunContext, Stage, StageContext, StageError,
    idempotency_key,
};
use crate::store::{Artifact, Cost};
use arbiter_core::decision::attachment::Polarity;
use arbiter_core::{CanonicalClaim, ClaimId, ModelId, ProviderId, Relation, RelationKind};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

/// Combined input, same reasoning as `options.cluster`'s `ClusterInput`
/// (PLAN_DEVIATIONS.md D34): this stage needs both the claims and the
/// options they're attached to (for T2's polarity sweep).
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzeInput {
    pub claims: NormalizedClaims,
    pub options: ClusteredOptions,
}

impl Artifact for AnalyzeInput {
    fn artifact_type(&self) -> &'static str {
        "relations_analyze_input.v1"
    }
    fn content_hash(&self) -> String {
        let combined = format!(
            "{}\u{1}{}",
            self.claims.content_hash(),
            self.options.content_hash()
        );
        format!("blake3:{}", blake3::hash(combined.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"claims": self.claims.to_json(), "options": self.options.to_json()})
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzedRelations(pub Vec<Relation>);

impl Artifact for AnalyzedRelations {
    fn artifact_type(&self) -> &'static str {
        "analyzed_relations.v1"
    }
    fn content_hash(&self) -> String {
        let mut rows: Vec<serde_json::Value> = self
            .0
            .iter()
            .map(|r| {
                serde_json::json!({
                    "from": r.from.as_str(),
                    "to": r.to.as_str(),
                    "kind": format!("{:?}", r.kind),
                    "confidence": r.confidence,
                })
            })
            .collect();
        rows.sort_by_key(|v| v.to_string());
        let text = serde_json::to_string(&rows).expect("relations serialize");
        format!("blake3:{}", blake3::hash(text.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.0
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "from": r.from.as_str(),
                        "to": r.to.as_str(),
                        "kind": format!("{:?}", r.kind),
                        "confidence": r.confidence,
                    })
                })
                .collect(),
        )
    }
}

#[derive(Debug, Deserialize)]
struct RawRelation {
    pair: String,
    kind: String,
    from: String,
    to: String,
    confidence: f64,
}

#[derive(Debug)]
pub struct RelationsAnalyze {
    classify_template: PromptTemplate,
    model: (ModelId, ProviderId),
    estimated_cost_per_call: Cost,
    max_pairs_per_batch: usize,
}

impl RelationsAnalyze {
    pub fn new(
        classify_template: PromptTemplate,
        model: (ModelId, ProviderId),
        estimated_cost_per_call: Cost,
    ) -> Self {
        Self {
            classify_template,
            model,
            estimated_cost_per_call,
            max_pairs_per_batch: 30,
        }
    }

    async fn call(
        &self,
        pairs_block: String,
        ctx: &StageContext<'_>,
        call_label: &str,
    ) -> Option<String> {
        let stage_name = self.name();
        let mut vars = BTreeMap::new();
        vars.insert("pairs".to_string(), pairs_block);
        let rendered = self.classify_template.render(&vars).ok()?;
        let prompt_hash = self.classify_template.prompt_hash(&rendered).to_string();
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

        let response = provider.call(request).await.ok()?;
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
}

/// T2: "every cross-model pair attached to opposing options" (ARCHITECTURE
/// §5.4). Read as: for one option, a claim the direct matrix marks
/// `Supports` and a claim it marks `Opposes` disagree about that option, and
/// are worth classifying to find out why — the two claims come from
/// different models (`Supports` and `Opposes` from the same model on the
/// same option is that model contradicting itself, which is not the
/// cross-model corroboration/disagreement signal T2 is described as
/// catching). Neither spec file gives this rule as a formula, only the one
/// sentence quoted above; this is the literal, non-inventive reading of it
/// (PLAN_DEVIATIONS.md D35).
fn polarity_pairs(
    claims: &[CanonicalClaim],
    options: &ClusteredOptions,
) -> BTreeSet<(usize, usize)> {
    let index_of: BTreeMap<&str, usize> = claims
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id.as_str(), i))
        .collect();

    let mut by_option_support: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    let mut by_option_oppose: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for ((claim_id, option_id), attachment) in &options.direct_matrix.cells {
        let Some(&idx) = index_of.get(claim_id.as_str()) else {
            continue;
        };
        match attachment.polarity {
            Polarity::Supports => by_option_support
                .entry(option_id.as_str())
                .or_default()
                .push(idx),
            Polarity::Opposes => by_option_oppose
                .entry(option_id.as_str())
                .or_default()
                .push(idx),
            Polarity::Neutral => {}
        }
    }

    let mut pairs = BTreeSet::new();
    for (option, supporters) in &by_option_support {
        let Some(opposers) = by_option_oppose.get(option) else {
            continue;
        };
        for &s in supporters {
            for &o in opposers {
                let cross_model = claims[s]
                    .members
                    .iter()
                    .any(|ms| claims[o].members.iter().any(|mo| ms.model != mo.model));
                if !cross_model {
                    continue;
                }
                let pair = if s < o { (s, o) } else { (o, s) };
                pairs.insert(pair);
            }
        }
    }
    pairs
}

fn parse_kind(s: &str) -> Option<RelationKind> {
    match s {
        "supports" => Some(RelationKind::Supports),
        "contradicts" => Some(RelationKind::Contradicts),
        "qualifies" => Some(RelationKind::Qualifies),
        "unrelated" => Some(RelationKind::Unrelated),
        "uncertain" => Some(RelationKind::Uncertain),
        _ => None,
    }
}

impl Stage for RelationsAnalyze {
    type In = AnalyzeInput;
    type Out = AnalyzedRelations;

    fn name(&self) -> StageName {
        StageName::new("relations.analyze")
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
        // A loose upper bound: worst case every claim pairs with every
        // other, chunked into batches -- cost_estimate is a pre-flight
        // figure, not a promise; the real spend is bounded by the actual
        // T1 ∪ T2 candidate count computed in `run`.
        let n = input.claims.0.len();
        let worst_case_pairs = n.saturating_mul(n.saturating_sub(1)) / 2;
        let batches = worst_case_pairs.div_ceil(self.max_pairs_per_batch).max(1) as u32;
        CostEstimate {
            calls: batches,
            tokens: 0,
            cost: Cost(self.estimated_cost_per_call.0 * batches as f64),
        }
    }

    async fn run(&self, input: Self::In, ctx: &StageContext<'_>) -> Result<Self::Out, StageError> {
        let claims = input.claims.0;
        ctx.events.emit(
            EventType::StageStarted,
            &self.name(),
            serde_json::json!({"claims": claims.len()}),
        );

        if claims.len() < 2 {
            ctx.events.emit(
                EventType::StageCompleted,
                &self.name(),
                serde_json::json!({"relations": 0}),
            );
            return Ok(AnalyzedRelations(Vec::new()));
        }

        let t1_pairs: BTreeSet<(usize, usize)> = top_k_pairs(&claims).into_iter().collect();
        let t2_pairs = polarity_pairs(&claims, &input.options);
        let candidates: Vec<(usize, usize)> = t1_pairs.union(&t2_pairs).copied().collect();

        ctx.events.emit(
            EventType::CandidatesSelected,
            &self.name(),
            serde_json::json!({
                "t1_pairs": t1_pairs.len(),
                "t2_pairs": t2_pairs.len(),
                "candidates": candidates.len(),
            }),
        );

        let mut relations = Vec::new();

        for (batch_no, chunk) in candidates.chunks(self.max_pairs_per_batch).enumerate() {
            let block = chunk
                .iter()
                .enumerate()
                .map(|(i, &(a, b))| {
                    format!(
                        "Pair #{}:\nA: {}\nB: {}",
                        i + 1,
                        claims[a].text,
                        claims[b].text
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n");

            let Some(response) = self
                .call(block, ctx, &format!("relations.classify.batch{batch_no}"))
                .await
            else {
                continue;
            };
            let Ok(raw) = serde_json::from_str::<Vec<RawRelation>>(&response) else {
                continue;
            };

            for r in raw {
                let Some(local_idx) = r
                    .pair
                    .strip_prefix('#')
                    .and_then(|n| n.parse::<usize>().ok())
                    .filter(|&n| n >= 1 && n <= chunk.len())
                    .map(|n| n - 1)
                else {
                    continue;
                };
                let Some(kind) = parse_kind(&r.kind) else {
                    continue;
                };
                let (a, b) = chunk[local_idx];
                let (from_idx, to_idx) = match r.from.as_str() {
                    "A" if r.to == "B" => (a, b),
                    "B" if r.to == "A" => (b, a),
                    _ => (a, b),
                };
                let from_id: ClaimId = claims[from_idx].id.clone();
                let to_id: ClaimId = claims[to_idx].id.clone();
                relations.push(Relation {
                    from: from_id,
                    to: to_id,
                    kind,
                    confidence: r.confidence.clamp(0.0, 1.0),
                });
            }
        }

        ctx.events.emit(
            EventType::StageCompleted,
            &self.name(),
            serde_json::json!({"relations": relations.len()}),
        );

        Ok(AnalyzedRelations(relations))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetLedger;
    use crate::cache::ResponseCache;
    use crate::provider::{Provider, ProviderCapabilities, ProviderError, ProviderResponse};
    use crate::stage::{CancellationToken, DeterministicRng, EventSink, ProviderRegistry};
    use arbiter_core::decision::attachment::{AttachSource, Attachment, AttachmentMatrix};
    use arbiter_core::{
        ClaimLifecycle, DecisionOption, EvidenceKind, Grounding, OptionId, PositionId, TextSpan,
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

    fn classify_template() -> PromptTemplate {
        PromptTemplate {
            stage: StageName::new("relations.classify"),
            body: "{{pairs}}".to_string(),
            variables: crate::prompt::VariableSchema::new(["pairs"]),
        }
    }

    fn claim(id: &str, text: &str, model: &str) -> CanonicalClaim {
        let member = arbiter_core::ClaimMember::new(
            ClaimId::new(id),
            ModelId::new(model),
            ProviderId::new("mock"),
            PositionId::new(format!("pos_{model}")),
            text,
            Grounding::DirectQuote {
                span: TextSpan {
                    start: 0,
                    end: text.len(),
                    quote: text.to_string(),
                },
            },
        );
        CanonicalClaim {
            id: ClaimId::new(id),
            text: text.to_string(),
            kind: EvidenceKind::Fact,
            lifecycle: ClaimLifecycle::Proposed,
            members: vec![member],
        }
    }

    fn empty_options() -> ClusteredOptions {
        ClusteredOptions {
            options: vec![],
            direct_matrix: AttachmentMatrix::default(),
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

    fn stage() -> RelationsAnalyze {
        RelationsAnalyze::new(
            classify_template(),
            (ModelId::new("model-a"), ProviderId::new("mock")),
            Cost(0.01),
        )
    }

    #[tokio::test]
    async fn a_lexically_similar_pair_is_classified() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([
            {"pair": "#1", "kind": "contradicts", "from": "A", "to": "B", "confidence": 0.9}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let input = AnalyzeInput {
            claims: NormalizedClaims(vec![
                claim(
                    "claim_a",
                    "microservices increase deployment complexity",
                    "model-a",
                ),
                claim(
                    "claim_b",
                    "microservices decrease deployment complexity",
                    "model-b",
                ),
            ]),
            options: empty_options(),
        };
        let out = stage().run(input, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 1);
        assert_eq!(out.0[0].kind, RelationKind::Contradicts);
        assert_eq!(out.0[0].from, ClaimId::new("claim_a"));
        assert_eq!(out.0[0].to, ClaimId::new("claim_b"));
    }

    #[tokio::test]
    async fn a_reversed_direction_is_recorded_correctly() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([
            {"pair": "#1", "kind": "qualifies", "from": "B", "to": "A", "confidence": 0.7}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let input = AnalyzeInput {
            claims: NormalizedClaims(vec![
                claim(
                    "claim_a",
                    "microservices increase deployment complexity",
                    "model-a",
                ),
                claim(
                    "claim_b",
                    "microservices decrease deployment complexity",
                    "model-b",
                ),
            ]),
            options: empty_options(),
        };
        let out = stage().run(input, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 1);
        assert_eq!(
            out.0[0].from,
            ClaimId::new("claim_b"),
            "B qualifies A means from=B"
        );
        assert_eq!(out.0[0].to, ClaimId::new("claim_a"));
    }

    #[tokio::test]
    async fn t2_finds_a_cross_model_pair_with_no_lexical_overlap() {
        // "alpha..." and "zzzz..." share no trigrams, so T1 would never pair
        // them -- only the polarity sweep (opposite polarity on the same
        // option, different models) should surface this pair.
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([
            {"pair": "#1", "kind": "contradicts", "from": "A", "to": "B", "confidence": 0.8}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let claims = vec![
            claim("claim_a", "alpha alpha alpha", "model-a"),
            claim("claim_b", "zzzz zzzz zzzz", "model-b"),
        ];
        let option = DecisionOption::new(OptionId::new("opt_1"), "Some option");
        let mut matrix = AttachmentMatrix::default();
        matrix.cells.insert(
            (ClaimId::new("claim_a"), option.id.clone()),
            Attachment {
                polarity: Polarity::Supports,
                confidence: 1.0,
                source: AttachSource::Authored,
            },
        );
        matrix.cells.insert(
            (ClaimId::new("claim_b"), option.id.clone()),
            Attachment {
                polarity: Polarity::Opposes,
                confidence: 1.0,
                source: AttachSource::Authored,
            },
        );

        let input = AnalyzeInput {
            claims: NormalizedClaims(claims),
            options: ClusteredOptions {
                options: vec![option],
                direct_matrix: matrix,
            },
        };
        let out = stage().run(input, &ctx).await.unwrap();

        assert_eq!(
            out.0.len(),
            1,
            "T2 must surface the pair even with zero lexical overlap"
        );
    }

    #[tokio::test]
    async fn same_model_opposite_polarity_is_not_a_t2_candidate() {
        // Both claims from model-a: opposite polarity on the same option is
        // not the cross-model disagreement signal T2 looks for.
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        // No response scripted: if a call happens, script-exhausted degrades
        // to zero relations, which the assertion below already expects --
        // so this test only proves the call never *needed* a script, not
        // that it never happened. Assert via emitted events instead.
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let claims = vec![
            claim("claim_a", "alpha alpha alpha", "model-a"),
            claim("claim_b", "zzzz zzzz zzzz", "model-a"),
        ];
        let option = DecisionOption::new(OptionId::new("opt_1"), "Some option");
        let mut matrix = AttachmentMatrix::default();
        matrix.cells.insert(
            (ClaimId::new("claim_a"), option.id.clone()),
            Attachment {
                polarity: Polarity::Supports,
                confidence: 1.0,
                source: AttachSource::Authored,
            },
        );
        matrix.cells.insert(
            (ClaimId::new("claim_b"), option.id.clone()),
            Attachment {
                polarity: Polarity::Opposes,
                confidence: 1.0,
                source: AttachSource::Authored,
            },
        );

        let input = AnalyzeInput {
            claims: NormalizedClaims(claims),
            options: ClusteredOptions {
                options: vec![option],
                direct_matrix: matrix,
            },
        };
        let out = stage().run(input, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 0);
        assert_eq!(
            sink.count(EventType::CallStarted),
            0,
            "no candidate pair means no classify call at all"
        );
    }

    #[tokio::test]
    async fn an_omitted_pair_produces_no_relation() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([])); // classifier has nothing to say
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let input = AnalyzeInput {
            claims: NormalizedClaims(vec![
                claim(
                    "claim_a",
                    "microservices increase deployment complexity",
                    "model-a",
                ),
                claim(
                    "claim_b",
                    "microservices decrease deployment complexity",
                    "model-b",
                ),
            ]),
            options: empty_options(),
        };
        let out = stage().run(input, &ctx).await.unwrap();
        assert_eq!(out.0.len(), 0);
    }

    #[tokio::test]
    async fn fewer_than_two_claims_never_calls_the_provider() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let input = AnalyzeInput {
            claims: NormalizedClaims(vec![claim("claim_a", "a solitary claim", "model-a")]),
            options: empty_options(),
        };
        let out = stage().run(input, &ctx).await.unwrap();
        assert_eq!(out.0.len(), 0);
        assert_eq!(sink.count(EventType::CallStarted), 0);
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

    #[test]
    fn the_shipped_classify_prompt_loads_and_renders() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../prompts/default/v1");
        let pack = crate::prompt::PromptPack::load(&dir).unwrap();
        let template = pack
            .template(&StageName::new("relations.classify"))
            .unwrap();

        let mut vars = BTreeMap::new();
        vars.insert("pairs".to_string(), "Pair #1:\nA: x\nB: y".to_string());
        assert!(template.render(&vars).unwrap().contains("Pair #1"));
    }
}
