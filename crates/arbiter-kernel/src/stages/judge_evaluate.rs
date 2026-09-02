//! `judge.evaluate` (ARCHITECTURE §5.6 / INTERFACES §4): anonymise every
//! position, shuffle the pseudonym assignment, hand each judge the full
//! dossier (recommendation, claims, and every challenge/rebuttal exchange),
//! and turn the response back into real-identity `Scorecard`s.
//!
//! ```text
//! positions -> anonymise (A..) -> shuffle -> one call per judge -> Scorecard per position
//!           -> aggregate across judges (mean) for evidence(); per-judge scores kept for dispersion
//! ```

use super::disputes_rank::RankedDisputes;
use super::positions_generate::Positions;
use super::rebuttal_run::RebuttalOutcome;
use crate::event::EventType;
use crate::ids::{CallId, ReservationId, StageName};
use crate::prompt::PromptTemplate;
use crate::provider::ProviderRequest;
use crate::stage::{
    CostEstimate, DeterministicRng, FailurePolicy, Key, Parallelism, RunContext, Stage,
    StageContext, StageError, idempotency_key,
};
use crate::store::{Artifact, CacheKey, CachedResponse, Cost};
use arbiter_core::{ClaimId, ModelId, ProviderId, Scorecard};
use serde::Deserialize;
use std::collections::BTreeMap;

/// `Stage::In`: same reasoning as every earlier combining wrapper (D34–D39)
/// — this stage needs the panel's positions, the final resolved graph
/// (claims + everything `controller.decide` carried forward), and every
/// challenge/rebuttal exchange that happened across however many rounds
/// ran. Assembling that last list across rounds is the eventual executor's
/// job (D39's own scope note); this stage takes it as given.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeInput {
    pub positions: Positions,
    pub resolved: RankedDisputes,
    pub exchanges: Vec<RebuttalOutcome>,
}

impl Artifact for JudgeInput {
    fn artifact_type(&self) -> &'static str {
        "judge_input.v1"
    }
    fn content_hash(&self) -> String {
        let exchange_rows: Vec<serde_json::Value> = self
            .exchanges
            .iter()
            .map(|e| {
                serde_json::json!({
                    "claim_id": e.claim_id.as_str(),
                    "rebuttal_text": e.rebuttal_text,
                    "outcome": format!("{:?}", e.outcome),
                })
            })
            .collect();
        let combined = format!(
            "{}\u{1}{}\u{1}{}\u{1}{}",
            self.artifact_type(),
            self.positions.content_hash(),
            self.resolved.content_hash(),
            serde_json::to_string(&exchange_rows).expect("exchanges serialize"),
        );
        format!("blake3:{}", blake3::hash(combined.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "positions": self.positions.to_json(),
            "resolved": self.resolved.to_json(),
        })
    }
}

/// `Stage::Out`. `scores_by_model` is the mean scorecard per model across
/// every judge — the shape `decision::evidence::evidence_map` already
/// consumes (`BTreeMap<ModelId, Scorecard>`). `per_judge_scores` keeps each
/// judge's own scorecard per model, undestroyed — `decision::confidence`'s
/// `judge_dispersion` needs the per-judge spread for whichever position(s)
/// the eventual decision cares about, not a pre-averaged number.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeEvaluation {
    pub resolved: RankedDisputes,
    pub scores_by_model: BTreeMap<ModelId, Scorecard>,
    pub per_judge_scores: BTreeMap<ModelId, Vec<Scorecard>>,
}

fn scorecard_json(s: &Scorecard) -> serde_json::Value {
    serde_json::json!({
        "model": s.model.as_str(),
        "factual_correctness": s.factual_correctness,
        "logical_reasoning": s.logical_reasoning,
        "evidence_quality": s.evidence_quality,
        "problem_relevance": s.problem_relevance,
        "assumption_quality": s.assumption_quality,
        "counterargument_handling": s.counterargument_handling,
        "risk_awareness": s.risk_awareness,
        "practicality": s.practicality,
        "clarity": s.clarity,
    })
}

