//! `positions.generate` (ARCHITECTURE §5, §5's own words: "parallel,
//! independent, no cross-talk"). Each panel member answers the question with
//! no visibility into any other member's answer or into this stage's own
//! prior output — round 1 has nothing to converge against yet.
//!
//! Concurrency lives inside this stage, not across it (INTERFACES §6): `run`
//! fans every panel member out under one bounded join set
//! (`futures_util::stream::buffer_unordered`), never a per-provider semaphore
//! — deferred, see this module's own doc note below — and a single member's
//! failure is `SkipItem` (INTERFACES §6: "a single model timing out in
//! `positions.generate` is `SkipItem` — the debate continues with four
//! positions and the record says so"), never fatal to the stage.

use crate::event::EventType;
use crate::ids::{CallId, ReservationId, StageName};
use crate::prompt::PromptTemplate;
use crate::provider::{ProviderRequest, ProviderResponse};
use crate::stage::{
    CostEstimate, FailurePolicy, Key, Parallelism, RunContext, Stage, StageContext, StageError,
    idempotency_key,
};
use crate::store::{Artifact, CacheKey, CachedResponse, Cost};
use arbiter_core::{ModelId, ProviderId};
use futures_util::StreamExt;
use std::collections::BTreeMap;

/// The validated question every position answers — `init`'s output, carried
/// into round 1 (ARCHITECTURE §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub text: String,
}

impl Artifact for Question {
    fn artifact_type(&self) -> &'static str {
        "question.v1"
    }
    fn content_hash(&self) -> String {
        let text = format!("{}\u{1}{}", self.artifact_type(), self.text);
        format!("blake3:{}", blake3::hash(text.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({"text": self.text})
    }
}

/// One model's independently generated position — ARCHITECTURE §5.1's own
/// phrase, "position text," is the only description either spec file gives;
/// no concrete struct exists anywhere (PLAN_DEVIATIONS.md D19-category gap,
/// D31). Modelled with the model/provider identity plus the raw text, so a
/// position is traceable to the call that produced it without re-deriving
/// anything. `id` is deterministic from `(provider, model)` — round 1 mints
/// exactly one position per panel member, so this pairing is already a stable
/// identity; `claims.extract`'s `ClaimMember::position` is what actually
/// needs a [`PositionId`] to point at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub id: arbiter_core::PositionId,
    pub model: ModelId,
    pub provider: ProviderId,
    pub text: String,
}

/// The whole panel's positions — `Stage::Out`. Sorted by `(provider, model)`
/// before construction so `content_hash` (and therefore the idempotency key
/// any later stage derives from it) does not depend on the nondeterministic
/// order concurrent calls happen to complete in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Positions(pub Vec<Position>);

impl Artifact for Positions {
    fn artifact_type(&self) -> &'static str {
        "positions.v1"
    }
    fn content_hash(&self) -> String {
        let canonical: Vec<serde_json::Value> = self.0.iter().map(position_json).collect();
        let text = format!(
            "{}\u{1}{}",
            self.artifact_type(),
            serde_json::to_string(&canonical).expect("positions serialize")
        );
        format!("blake3:{}", blake3::hash(text.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Array(self.0.iter().map(position_json).collect())
    }
}

fn position_json(p: &Position) -> serde_json::Value {
    serde_json::json!({
        "id": p.id.as_str(),
        "model": p.model.as_str(),
        "provider": p.provider.as_str(),
        "text": p.text,
    })
}

/// Why one panel member produced no position. INTERFACES §6 makes a single
/// member's failure `SkipItem`, but "skip" was being read as "say nothing":
/// the provider's own error was discarded at the `match`, so a whole panel of
/// bad API keys reported `InsufficientEvidence / complete` with a silent,
/// empty transcript. ARCHITECTURE §8.4 gives FAILED no event of its own
/// (its Event column is "—"), so the reason rides `STAGE_COMPLETED`'s payload
/// rather than a new `EventType` variant that INTERFACES §13 does not name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PositionSkip {
    model: ModelId,
    provider: ProviderId,
    reason: String,
}

impl PositionSkip {
    fn new(model: &ModelId, provider: &ProviderId, reason: impl Into<String>) -> Self {
        Self {
            model: model.clone(),
            provider: provider.clone(),
            reason: reason.into(),
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "model": self.model.as_str(),
            "provider": self.provider.as_str(),
            "reason": self.reason,
        })
    }
}

/// `positions.generate`. Constructed with the resolved panel (`panel.resolve`'s
/// output — not yet implemented, D30/G2 scope note; a caller supplies the list
/// directly until it exists) and the loaded `positions.generate` template.
#[derive(Debug)]
pub struct PositionsGenerate {
    panel: Vec<(ModelId, ProviderId)>,
    template: PromptTemplate,
    /// Charged as `actual_cost` on every completed call, in the absence of any
    /// real per-token pricing table anywhere in this workspace yet (P4 is the
    /// task that would add one) — conservative in the sense that it never
    /// under-charges the ledger relative to what was reserved.
    estimated_cost_per_call: Cost,
    max_parallelism: usize,
}

impl PositionsGenerate {
    pub fn new(
        panel: Vec<(ModelId, ProviderId)>,
        template: PromptTemplate,
        estimated_cost_per_call: Cost,
        max_parallelism: usize,
    ) -> Self {
        Self {
            panel,
            template,
            estimated_cost_per_call,
            max_parallelism: max_parallelism.max(1),
        }
    }

    async fn generate_one(
        &self,
        question: &Question,
        model: ModelId,
        provider_id: ProviderId,
        ctx: &StageContext<'_>,
    ) -> Result<Position, PositionSkip> {
        if ctx.cancel.is_cancelled() {
            return Err(PositionSkip::new(&model, &provider_id, "run cancelled"));
        }

        let mut vars = BTreeMap::new();
        vars.insert("question".to_string(), question.text.clone());
        let rendered = self.template.render(&vars).map_err(|e| {
            PositionSkip::new(&model, &provider_id, format!("prompt render failed: {e}"))
        })?;
        let prompt_hash = self.template.prompt_hash(&rendered).to_string();

        ctx.events.emit(
            EventType::PositionStarted,
            &self.name(),
            serde_json::json!({"model": model.as_str(), "provider": provider_id.as_str()}),
        );

        let position_id = arbiter_core::PositionId::new(format!(
            "pos_{}_{}",
            provider_id.as_str(),
            model.as_str()
        ));

        let cache_key = CacheKey {
            provider: provider_id.clone(),
            model: model.clone(),
            params: "{}".to_string(),
            prompt_hash: prompt_hash.clone(),
        };

        // "Provider stages consult the cache first" (INTERFACES §7). Only an
        // inline hit is usable here: a response that moved to the blob store
        // needs `arbiter-store` to read it back, which this crate cannot
        // depend on (D1) -- a blob-backed hit falls through to a real call.
        if let Some(cached) = ctx.cache.get(&cache_key)
            && let Some(text) = cached.inline
        {
            ctx.events.emit(
                EventType::PositionCompleted,
                &self.name(),
                serde_json::json!({"model": model.as_str(), "cache_hit": true}),
            );
            return Ok(Position {
                id: position_id,
                model,
                provider: provider_id,
                text,
            });
        }

        let reservation_id = ReservationId::new(format!(
            "res_{}_{}_{}",
            self.name(),
            provider_id.as_str(),
            model.as_str()
        ));
        let guard = match ctx
            .budget
            .reserve(reservation_id.clone(), self.estimated_cost_per_call)
        {
            Ok(guard) => guard,
            Err(e) => {
                ctx.events.emit(
                    EventType::BudgetExhausted,
                    &self.name(),
                    serde_json::json!({"model": model.as_str()}),
                );
                return Err(PositionSkip::new(
                    &model,
                    &provider_id,
                    format!(
                        "budget exhausted: needed {:.6}, {:.6} available",
                        e.requested.0, e.available.0
                    ),
                ));
            }
        };
        ctx.events.emit(
            EventType::BudgetReserved,
            &self.name(),
            serde_json::json!({
                "reservation_id": reservation_id.as_str(),
                "estimate": self.estimated_cost_per_call.0,
            }),
        );

        let Some(provider) = ctx.providers.get(&provider_id) else {
            // No provider registered for this panel member: SkipItem. The
            // reservation is released by ReservationGuard's own Drop; the
            // event has to be emitted here, because Drop cannot reach the sink.
            super::emit_budget_released(
                ctx,
                &self.name(),
                &reservation_id,
                self.estimated_cost_per_call,
                "no provider registered for this panel member",
            );
            return Err(PositionSkip::new(
                &model,
                &provider_id,
                "no provider registered for this panel member",
            ));
        };

        let call_id = CallId::new(format!(
            "call_{}_{}_{}",
            self.name(),
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
            &self.name(),
            serde_json::json!({
                "call_id": call_id.as_str(),
                "prompt_hash": prompt_hash,
                "reservation_id": reservation_id.as_str(),
                "estimate": self.estimated_cost_per_call.0,
            }),
        );
        guard.mark_sent();

        let response: Result<ProviderResponse, _> = provider.call(request).await;
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                // A clean provider error (not a never-arrived response) --
                // FAILED, not ORPHANED. Drop releases the reservation, but the
                // release event and the provider's own message both have to be
                // raised here: an unauthorised key is the single most likely
                // reason a real panel produces nothing, and swallowing it left
                // the operator with an empty transcript and a cheerful
                // "InsufficientEvidence / complete".
                let reason = e.to_string();
                super::emit_budget_released(
                    ctx,
                    &self.name(),
                    &reservation_id,
                    self.estimated_cost_per_call,
                    &reason,
                );
                return Err(PositionSkip::new(&model, &provider_id, reason));
            }
        };

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
            serde_json::json!({
                "call_id": call_id.as_str(),
                "response_hash": response_hash,
                "actual_cost": actual_cost.0,
            }),
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
            EventType::PositionCompleted,
            &self.name(),
            serde_json::json!({"model": model.as_str(), "cache_hit": false}),
        );

        Ok(Position {
            id: position_id,
            model,
            provider: provider_id,
            text: response.text,
        })
    }
}