impl Artifact for JudgeEvaluation {
    fn artifact_type(&self) -> &'static str {
        "judge_evaluation.v1"
    }
    fn content_hash(&self) -> String {
        let mean_rows: Vec<serde_json::Value> =
            self.scores_by_model.values().map(scorecard_json).collect();
        let combined = format!(
            "{}\u{1}{}\u{1}{}",
            self.artifact_type(),
            self.resolved.content_hash(),
            serde_json::to_string(&mean_rows).expect("scores serialize"),
        );
        format!("blake3:{}", blake3::hash(combined.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "resolved": self.resolved.to_json(),
            "scores_by_model": self.scores_by_model.values().map(scorecard_json).collect::<Vec<_>>(),
        })
    }
}

/// ARCHITECTURE §5.6: "surface form is normalised before judging (tables
/// flattened, headings stripped, bullets unified)." No algorithm is given —
/// a reasonable, conservative subset, not a full markdown parser
/// (PLAN_DEVIATIONS.md D40): heading markers and bullet glyphs are stripped
/// line-by-line; a pipe-table row has its `|` delimiters turned into plain
/// comma separation, and a table's own separator row (`|---|---|`) is
/// dropped entirely rather than rendered as junk text. Length is never
/// truncated, matching the spec's own explicit "length not truncated."
fn normalize_surface_form(text: &str) -> String {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return Some(String::new());
            }
            // A markdown table separator row, e.g. `|---|:--:|---|` -- pure
            // punctuation, carries no content, dropped rather than flattened.
            if trimmed.starts_with('|')
                && trimmed.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
            {
                return None;
            }
            let stripped = trimmed.trim_start_matches('#').trim_start();
            let stripped = stripped
                .strip_prefix("- ")
                .or_else(|| stripped.strip_prefix("* "))
                .or_else(|| stripped.strip_prefix("+ "))
                .unwrap_or(stripped);
            let flattened = if stripped.starts_with('|') && stripped.ends_with('|') {
                stripped
                    .trim_matches('|')
                    .split('|')
                    .map(str::trim)
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                stripped.to_string()
            };
            Some(flattened)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One position's data, ready to render into the anonymised dossier block.
struct Dossier {
    pseudonym: String,
    model: ModelId,
    provider: ProviderId,
    text: String,
    claims: Vec<(ClaimId, String, String)>, // (id, kind, text)
    exchanges: Vec<(String, String, String)>, // (challenge, rebuttal, lifecycle_outcome)
}

fn pseudonym_for(index: usize) -> String {
    // A..Z, then AA/AB/... -- practically never reached (panels stay well
    // under 26), but total rather than panicking past it.
    if index < 26 {
        ((b'A' + index as u8) as char).to_string()
    } else {
        format!(
            "{}{}",
            ((b'A' + (index / 26 - 1) as u8) as char),
            ((b'A' + (index % 26) as u8) as char)
        )
    }
}

fn build_dossiers(input: &JudgeInput, rng: &mut DeterministicRng) -> Vec<Dossier> {
    let claims = &input.resolved.claims.0;
    let mut order: Vec<usize> = (0..input.positions.0.len()).collect();
    // Fisher-Yates using the deterministic, seeded RNG -- "random shuffle"
    // (§5.6), reproducible from the manifest seed like every other
    // randomised choice this kernel makes.
    for i in (1..order.len()).rev() {
        let j = (rng.next_u64() as usize) % (i + 1);
        order.swap(i, j);
    }

    order
        .into_iter()
        .enumerate()
        .map(|(pseudo_idx, pos_idx)| {
            let position = &input.positions.0[pos_idx];
            let position_claims: Vec<_> = claims
                .iter()
                .filter(|c| c.members.iter().any(|m| m.position == position.id))
                .collect();
            let claim_rows: Vec<(ClaimId, String, String)> = position_claims
                .iter()
                .map(|c| (c.id.clone(), format!("{:?}", c.kind), c.text.clone()))
                .collect();
            let claim_ids: std::collections::BTreeSet<&ClaimId> =
                position_claims.iter().map(|c| &c.id).collect();
            let exchange_rows: Vec<(String, String, String)> = input
                .exchanges
                .iter()
                .filter(|e| claim_ids.contains(&e.claim_id))
                .map(|e| {
                    let lifecycle = claims
                        .iter()
                        .find(|c| c.id == e.claim_id)
                        .map(|c| format!("{:?}", c.lifecycle))
                        .unwrap_or_else(|| "unknown".to_string());
                    (e.challenge_text.clone(), e.rebuttal_text.clone(), lifecycle)
                })
                .collect();

            Dossier {
                pseudonym: pseudonym_for(pseudo_idx),
                model: position.model.clone(),
                provider: position.provider.clone(),
                text: normalize_surface_form(&position.text),
                claims: claim_rows,
                exchanges: exchange_rows,
            }
        })
        .collect()
}