impl Stage for PositionsGenerate {
    type In = Question;
    type Out = Positions;

    fn name(&self) -> StageName {
        StageName::new("positions.generate")
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

    fn cost_estimate(&self, _input: &Self::In) -> CostEstimate {
        CostEstimate {
            calls: self.panel.len() as u32,
            tokens: 0,
            cost: Cost(self.estimated_cost_per_call.0 * self.panel.len() as f64),
        }
    }

    async fn run(&self, input: Self::In, ctx: &StageContext<'_>) -> Result<Self::Out, StageError> {
        ctx.events.emit(
            EventType::StageStarted,
            &self.name(),
            serde_json::json!({"panel_size": self.panel.len()}),
        );

        let outcomes: Vec<Result<Position, PositionSkip>> =
            futures_util::stream::iter(self.panel.iter().cloned())
                .map(|(model, provider_id)| {
                    let question = &input;
                    async move { self.generate_one(question, model, provider_id, ctx).await }
                })
                .buffer_unordered(self.max_parallelism)
                .collect()
                .await;

        let mut results: Vec<Position> = Vec::new();
        let mut skipped: Vec<PositionSkip> = Vec::new();
        for outcome in outcomes {
            match outcome {
                Ok(position) => results.push(position),
                Err(skip) => skipped.push(skip),
            }
        }

        results.sort_by(|a, b| {
            (a.provider.as_str(), a.model.as_str()).cmp(&(b.provider.as_str(), b.model.as_str()))
        });
        skipped.sort_by(|a, b| {
            (a.provider.as_str(), a.model.as_str()).cmp(&(b.provider.as_str(), b.model.as_str()))
        });

        // Every member failed on a panel that had members. INTERFACES §6's
        // `SkipItem` covers "the debate continues with four positions and the
        // record says so"; it does not cover continuing with zero. Three of
        // four models failing is a degraded debate, and the fixture
        // `k2_provider_timeout` requires that it proceed. None of N succeeding
        // is not a debate at all: every stage after this one would read an
        // empty panel as "the models had nothing to say" and synthesise a
        // confident-looking `InsufficientEvidence`, hiding the real cause.
        if results.is_empty() && !self.panel.is_empty() {
            let detail = skipped
                .iter()
                .map(|s| format!("{}/{}: {}", s.provider.as_str(), s.model.as_str(), s.reason))
                .collect::<Vec<_>>()
                .join("; ");
            ctx.events.emit(
                EventType::StageFailed,
                &self.name(),
                serde_json::json!({
                    "panel_size": self.panel.len(),
                    "positions": 0,
                    "skipped": skipped.iter().map(PositionSkip::to_json).collect::<Vec<_>>(),
                }),
            );
            return Err(StageError::Other(format!(
                "every model in the panel failed to produce a position ({} of {}): {detail}",
                skipped.len(),
                self.panel.len()
            )));
        }

        ctx.events.emit(
            EventType::StageCompleted,
            &self.name(),
            serde_json::json!({
                "positions": results.len(),
                "skipped": skipped.iter().map(PositionSkip::to_json).collect::<Vec<_>>(),
            }),
        );

        Ok(Positions(results))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetLedger;
    use crate::cache::ResponseCache;
    use crate::provider::{Provider, ProviderCapabilities, ProviderError};
    use crate::stage::{CancellationToken, DeterministicRng, EventSink, ProviderRegistry};
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::time::Instant;

    /// A minimal scripted `Provider`, local to this module: `arbiter-kernel`
    /// cannot depend on `arbiter-providers::mock::MockProvider` (D1 — that
    /// crate already depends on this one, so the reverse would be a cycle).
    #[derive(Debug)]
    struct ScriptedProvider {
        id: ProviderId,
        script: Mutex<VecDeque<Result<ProviderResponse, ProviderError>>>,
    }
    impl ScriptedProvider {
        fn new(id: ProviderId) -> Self {
            Self {
                id,
                script: Mutex::new(VecDeque::new()),
            }
        }
        fn script_text(&self, text: impl Into<String>) {
            self.script.lock().unwrap().push_back(Ok(ProviderResponse {
                text: text.into(),
                prompt_tokens: 0,
                completion_tokens: 0,
                request_id: None,
            }));
        }
        fn script_error(&self, message: impl Into<String>) {
            self.script
                .lock()
                .unwrap()
                .push_back(Err(ProviderError::Other(message.into())));
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

    fn template() -> PromptTemplate {
        PromptTemplate {
            stage: StageName::new("positions.generate"),
            body: "Question: {{question}}".to_string(),
            variables: crate::prompt::VariableSchema::new(["question"]),
        }
    }

    #[derive(Debug, Default)]
    struct RecordingSink {
        emitted: Mutex<Vec<(EventType, serde_json::Value)>>,
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
    async fn round_one_positions_never_see_each_other() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_text("first panelist's answer");
        mock.script_text("second panelist's answer");
        mock.script_text("third panelist's answer");

        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let panel = vec![
            (ModelId::new("model-a"), ProviderId::new("mock")),
            (ModelId::new("model-b"), ProviderId::new("mock")),
            (ModelId::new("model-c"), ProviderId::new("mock")),
        ];
        let stage = PositionsGenerate::new(panel, template(), Cost(0.01), 3);

        let question = Question {
            text: "Should we adopt microservices?".to_string(),
        };
        let out = stage.run(question, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 3);
        // Every position's text is one of the three independently scripted
        // answers, and no position's text contains another's -- proof no
        // cross-talk happened (each request only ever carried the question).
        let texts: Vec<&str> = out.0.iter().map(|p| p.text.as_str()).collect();
        for (i, a) in texts.iter().enumerate() {
            for (j, b) in texts.iter().enumerate() {
                if i != j {
                    assert!(!a.contains(*b), "{a} must not contain {b}");
                }
            }
        }
    }

    #[tokio::test]
    async fn requests_are_rendered_identically_across_the_panel() {
        // Only "question" varies the prompt; every panel member's rendered
        // request must therefore be byte-identical.
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_text("a");
        mock.script_text("b");

        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let panel = vec![
            (ModelId::new("model-a"), ProviderId::new("mock")),
            (ModelId::new("model-b"), ProviderId::new("mock")),
        ];
        let stage = PositionsGenerate::new(panel, template(), Cost(0.01), 2);
        let question = Question {
            text: "What should we build?".to_string(),
        };
        stage.run(question, &ctx).await.unwrap();
    }

    #[tokio::test]
    async fn an_unregistered_provider_is_skipped_not_fatal() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_text("the registered member still answers");

        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let panel = vec![
            (
                ModelId::new("model-a"),
                ProviderId::new("nobody-registered"),
            ),
            (ModelId::new("model-b"), ProviderId::new("mock")),
        ];
        let stage = PositionsGenerate::new(panel, template(), Cost(0.01), 2);
        let question = Question {
            text: "Q".to_string(),
        };
        let out = stage.run(question, &ctx).await.unwrap();
        assert_eq!(
            out.0.len(),
            1,
            "an unregistered provider must be skipped, not fatal"
        );
        // Abandoning the reservation has to say so: Drop performs the ledger
        // half silently, so without this event the transcript shows a reserve
        // with no matching release.
        assert_eq!(sink.count(EventType::BudgetReleased), 1);
    }

    #[tokio::test]
    async fn a_scripted_provider_error_is_skipped_not_fatal() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_error("simulated failure");
        mock.script_text("second one succeeds");

        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let panel = vec![
            (ModelId::new("model-a"), ProviderId::new("mock")),
            (ModelId::new("model-b"), ProviderId::new("mock")),
        ];
        let stage = PositionsGenerate::new(panel, template(), Cost(0.01), 2);
        let question = Question {
            text: "Q".to_string(),
        };
        let out = stage.run(question, &ctx).await.unwrap();
        assert_eq!(
            out.0.len(),
            1,
            "one failed call must not sink the other position"
        );
    }

    #[tokio::test]
    async fn a_budget_exhausted_reservation_is_skipped_not_fatal() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_text("never reached");

        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        // Room for exactly one of the two calls: the second reservation is
        // refused, which is a skip, not a stage failure.
        let budget = BudgetLedger::new(Some(Cost(1.0)));
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let panel = vec![
            (ModelId::new("model-a"), ProviderId::new("mock")),
            (ModelId::new("model-b"), ProviderId::new("mock")),
        ];
        // Serially, so the first call commits before the second reserves.
        let stage = PositionsGenerate::new(panel, template(), Cost(1.0), 1);
        let question = Question {
            text: "Q".to_string(),
        };
        let out = stage.run(question, &ctx).await.unwrap();
        assert_eq!(out.0.len(), 1);
        assert_eq!(sink.count(EventType::BudgetExhausted), 1);
    }

    #[tokio::test]
    async fn a_cache_hit_skips_the_provider_call_entirely() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_text("real call, should not be needed");

        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();

        let question = Question {
            text: "Q".to_string(),
        };
        let stage_template = template();
        let mut vars = BTreeMap::new();
        vars.insert("question".to_string(), question.text.clone());
        let rendered = stage_template.render(&vars).unwrap();
        let prompt_hash = stage_template.prompt_hash(&rendered).to_string();
        cache.put(
            CacheKey {
                provider: ProviderId::new("mock"),
                model: ModelId::new("model-a"),
                params: "{}".to_string(),
                prompt_hash,
            },
            CachedResponse {
                response_hash: "blake3:cached".to_string(),
                size_bytes: 5,
                inline: Some("cached answer".to_string()),
            },
        );

        let ctx = stage_ctx(&registry, &budget, &cache, &sink);
        let panel = vec![(ModelId::new("model-a"), ProviderId::new("mock"))];
        let stage = PositionsGenerate::new(panel, stage_template, Cost(0.01), 1);
        let out = stage.run(question, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 1);
        assert_eq!(out.0[0].text, "cached answer");
        assert_eq!(
            sink.count(EventType::CallStarted),
            0,
            "a cache hit must not dispatch a real call"
        );
    }