fn render_dossiers(dossiers: &[Dossier]) -> String {
    dossiers
        .iter()
        .map(|d| {
            let claims_block = if d.claims.is_empty() {
                "  (no claims)".to_string()
            } else {
                d.claims
                    .iter()
                    .map(|(id, kind, text)| format!("  {} [{kind}]: {text}", id.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let exchanges_block = if d.exchanges.is_empty() {
                "  (no challenges received)".to_string()
            } else {
                d.exchanges
                    .iter()
                    .enumerate()
                    .map(|(i, (challenge, rebuttal, outcome))| {
                        format!(
                            "  Exchange {}:\n    Challenge: {challenge}\n    Rebuttal: {rebuttal}\n    Outcome: {outcome}",
                            i + 1
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            format!(
                "POSITION {}\nRecommendation:\n{}\n\nClaims:\n{}\n\nExchanges:\n{}",
                d.pseudonym, d.text, claims_block, exchanges_block
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n")
}

#[derive(Debug, Deserialize)]
struct RawScorecard {
    pseudonym: String,
    factual_correctness: f64,
    logical_reasoning: f64,
    evidence_quality: f64,
    problem_relevance: f64,
    assumption_quality: f64,
    counterargument_handling: f64,
    risk_awareness: f64,
    practicality: f64,
    clarity: f64,
}

impl RawScorecard {
    fn into_scorecard(self, model: ModelId) -> Scorecard {
        let clamp = |v: f64| v.clamp(0.0, 1.0);
        Scorecard {
            model,
            factual_correctness: clamp(self.factual_correctness),
            logical_reasoning: clamp(self.logical_reasoning),
            evidence_quality: clamp(self.evidence_quality),
            problem_relevance: clamp(self.problem_relevance),
            assumption_quality: clamp(self.assumption_quality),
            counterargument_handling: clamp(self.counterargument_handling),
            risk_awareness: clamp(self.risk_awareness),
            practicality: clamp(self.practicality),
            clarity: clamp(self.clarity),
        }
    }
}

fn mean_scorecard(model: ModelId, cards: &[Scorecard]) -> Scorecard {
    let n = cards.len() as f64;
    let sum = |f: fn(&Scorecard) -> f64| cards.iter().map(f).sum::<f64>() / n;
    Scorecard {
        model,
        factual_correctness: sum(|s| s.factual_correctness),
        logical_reasoning: sum(|s| s.logical_reasoning),
        evidence_quality: sum(|s| s.evidence_quality),
        problem_relevance: sum(|s| s.problem_relevance),
        assumption_quality: sum(|s| s.assumption_quality),
        counterargument_handling: sum(|s| s.counterargument_handling),
        risk_awareness: sum(|s| s.risk_awareness),
        practicality: sum(|s| s.practicality),
        clarity: sum(|s| s.clarity),
    }
}

#[derive(Debug)]
pub struct JudgeEvaluate {
    template: PromptTemplate,
    judges: Vec<(ModelId, ProviderId)>,
    estimated_cost_per_call: Cost,
}

impl JudgeEvaluate {
    pub fn new(
        template: PromptTemplate,
        judges: Vec<(ModelId, ProviderId)>,
        estimated_cost_per_call: Cost,
    ) -> Self {
        Self {
            template,
            judges,
            estimated_cost_per_call,
        }
    }

    async fn call_judge(
        &self,
        judge_model: &ModelId,
        judge_provider: &ProviderId,
        dossiers_block: &str,
        ctx: &StageContext<'_>,
    ) -> Option<String> {
        let mut vars = BTreeMap::new();
        vars.insert("dossiers".to_string(), dossiers_block.to_string());
        let rendered = self.template.render(&vars).ok()?;
        let prompt_hash = self.template.prompt_hash(&rendered).to_string();

        let cache_key = CacheKey {
            provider: judge_provider.clone(),
            model: judge_model.clone(),
            params: "{}".to_string(),
            prompt_hash: prompt_hash.clone(),
        };
        if let Some(cached) = ctx.cache.get(&cache_key)
            && let Some(text) = cached.inline
        {
            return Some(text);
        }

        let reservation_id =
            ReservationId::new(format!("res_{}_{}", self.name(), judge_model.as_str()));
        let guard = ctx
            .budget
            .reserve(reservation_id.clone(), self.estimated_cost_per_call)
            .ok()?;
        ctx.events.emit(
            EventType::BudgetReserved,
            &self.name(),
            serde_json::json!({"reservation_id": reservation_id.as_str(), "estimate": self.estimated_cost_per_call.0}),
        );

        let provider = ctx.providers.get(judge_provider)?;
        let call_id = CallId::new(format!("call_{}_{}", self.name(), judge_model.as_str()));
        let request = ProviderRequest {
            model: judge_model.clone(),
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
        guard.commit(actual_cost);
        let response_hash = format!("blake3:{}", blake3::hash(response.text.as_bytes()).to_hex());
        ctx.events.emit(
            EventType::CallCompleted,
            &self.name(),
            serde_json::json!({"call_id": call_id.as_str(), "response_hash": response_hash, "actual_cost": actual_cost.0}),
        );

        ctx.cache.put(
            cache_key,
            CachedResponse {
                response_hash,
                size_bytes: response.text.len() as u64,
                inline: Some(response.text.clone()),
            },
        );

        Some(response.text)
    }
}

impl Stage for JudgeEvaluate {
    type In = JudgeInput;
    type Out = JudgeEvaluation;

    fn name(&self) -> StageName {
        StageName::new("judge.evaluate")
    }

    fn parallelism(&self) -> Parallelism {
        Parallelism::PerItem {
            max: self.judges.len().max(1),
        }
    }

    fn failure_policy(&self) -> FailurePolicy {
        FailurePolicy::DegradeWithEvent
    }

    fn idempotency_key(&self, input: &Self::In, ctx: &RunContext) -> Key {
        idempotency_key(&self.name(), ctx, &[input.content_hash()])
    }

    fn cost_estimate(&self, _input: &Self::In) -> CostEstimate {
        let n = self.judges.len() as u32;
        CostEstimate {
            calls: n,
            tokens: 0,
            cost: Cost(self.estimated_cost_per_call.0 * n as f64),
        }
    }

    async fn run(&self, input: Self::In, ctx: &StageContext<'_>) -> Result<Self::Out, StageError> {
        let stage_name = self.name();
        ctx.events.emit(
            EventType::StageStarted,
            &stage_name,
            serde_json::json!({"positions": input.positions.0.len(), "judges": self.judges.len()}),
        );

        let mut rng = ctx.rng;
        let dossiers = build_dossiers(&input, &mut rng);
        let dossiers_block = render_dossiers(&dossiers);

        // (pseudonym -> (model, provider)) -- restores real identity after
        // every judge's response is parsed.
        let by_pseudonym: BTreeMap<&str, (&ModelId, &ProviderId)> = dossiers
            .iter()
            .map(|d| (d.pseudonym.as_str(), (&d.model, &d.provider)))
            .collect();

        let mut per_judge_scores: BTreeMap<ModelId, Vec<Scorecard>> = BTreeMap::new();

        for (judge_model, judge_provider) in &self.judges {
            let Some(response) = self
                .call_judge(judge_model, judge_provider, &dossiers_block, ctx)
                .await
            else {
                continue;
            };
            let Ok(raw) = serde_json::from_str::<Vec<RawScorecard>>(&response) else {
                continue;
            };
            for r in raw {
                let Some(&(model, _provider)) = by_pseudonym.get(r.pseudonym.as_str()) else {
                    continue;
                };
                let scorecard = r.into_scorecard(model.clone());
                ctx.events.emit(
                    EventType::JudgeScored,
                    &stage_name,
                    serde_json::json!({
                        "judge": judge_model.as_str(),
                        "model": model.as_str(),
                        "weighted": scorecard.weighted(),
                    }),
                );
                per_judge_scores
                    .entry(model.clone())
                    .or_default()
                    .push(scorecard);
            }
        }

        let scores_by_model: BTreeMap<ModelId, Scorecard> = per_judge_scores
            .iter()
            .map(|(model, cards)| (model.clone(), mean_scorecard(model.clone(), cards)))
            .collect();

        ctx.events.emit(
            EventType::StageCompleted,
            &stage_name,
            serde_json::json!({"scored": scores_by_model.len()}),
        );

        Ok(JudgeEvaluation {
            resolved: input.resolved,
            scores_by_model,
            per_judge_scores,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetLedger;
    use crate::cache::ResponseCache;
    use crate::provider::{Provider, ProviderCapabilities, ProviderError, ProviderResponse};
    use crate::stage::{CancellationToken, EventSink, ProviderRegistry};
    use crate::stages::claims_normalize::NormalizedClaims;
    use crate::stages::options_cluster::ClusteredOptions;
    use crate::stages::positions_generate::Position;
    use crate::stages::rebuttal_run::RebuttalKind;
    use crate::stages::relations_analyze::AnalyzedRelations;
    use arbiter_core::{
        AttachmentMatrix, CanonicalClaim, ClaimLifecycle, ClaimMember, EvidenceKind, Grounding,
        PositionId, TextSpan,
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
            stage: StageName::new("judge.evaluate"),
            body: "{{dossiers}}".to_string(),
            variables: crate::prompt::VariableSchema::new(["dossiers"]),
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

    fn claim(id: &str, text: &str, model: &str, position: &str) -> CanonicalClaim {
        let member = ClaimMember::new(
            ClaimId::new(id),
            ModelId::new(model),
            ProviderId::new(format!("{model}-provider")),
            PositionId::new(position),
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

    fn resolved(claims: Vec<CanonicalClaim>) -> RankedDisputes {
        RankedDisputes {
            claims: NormalizedClaims(claims),
            relations: AnalyzedRelations(vec![]),
            options: ClusteredOptions {
                options: vec![],
                direct_matrix: AttachmentMatrix::default(),
            },
            standing: BTreeMap::new(),
            propagated_matrix: AttachmentMatrix::default(),
            ranked: vec![],
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
    async fn a_single_judges_scorecard_maps_back_to_the_real_model() {
        let mock = ScriptedProvider::new(ProviderId::new("judge-provider"));
        // Whichever pseudonym model-a landed on, script all plausible
        // letters so the test doesn't depend on the shuffle's outcome.
        mock.script_json(serde_json::json!([
            {"pseudonym": "A", "factual_correctness": 0.9, "logical_reasoning": 0.8,
             "evidence_quality": 0.7, "problem_relevance": 0.9, "assumption_quality": 0.6,
             "counterargument_handling": 0.85, "risk_awareness": 0.5, "practicality": 0.6,
             "clarity": 0.95}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let stage_ctx = ctx(&registry, &budget, &cache, &sink);

        let positions = Positions(vec![Position {
            id: PositionId::new("pos_a"),
            model: ModelId::new("model-a"),
            provider: ProviderId::new("model-a-provider"),
            text: "# Recommendation\nDo the thing.".to_string(),
        }]);
        let claims = vec![claim("c1", "a supporting fact", "model-a", "pos_a")];
        let input = JudgeInput {
            positions,
            resolved: resolved(claims),
            exchanges: vec![],
        };

        let stage = JudgeEvaluate::new(
            template(),
            vec![(ModelId::new("judge-1"), ProviderId::new("judge-provider"))],
            Cost(0.10),
        );
        let out = stage.run(input, &stage_ctx).await.unwrap();

        assert_eq!(out.scores_by_model.len(), 1);
        let sc = out.scores_by_model.get(&ModelId::new("model-a")).unwrap();
        assert_eq!(sc.model, ModelId::new("model-a"));
        assert!((sc.factual_correctness - 0.9).abs() < 1e-9);
        assert_eq!(sink.count(EventType::JudgeScored), 1);
    }

    #[tokio::test]
    async fn two_judges_produce_a_mean_scorecard_and_keep_both_originals() {
        let mock1 = ScriptedProvider::new(ProviderId::new("judge-provider-1"));
        mock1.script_json(serde_json::json!([
            {"pseudonym": "A", "factual_correctness": 1.0, "logical_reasoning": 1.0,
             "evidence_quality": 1.0, "problem_relevance": 1.0, "assumption_quality": 1.0,
             "counterargument_handling": 1.0, "risk_awareness": 1.0, "practicality": 1.0,
             "clarity": 1.0}
        ]));
        let mock2 = ScriptedProvider::new(ProviderId::new("judge-provider-2"));
        mock2.script_json(serde_json::json!([
            {"pseudonym": "A", "factual_correctness": 0.0, "logical_reasoning": 0.0,
             "evidence_quality": 0.0, "problem_relevance": 0.0, "assumption_quality": 0.0,
             "counterargument_handling": 0.0, "risk_awareness": 0.0, "practicality": 0.0,
             "clarity": 0.0}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock1));
        registry.register(Box::new(mock2));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let stage_ctx = ctx(&registry, &budget, &cache, &sink);

        let positions = Positions(vec![Position {
            id: PositionId::new("pos_a"),
            model: ModelId::new("model-a"),
            provider: ProviderId::new("model-a-provider"),
            text: "Do the thing.".to_string(),
        }]);
        let input = JudgeInput {
            positions,
            resolved: resolved(vec![]),
            exchanges: vec![],
        };

        let stage = JudgeEvaluate::new(
            template(),
            vec![
                (ModelId::new("judge-1"), ProviderId::new("judge-provider-1")),
                (ModelId::new("judge-2"), ProviderId::new("judge-provider-2")),
            ],
            Cost(0.10),
        );
        let out = stage.run(input, &stage_ctx).await.unwrap();

        let mean = out.scores_by_model.get(&ModelId::new("model-a")).unwrap();
        assert!((mean.factual_correctness - 0.5).abs() < 1e-9);

        let per_judge = out.per_judge_scores.get(&ModelId::new("model-a")).unwrap();
        assert_eq!(
            per_judge.len(),
            2,
            "both judges' original scorecards must survive"
        );
    }

    #[tokio::test]
    async fn an_unparseable_judge_response_degrades_without_scoring_anyone() {
        let mock = ScriptedProvider::new(ProviderId::new("judge-provider"));
        mock.script_json(serde_json::json!("not a scorecard array"));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let stage_ctx = ctx(&registry, &budget, &cache, &sink);

        let positions = Positions(vec![Position {
            id: PositionId::new("pos_a"),
            model: ModelId::new("model-a"),
            provider: ProviderId::new("model-a-provider"),
            text: "Do the thing.".to_string(),
        }]);
        let input = JudgeInput {
            positions,
            resolved: resolved(vec![]),
            exchanges: vec![],
        };

        let stage = JudgeEvaluate::new(
            template(),
            vec![(ModelId::new("judge-1"), ProviderId::new("judge-provider"))],
            Cost(0.10),
        );
        let out = stage.run(input, &stage_ctx).await.unwrap();
        assert!(out.scores_by_model.is_empty());
    }

    #[test]
    fn surface_normalisation_strips_headings_bullets_and_flattens_tables() {
        let text = "# Heading\n- bullet one\n* bullet two\n| a | b |\n|---|---|\n| 1 | 2 |";
        let normalized = normalize_surface_form(text);
        assert!(!normalized.contains('#'));
        assert!(!normalized.contains("- bullet"));
        assert!(normalized.contains("a, b"));
        assert!(normalized.contains("1, 2"));
        assert!(!normalized.contains("---"));
    }

    #[test]
    fn exchanges_are_grouped_onto_the_position_that_authored_the_challenged_claim() {
        let mut rng = DeterministicRng::seeded(1);
        let positions = Positions(vec![Position {
            id: PositionId::new("pos_a"),
            model: ModelId::new("model-a"),
            provider: ProviderId::new("model-a-provider"),
            text: "text".to_string(),
        }]);
        let claims = vec![claim("c1", "claim text", "model-a", "pos_a")];
        let exchanges = vec![RebuttalOutcome {
            claim_id: ClaimId::new("c1"),
            challenger: ModelId::new("model-b"),
            defender: ModelId::new("model-a"),
            defender_provider: ProviderId::new("model-a-provider"),
            challenge_text: "why though".to_string(),
            rebuttal_text: "because X".to_string(),
            outcome: RebuttalKind::Defend,
            revised_text: None,
        }];
        let input = JudgeInput {
            positions,
            resolved: resolved(claims),
            exchanges,
        };
        let dossiers = build_dossiers(&input, &mut rng);
        assert_eq!(dossiers.len(), 1);
        assert_eq!(dossiers[0].exchanges.len(), 1);
        assert_eq!(dossiers[0].exchanges[0].0, "why though");
    }
}