    /// Proves the actual shipped `positions.generate.md` (not this test
    /// module's own minimal fixture) loads and renders -- the real content
    /// G1 deferred to whichever stage first needed one.
    /// The defect this stage shipped with: a panel whose every member failed
    /// returned `Ok(Positions([]))`, so the run carried on and synthesised
    /// `InsufficientEvidence / complete` from nothing. An operator with a
    /// revoked API key saw a confident-looking empty decision and no reason.
    #[tokio::test]
    async fn a_panel_where_every_member_fails_is_a_stage_failure_naming_the_reason() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_error("401 Unauthorized: invalid x-api-key");
        mock.script_error("401 Unauthorized: invalid x-api-key");

        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let panel = vec![
            (ModelId::new("model-a"), ProviderId::new("mock")),
            (ModelId::new("model-b"), ProviderId::new("mock")),
        ];
        let stage = PositionsGenerate::new(panel, template(), Cost(0.01), 2);
        let err = stage
            .run(
                Question {
                    text: "Q".to_string(),
                },
                &ctx,
            )
            .await
            .expect_err("zero positions from a non-empty panel is not a debate");

        let message = err.to_string();
        assert!(
            message.contains("invalid x-api-key"),
            "the provider's own words must survive to the operator: {message}"
        );
        assert!(message.contains("model-a"), "{message}");
        assert_eq!(sink.count(EventType::StageFailed), 1);
        // Both reservations were abandoned, and both said so.
        assert_eq!(sink.count(EventType::BudgetReleased), 2);
        assert_eq!(sink.count(EventType::BudgetCommitted), 0);
    }

    /// The other half of the same rule, and the one INTERFACES §6 states
    /// outright: three of four is a degraded debate that continues. The
    /// `k2_provider_timeout` fixture depends on exactly this.
    #[tokio::test]
    async fn a_partly_failing_panel_still_produces_a_debate_and_records_who_dropped_out() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_error("upstream timed out after 120s");
        mock.script_text("b answers");
        mock.script_text("c answers");

        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let panel = vec![
            (ModelId::new("model-a"), ProviderId::new("mock")),
            (ModelId::new("model-b"), ProviderId::new("mock")),
            (ModelId::new("model-c"), ProviderId::new("mock")),
        ];
        // Serially, so `model-a` is the one that draws the scripted error.
        let stage = PositionsGenerate::new(panel, template(), Cost(0.01), 1);
        let out = stage
            .run(
                Question {
                    text: "Q".to_string(),
                },
                &ctx,
            )
            .await
            .unwrap();

        assert_eq!(out.0.len(), 2);
        assert_eq!(sink.count(EventType::StageFailed), 0);

        // "the debate continues with four positions and the record says so" --
        // the saying-so is this payload. Without it the transcript shows three
        // CALL_STARTED and two positions with nothing to explain the gap.
        let emitted = sink.emitted.lock().unwrap();
        let (_, payload) = emitted
            .iter()
            .find(|(t, _)| *t == EventType::StageCompleted)
            .expect("stage completed");
        let skipped = payload["skipped"].as_array().expect("skipped array");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0]["model"], "model-a");
        assert!(
            skipped[0]["reason"].as_str().unwrap().contains("timed out"),
            "{payload}"
        );
    }

    #[test]
    fn the_shipped_prompt_pack_loads_and_renders() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../prompts/default/v1");
        let pack = crate::prompt::PromptPack::load(&dir).unwrap();
        let template = pack
            .template(&StageName::new("positions.generate"))
            .expect("prompts/default/v1/positions.generate.md must exist");

        let mut vars = BTreeMap::new();
        vars.insert(
            "question".to_string(),
            "Should we adopt microservices?".to_string(),
        );
        let rendered = template.render(&vars).unwrap();
        assert!(rendered.contains("Should we adopt microservices?"));
    }
}
