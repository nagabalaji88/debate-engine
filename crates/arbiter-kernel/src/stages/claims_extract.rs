//! `claims.extract` (ARCHITECTURE §5.1 / INTERFACES §2): structured claims plus
//! grounding, with a bounded repair loop. Per position:
//!
//! ```text
//! position text -> extractor -> structured claim candidates
//!                     each candidate -> grounding check
//!                       exact substring match           -> DirectQuote
//!                       fuzzy match (trigram Jaccard)    -> DirectQuote
//!                       premises resolve + acyclic       -> Derived
//!                       neither                          -> repair once
//!                         still neither                  -> Unsupported (weight 0.15)
//! ```
//!
//! Premise cycles (INTERFACES §2's own worked protocol) are untangled before
//! anything is degraded: the same repair call is asked to name a base premise
//! or mark the cycle's members independent; if it still doesn't resolve, the
//! lowest-confidence edges in the cycle are cut (greedily — see this module's
//! own scope note on the exact-vs-greedy simplification, D32) and grounding is
//! re-checked only for the claims that lost a premise.

use super::positions_generate::{Position, Positions};
use crate::event::EventType;
use crate::ids::{CallId, ReservationId, StageName};
use crate::prompt::PromptTemplate;
use crate::provider::ProviderRequest;
use crate::stage::{
    CostEstimate, FailurePolicy, Key, Parallelism, RunContext, Stage, StageContext, StageError,
    idempotency_key,
};
use crate::store::{Artifact, Cost};
use arbiter_core::{
    CanonicalClaim, ClaimId, ClaimLifecycle, ClaimMember, EvidenceKind, Grounding, ModelId,
    ProviderId, TextSpan,
};
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// The extractor's per-claim output, INTERFACES §2's own JSON shape:
/// `{"text": "…", "kind": "fact"|"inference", "grounding": {"quote": "…"} | {"derived_from": […], "confidence": …}}`.
/// `confidence` on an inference is not in either of INTERFACES §2's two
/// example objects, but the cycle-cutting protocol names "ascending extractor
/// confidence" as its tie-break (PLAN_DEVIATIONS.md D32) — there is nowhere
/// else that value could come from, so it is added here as an
/// extractor-supplied field, defaulting to a neutral 0.5 when a real model
/// omits it (a scripted test fixture, say).
#[derive(Debug, Clone, Deserialize)]
struct RawGrounding {
    quote: Option<String>,
    derived_from: Option<Vec<String>>,
    confidence: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawCandidate {
    text: String,
    kind: String,
    grounding: RawGrounding,
}

/// One repaired claim, keyed back to its original 1-based index by the
/// repair prompt's own contract (`prompts/default/v1/claims.repair.md`).
#[derive(Debug, Clone, Deserialize)]
struct RawRepair {
    index: String,
    kind: String,
    grounding: RawGrounding,
}

/// `claims.extract`'s output: one singleton [`CanonicalClaim`] per extracted
/// claim (one member each) — `claims.normalize` (a later stage) is what
/// clusters equivalent members across positions into multi-member canonical
/// claims; extraction only ever mints provisional, per-position identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedClaims(pub Vec<CanonicalClaim>);

impl Artifact for ExtractedClaims {
    fn artifact_type(&self) -> &'static str {
        "extracted_claims.v1"
    }
    fn content_hash(&self) -> String {
        let mut ids: Vec<&str> = self.0.iter().map(|c| c.id.as_str()).collect();
        ids.sort_unstable();
        let canonical: Vec<serde_json::Value> = self
            .0
            .iter()
            .collect::<Vec<_>>()
            .into_iter()
            .map(claim_json)
            .collect();
        let mut pairs: Vec<(String, serde_json::Value)> = ids
            .iter()
            .zip(canonical)
            .map(|(id, v)| (id.to_string(), v))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let text = format!(
            "{}\u{1}{}",
            self.artifact_type(),
            serde_json::to_string(&pairs).expect("claims serialize")
        );
        format!("blake3:{}", blake3::hash(text.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Array(self.0.iter().map(claim_json).collect())
    }
}

fn claim_json(c: &CanonicalClaim) -> serde_json::Value {
    serde_json::json!({
        "id": c.id.as_str(),
        "text": c.text,
        "kind": format!("{:?}", c.kind),
        "members": c.members.iter().map(|m| serde_json::json!({
            "model": m.model.as_str(),
            "provider": m.provider.as_str(),
            "position": m.position.as_str(),
            "original_text": m.original_text,
        })).collect::<Vec<_>>(),
    })
}

/// `claims.extract`. Constructed with the loaded extract/repair templates, the
/// repair model to call, and this stage invocation's own repair budget cap
/// (`bounds::repair_budget`'s result — computed by the caller, since this
/// crate's `bounds.rs` already owns that formula).
#[derive(Debug)]
pub struct ClaimsExtract {
    extract_template: PromptTemplate,
    repair_template: PromptTemplate,
    repair_model: (ModelId, ProviderId),
    estimated_cost_per_call: Cost,
    repair_budget_cap: Cost,
    repair_spent: Mutex<f64>,
    max_parallelism: usize,
    fuzzy_match_threshold: f64,
}

impl ClaimsExtract {
    pub fn new(
        extract_template: PromptTemplate,
        repair_template: PromptTemplate,
        repair_model: (ModelId, ProviderId),
        estimated_cost_per_call: Cost,
        repair_budget_cap: Cost,
        max_parallelism: usize,
    ) -> Self {
        Self {
            extract_template,
            repair_template,
            repair_model,
            estimated_cost_per_call,
            repair_budget_cap,
            repair_spent: Mutex::new(0.0),
            max_parallelism: max_parallelism.max(1),
            fuzzy_match_threshold: 0.85,
        }
    }

    /// `true` and reserves the spend if the cumulative repair budget still has
    /// room; `false` (nothing reserved) once `repair_budget_fraction` has been
    /// exhausted — "whichever binds first stops repairs, and remaining
    /// failures are admitted as `Unsupported`" (INTERFACES §2).
    fn try_spend_repair_budget(&self, amount: Cost) -> bool {
        let mut spent = self.repair_spent.lock().unwrap();
        if *spent + amount.0 > self.repair_budget_cap.0 {
            return false;
        }
        *spent += amount.0;
        true
    }

    async fn extract_one(
        &self,
        position: &Position,
        ctx: &StageContext<'_>,
    ) -> Vec<CanonicalClaim> {
        if ctx.cancel.is_cancelled() {
            return Vec::new();
        }

        let stage_name = self.name();
        let mut vars = BTreeMap::new();
        vars.insert("position_text".to_string(), position.text.clone());
        let Ok(rendered) = self.extract_template.render(&vars) else {
            return Vec::new();
        };

        let Some(response_text) = self
            .call_provider(
                &position.provider,
                &position.model,
                rendered,
                &self.extract_template,
                ctx,
                "claims.extract",
            )
            .await
        else {
            return Vec::new();
        };

        let Ok(mut candidates) = serde_json::from_str::<Vec<RawCandidate>>(&response_text) else {
            ctx.events.emit(
                EventType::ClaimUngrounded,
                &stage_name,
                serde_json::json!({"position": position.id.as_str(), "reason": "unparseable extraction response"}),
            );
            return Vec::new();
        };

        let mut resolution = resolve(&candidates, &position.text, self.fuzzy_match_threshold);

        if resolution_needs_repair(&candidates, &resolution) {
            let repair_amount = self.estimated_cost_per_call;
            if self.try_spend_repair_budget(repair_amount)
                && let Some(repair_response) = self
                    .run_repair(position, &candidates, &resolution, ctx)
                    .await
                && let Ok(repairs) = serde_json::from_str::<Vec<RawRepair>>(&repair_response)
            {
                apply_repairs(&mut candidates, &repairs);
                resolution = resolve(&candidates, &position.text, self.fuzzy_match_threshold);
            }
        }

        // Cycle still unresolved after (at most) one repair pass: cut the
        // lowest-confidence edges within it and re-check (INTERFACES §2 step
        // 2 of the untangle protocol; D32 documents the greedy-only scope).
        if let Some(cycle) = resolution.cycle.clone() {
            let mut edges = resolution.premise_edges.clone();
            cut_cycle_edges(&mut edges, &cycle);
            resolution = resolve_with_edges(
                &candidates,
                &position.text,
                self.fuzzy_match_threshold,
                edges,
            );
        }

        for (i, _) in candidates.iter().enumerate() {
            if !resolution.grounded.contains_key(&i) {
                ctx.events.emit(
                    EventType::ClaimUngrounded,
                    &stage_name,
                    serde_json::json!({
                        "position": position.id.as_str(),
                        "claim_index": i + 1,
                    }),
                );
            }
        }

        build_claims(&candidates, &resolution.grounded, position)
    }

    async fn run_repair(
        &self,
        position: &Position,
        candidates: &[RawCandidate],
        resolution: &Resolution,
        ctx: &StageContext<'_>,
    ) -> Option<String> {
        let failed_text = render_failed_claims(candidates, resolution);
        let mut vars = BTreeMap::new();
        vars.insert("position_text".to_string(), position.text.clone());
        vars.insert("failed_claims".to_string(), failed_text);
        let rendered = self.repair_template.render(&vars).ok()?;

        self.call_provider(
            &self.repair_model.1.clone(),
            &self.repair_model.0.clone(),
            rendered,
            &self.repair_template,
            ctx,
            "claims.repair",
        )
        .await
    }

    /// Shared cache-then-reserve-then-call-then-commit path, the same
    /// sequence `positions.generate` established (D31) — extraction and
    /// repair calls are both ordinary provider calls and follow it identically.
    async fn call_provider(
        &self,
        provider_id: &ProviderId,
        model: &ModelId,
        rendered: String,
        template: &PromptTemplate,
        ctx: &StageContext<'_>,
        call_label: &str,
    ) -> Option<String> {
        let stage_name = self.name();
        let prompt_hash = template.prompt_hash(&rendered).to_string();

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

        let provider = ctx.providers.get(provider_id)?;
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
}

impl Stage for ClaimsExtract {
    type In = Positions;
    type Out = ExtractedClaims;

    fn name(&self) -> StageName {
        StageName::new("claims.extract")
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
        // One extraction call per position, plus a worst-case repair call per
        // position (INTERFACES §2: "one extra call per position").
        let calls = input.0.len() as u32 * 2;
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
            serde_json::json!({"positions": input.0.len()}),
        );

        let all: Vec<Vec<CanonicalClaim>> = futures_util::stream::iter(input.0.iter().cloned())
            .map(|position| async move { self.extract_one(&position, ctx).await })
            .buffer_unordered(self.max_parallelism)
            .collect()
            .await;

        let mut claims: Vec<CanonicalClaim> = all.into_iter().flatten().collect();
        claims.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));

        ctx.events.emit(
            EventType::StageCompleted,
            &self.name(),
            serde_json::json!({"claims": claims.len()}),
        );

        Ok(ExtractedClaims(claims))
    }
}

// ---------------------------------------------------------------------------
// Grounding resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct Resolution {
    grounded: BTreeMap<usize, Grounding>,
    /// node -> premises it depends on, with the extractor's confidence in
    /// that specific edge (only inference-kind candidates with a
    /// `derived_from` list appear here).
    premise_edges: BTreeMap<usize, Vec<(usize, f64)>>,
    cycle: Option<BTreeSet<usize>>,
}

fn resolve(candidates: &[RawCandidate], position_text: &str, fuzzy_threshold: f64) -> Resolution {
    let edges = build_premise_edges(candidates);
    resolve_with_edges(candidates, position_text, fuzzy_threshold, edges)
}

fn build_premise_edges(candidates: &[RawCandidate]) -> BTreeMap<usize, Vec<(usize, f64)>> {
    let n = candidates.len();
    let mut edges = BTreeMap::new();
    for (i, c) in candidates.iter().enumerate() {
        if c.kind == "inference"
            && let Some(refs) = &c.grounding.derived_from
        {
            let confidence = c.grounding.confidence.unwrap_or(0.5);
            let premises: Vec<(usize, f64)> = refs
                .iter()
                .filter_map(|r| parse_ref(r, n))
                .map(|p| (p, confidence))
                .collect();
            if !premises.is_empty() {
                edges.insert(i, premises);
            }
        }
    }
    edges
}

fn resolve_with_edges(
    candidates: &[RawCandidate],
    position_text: &str,
    fuzzy_threshold: f64,
    edges: BTreeMap<usize, Vec<(usize, f64)>>,
) -> Resolution {
    let mut grounded: BTreeMap<usize, Grounding> = BTreeMap::new();

    // Step 1/2: exact, then fuzzy match, for every candidate that declares a quote.
    for (i, c) in candidates.iter().enumerate() {
        if let Some(quote) = &c.grounding.quote {
            if let Some(span) = find_exact_match(quote, position_text) {
                grounded.insert(i, Grounding::DirectQuote { span });
            } else if let Some(span) = find_fuzzy_match(quote, position_text, fuzzy_threshold) {
                grounded.insert(i, Grounding::DirectQuote { span });
            }
        }
    }

    // Step 3: derived, only where the premise graph (restricted to inference
    // candidates) is acyclic.
    let nodes: BTreeSet<usize> = edges.keys().copied().collect();
    let deps_only: BTreeMap<usize, Vec<usize>> = edges
        .iter()
        .map(|(&k, v)| (k, v.iter().map(|&(p, _)| p).collect()))
        .collect();

    let cycle = match topo_sort(&nodes, &deps_only) {
        Ok(order) => {
            for i in order {
                if grounded.contains_key(&i) {
                    continue;
                }
                if let Some(premises) = edges.get(&i)
                    && !premises.is_empty()
                    && premises.iter().all(|(p, _)| grounded.contains_key(p))
                {
                    let claim_premises: Vec<ClaimId> = premises
                        .iter()
                        .map(|(p, _)| ClaimId::new(format!("__local_{p}")))
                        .collect();
                    grounded.insert(
                        i,
                        Grounding::Derived {
                            premises: claim_premises,
                        },
                    );
                }
            }
            None
        }
        Err(cyclic) => Some(cyclic),
    };

    Resolution {
        grounded,
        premise_edges: edges,
        cycle,
    }
}

/// A candidate list's own `needs_repair` check, done against the real
/// candidate count rather than [`Resolution::needs_repair`]'s placeholder.
fn resolution_needs_repair(candidates: &[RawCandidate], resolution: &Resolution) -> bool {
    resolution.cycle.is_some() || resolution.grounded.len() < candidates.len()
}

fn parse_ref(r: &str, n: usize) -> Option<usize> {
    let idx: usize = r.strip_prefix('#')?.parse().ok()?;
    if idx == 0 || idx > n {
        return None;
    }
    Some(idx - 1)
}

/// Standard Kahn's algorithm. `edges[node]` lists the premises `node`
/// depends on. Returns the topological order (premises before dependents) or
/// the set of nodes still blocked once no more progress can be made — every
/// node in that set is either on a cycle or depends (transitively) on one.
fn topo_sort(
    nodes: &BTreeSet<usize>,
    edges: &BTreeMap<usize, Vec<usize>>,
) -> Result<Vec<usize>, BTreeSet<usize>> {
    let mut remaining_deps: BTreeMap<usize, BTreeSet<usize>> = nodes
        .iter()
        .map(|&n| {
            let deps: BTreeSet<usize> = edges
                .get(&n)
                .into_iter()
                .flatten()
                .copied()
                .filter(|d| nodes.contains(d))
                .collect();
            (n, deps)
        })
        .collect();

    let mut order = Vec::new();
    loop {
        let ready: Vec<usize> = remaining_deps
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(&n, _)| n)
            .collect();
        if ready.is_empty() {
            break;
        }
        for n in &ready {
            remaining_deps.remove(n);
        }
        for deps in remaining_deps.values_mut() {
            for n in &ready {
                deps.remove(n);
            }
        }
        order.extend(ready);
    }

    if remaining_deps.is_empty() {
        Ok(order)
    } else {
        Err(remaining_deps.keys().copied().collect())
    }
}

/// INTERFACES §2's cycle-cutting fallback, applied uniformly (D32): removes
/// the lowest-confidence edge among those still inside the cyclic set,
/// re-checks, and repeats until acyclic or no cuttable edge remains.
fn cut_cycle_edges(edges: &mut BTreeMap<usize, Vec<(usize, f64)>>, cycle: &BTreeSet<usize>) {
    loop {
        let nodes: BTreeSet<usize> = edges.keys().copied().collect();
        let deps_only: BTreeMap<usize, Vec<usize>> = edges
            .iter()
            .map(|(&k, v)| (k, v.iter().map(|&(p, _)| p).collect()))
            .collect();
        let cyclic = match topo_sort(&nodes, &deps_only) {
            Ok(_) => return,
            Err(cyclic) => cyclic,
        };
        let cyclic: BTreeSet<usize> = cyclic.intersection(cycle).copied().collect();
        if cyclic.is_empty() {
            return;
        }

        let mut candidates: Vec<(usize, usize, f64)> = Vec::new();
        for &u in &cyclic {
            if let Some(list) = edges.get(&u) {
                for &(v, conf) in list {
                    if cyclic.contains(&v) {
                        candidates.push((u, v, conf));
                    }
                }
            }
        }
        candidates.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
        let Some(&(u, v, _)) = candidates.first() else {
            return;
        };
        if let Some(list) = edges.get_mut(&u) {
            list.retain(|&(p, _)| p != v);
        }
    }
}

fn render_failed_claims(candidates: &[RawCandidate], resolution: &Resolution) -> String {
    let mut lines = Vec::new();
    for (i, c) in candidates.iter().enumerate() {
        if resolution.grounded.contains_key(&i) {
            continue;
        }
        lines.push(format!("#{} ({}): {}", i + 1, c.kind, c.text));
    }
    if let Some(cycle) = &resolution.cycle {
        let members: Vec<String> = cycle.iter().map(|i| format!("#{}", i + 1)).collect();
        lines.push(format!(
            "Note: claims {} cite each other as premises (a cycle) and cannot all be derived as written.",
            members.join(", ")
        ));
    }
    lines.join("\n")
}

fn apply_repairs(candidates: &mut [RawCandidate], repairs: &[RawRepair]) {
    for r in repairs {
        let Some(idx) = parse_ref(&r.index, candidates.len()) else {
            continue;
        };
        candidates[idx].kind = r.kind.clone();
        candidates[idx].grounding = r.grounding.clone();
    }
}

fn build_claims(
    candidates: &[RawCandidate],
    grounded: &BTreeMap<usize, Grounding>,
    position: &Position,
) -> Vec<CanonicalClaim> {
    candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let grounding = grounded.get(&i).cloned().unwrap_or(Grounding::Unsupported);
            let claim_id = ClaimId::new(format!("claim_{}_{}", position.id.as_str(), i + 1));
            let grounding = resolve_local_premise_ids(grounding, position);
            let kind = match (&grounding, c.kind.as_str()) {
                (Grounding::Unsupported, _) => EvidenceKind::Unverified,
                (_, "inference") => EvidenceKind::Inference,
                _ => EvidenceKind::Fact,
            };
            let member = ClaimMember::new(
                claim_id.clone(),
                position.model.clone(),
                position.provider.clone(),
                position.id.clone(),
                c.text.clone(),
                grounding,
            );
            CanonicalClaim {
                id: claim_id,
                text: c.text.clone(),
                kind,
                lifecycle: ClaimLifecycle::Proposed,
                members: vec![member],
            }
        })
        .collect()
}

/// [`resolve_with_edges`] mints placeholder `__local_N` ids for premises,
/// since real [`ClaimId`]s aren't assigned until every candidate in the
/// position has an index to derive one from; this rewrites them to the real
/// per-position claim ids once that's known.
fn resolve_local_premise_ids(grounding: Grounding, position: &Position) -> Grounding {
    match grounding {
        Grounding::Derived { premises } => Grounding::Derived {
            premises: premises
                .into_iter()
                .map(|p| rewrite_local_id(&p, position))
                .collect(),
        },
        other => other,
    }
}

fn rewrite_local_id(id: &ClaimId, position: &Position) -> ClaimId {
    if let Some(n) = id.as_str().strip_prefix("__local_")
        && let Ok(n) = n.parse::<usize>()
    {
        return ClaimId::new(format!("claim_{}_{}", position.id.as_str(), n + 1));
    }
    id.clone()
}

// ---------------------------------------------------------------------------
// Matching: exact and fuzzy substring search
// ---------------------------------------------------------------------------

/// Whitespace-delimited token spans (byte offsets) over `text`.
fn tokenize_with_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (i, ch) in text.char_indices() {
        if ch.is_whitespace() {
            if let Some(s) = start.take() {
                spans.push((s, i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        spans.push((s, text.len()));
    }
    spans
}

/// "Whitespace- and case-normalised substring search" (INTERFACES §2): a
/// sliding window of whitespace-delimited tokens, compared case-insensitively
/// token-for-token. Token-based rather than a literal
/// normalize-then-substring-find, since the latter would need a lossy
/// index-mapping step back to the original text's byte offsets once
/// normalization changes the string's length.
fn find_exact_match(quote: &str, haystack: &str) -> Option<TextSpan> {
    let quote_tokens: Vec<String> = quote.split_whitespace().map(|t| t.to_lowercase()).collect();
    if quote_tokens.is_empty() {
        return None;
    }
    let haystack_spans = tokenize_with_spans(haystack);
    if haystack_spans.len() < quote_tokens.len() {
        return None;
    }
    let haystack_tokens: Vec<String> = haystack_spans
        .iter()
        .map(|&(s, e)| haystack[s..e].to_lowercase())
        .collect();
    for start_idx in 0..=(haystack_tokens.len() - quote_tokens.len()) {
        let window = &haystack_tokens[start_idx..start_idx + quote_tokens.len()];
        if window == quote_tokens.as_slice() {
            let (span_start, _) = haystack_spans[start_idx];
            let (_, span_end) = haystack_spans[start_idx + quote_tokens.len() - 1];
            return Some(TextSpan {
                start: span_start,
                end: span_end,
                quote: haystack[span_start..span_end].to_string(),
            });
        }
    }
    None
}

fn char_trigrams(s: &str) -> BTreeSet<[char; 3]> {
    let chars: Vec<char> = s.chars().collect();
    let mut set = BTreeSet::new();
    if chars.len() < 3 {
        return set;
    }
    for w in chars.windows(3) {
        set.insert([w[0], w[1], w[2]]);
    }
    set
}

fn trigram_jaccard(a: &str, b: &str) -> f64 {
    let ta = char_trigrams(&a.to_lowercase());
    let tb = char_trigrams(&b.to_lowercase());
    if ta.is_empty() && tb.is_empty() {
        return 1.0;
    }
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let inter = ta.intersection(&tb).count();
    let union = ta.union(&tb).count();
    inter as f64 / union as f64
}

/// "Fuzzy match — trigram Jaccard ≥ 0.85 over a sliding window the length of
/// the quote" (INTERFACES §2). The window slides in whole tokens (matching
/// [`find_exact_match`]'s own tokenization) so both matchers agree on what a
/// "span" is; the score itself is computed over characters, per the spec.
fn find_fuzzy_match(quote: &str, haystack: &str, threshold: f64) -> Option<TextSpan> {
    let quote_token_count = quote.split_whitespace().count();
    if quote_token_count == 0 {
        return None;
    }
    let haystack_spans = tokenize_with_spans(haystack);
    if haystack_spans.len() < quote_token_count {
        return None;
    }
    let mut best: Option<(f64, usize, usize)> = None;
    for start_idx in 0..=(haystack_spans.len() - quote_token_count) {
        let (span_start, _) = haystack_spans[start_idx];
        let (_, span_end) = haystack_spans[start_idx + quote_token_count - 1];
        let window_text = &haystack[span_start..span_end];
        let score = trigram_jaccard(quote, window_text);
        if best.map(|(b, _, _)| score > b).unwrap_or(true) {
            best = Some((score, span_start, span_end));
        }
    }
    let (score, start, end) = best?;
    if score >= threshold {
        Some(TextSpan {
            start,
            end,
            quote: haystack[start..end].to_string(),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetLedger;
    use crate::cache::ResponseCache;
    use crate::provider::{Provider, ProviderCapabilities, ProviderError, ProviderResponse};
    use crate::stage::{CancellationToken, DeterministicRng, EventSink, ProviderRegistry};
    use arbiter_core::PositionId;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;

    // ---- pure-function tests -------------------------------------------------

    #[test]
    fn exact_match_is_case_and_whitespace_tolerant() {
        let haystack = "Our team   has\n8 developers on staff.";
        let span = find_exact_match("our team has 8 developers", haystack).unwrap();
        assert_eq!(
            &haystack[span.start..span.end],
            "Our team   has\n8 developers"
        );
    }

    #[test]
    fn exact_match_fails_on_a_genuinely_different_substring() {
        assert!(find_exact_match("modular monolith", "microservices are complex").is_none());
    }

    #[test]
    fn fuzzy_match_finds_a_near_paraphrase_above_threshold() {
        let haystack = "container orchestration maintenance workload is substantial";
        let span = find_fuzzy_match("container orchestration maintenance", haystack, 0.85);
        assert!(span.is_some(), "a near-identical window should fuzzy-match");
    }

    #[test]
    fn fuzzy_match_rejects_a_genuinely_unrelated_window() {
        let haystack = "the weather today is sunny and warm";
        assert!(find_fuzzy_match("kubernetes deployment overhead", haystack, 0.85).is_none());
    }

    #[test]
    fn trigram_jaccard_of_identical_strings_is_one() {
        assert!((trigram_jaccard("hello world", "hello world") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn topo_sort_orders_premises_before_dependents() {
        let nodes: BTreeSet<usize> = [0usize, 1, 2].into_iter().collect();
        let mut edges = BTreeMap::new();
        edges.insert(1usize, vec![0usize]); // 1 depends on 0
        edges.insert(2usize, vec![1usize]); // 2 depends on 1
        let order = topo_sort(&nodes, &edges).unwrap();
        let pos = |n: usize| order.iter().position(|&x| x == n).unwrap();
        assert!(pos(0) < pos(1));
        assert!(pos(1) < pos(2));
    }

    #[test]
    fn topo_sort_detects_a_two_node_cycle() {
        let nodes: BTreeSet<usize> = [0usize, 1].into_iter().collect();
        let mut edges = BTreeMap::new();
        edges.insert(0usize, vec![1usize]);
        edges.insert(1usize, vec![0usize]);
        let result = topo_sort(&nodes, &edges);
        assert_eq!(result, Err([0usize, 1].into_iter().collect()));
    }

    #[test]
    fn cut_cycle_edges_removes_the_lowest_confidence_edge_first() {
        let mut edges = BTreeMap::new();
        edges.insert(0usize, vec![(1usize, 0.9)]);
        edges.insert(1usize, vec![(0usize, 0.2)]);
        let cycle: BTreeSet<usize> = [0, 1].into_iter().collect();
        cut_cycle_edges(&mut edges, &cycle);

        // The 0.2-confidence edge (1 -> 0) must be the one cut; 0 -> 1 (0.9)
        // survives.
        assert_eq!(edges.get(&1).map(|v| v.as_slice()), Some(&[][..]));
        assert_eq!(edges.get(&0).unwrap(), &vec![(1usize, 0.9)]);
    }

    // ---- stage-level tests ----------------------------------------------------

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
        fn script_text(&self, text: impl Into<String>) {
            self.script.lock().unwrap().push_back(Ok(ProviderResponse {
                text: text.into(),
                prompt_tokens: 0,
                completion_tokens: 0,
                request_id: None,
            }));
        }
        /// Scripts a response built from a `serde_json::json!` value rather
        /// than a raw string literal -- the extraction/repair JSON this
        /// module tests is full of `"#1"`-style claim references, and a
        /// Rust raw string closes on the first bare `"#` it meets, which
        /// collides with that syntax.
        fn script_json(&self, value: serde_json::Value) {
            self.script_text(value.to_string());
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

    fn extract_template() -> PromptTemplate {
        PromptTemplate {
            stage: StageName::new("claims.extract"),
            body: "{{position_text}}".to_string(),
            variables: crate::prompt::VariableSchema::new(["position_text"]),
        }
    }

    fn repair_template() -> PromptTemplate {
        PromptTemplate {
            stage: StageName::new("claims.repair"),
            body: "{{position_text}} {{failed_claims}}".to_string(),
            variables: crate::prompt::VariableSchema::new(["position_text", "failed_claims"]),
        }
    }

    fn position(text: &str) -> Position {
        Position {
            id: PositionId::new("pos_mock_model-a"),
            model: ModelId::new("model-a"),
            provider: ProviderId::new("mock"),
            text: text.to_string(),
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

    fn stage(repair_cap: Cost) -> ClaimsExtract {
        ClaimsExtract::new(
            extract_template(),
            repair_template(),
            (ModelId::new("model-a"), ProviderId::new("mock")),
            Cost(0.01),
            repair_cap,
            1,
        )
    }

    #[tokio::test]
    async fn a_directly_quoted_claim_is_grounded_as_direct_quote() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([
            {"text": "we have 8 developers", "kind": "fact", "grounding": {"quote": "our team has 8 developers"}}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let positions = Positions(vec![position("Our team has 8 developers on staff.")]);
        let out = stage(Cost(0.0)).run(positions, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 1);
        let member = &out.0[0].members[0];
        assert!(matches!(member.grounding, Grounding::DirectQuote { .. }));
        assert_eq!(out.0[0].kind, EvidenceKind::Fact);
    }

    #[tokio::test]
    async fn a_derived_claim_resolves_from_a_grounded_premise() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([
            {"text": "we have 8 developers", "kind": "fact", "grounding": {"quote": "our team has 8 developers"}},
            {"text": "therefore a modular monolith is safer", "kind": "inference", "grounding": {"derived_from": ["#1"], "confidence": 0.9}}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let positions = Positions(vec![position("Our team has 8 developers on staff.")]);
        let out = stage(Cost(0.0)).run(positions, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 2);
        let inference = &out.0[1];
        assert!(matches!(
            inference.members[0].grounding,
            Grounding::Derived { .. }
        ));
        assert_eq!(inference.kind, EvidenceKind::Inference);
    }

    #[tokio::test]
    async fn an_unparseable_extraction_response_yields_no_claims_not_an_error() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_text("not json at all");
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let positions = Positions(vec![position("Our team has 8 developers.")]);
        let out = stage(Cost(0.0)).run(positions, &ctx).await.unwrap();
        assert_eq!(out.0.len(), 0);
    }

    #[tokio::test]
    async fn a_failed_claim_is_repaired_successfully() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        // Extraction: a quote that does not appear in the position at all.
        mock.script_json(serde_json::json!([
            {"text": "we have 8 developers", "kind": "fact", "grounding": {"quote": "totally wrong quote"}}
        ]));
        // Repair: corrects the quote to one that actually matches.
        mock.script_json(serde_json::json!([
            {"index": "#1", "kind": "fact", "grounding": {"quote": "our team has 8 developers"}}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let positions = Positions(vec![position("Our team has 8 developers on staff.")]);
        let out = stage(Cost(1.0)).run(positions, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 1);
        assert!(matches!(
            out.0[0].members[0].grounding,
            Grounding::DirectQuote { .. }
        ));
    }

    #[tokio::test]
    async fn a_claim_still_failing_after_repair_is_admitted_as_unsupported() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([
            {"text": "we have 8 developers", "kind": "fact", "grounding": {"quote": "totally wrong quote"}}
        ]));
        // Repair also fails to find a real quote.
        mock.script_json(serde_json::json!([
            {"index": "#1", "kind": "fact", "grounding": {"quote": "still wrong"}}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let positions = Positions(vec![position("Our team has 8 developers on staff.")]);
        let out = stage(Cost(1.0)).run(positions, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 1);
        assert_eq!(out.0[0].members[0].grounding, Grounding::Unsupported);
        assert_eq!(out.0[0].kind, EvidenceKind::Unverified);
        assert!(sink.count(EventType::ClaimUngrounded) >= 1);
    }

    #[tokio::test]
    async fn repair_is_skipped_once_the_repair_budget_is_exhausted() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([
            {"text": "we have 8 developers", "kind": "fact", "grounding": {"quote": "totally wrong quote"}}
        ]));
        // No repair response scripted -- if the stage tries to call repair
        // anyway, the script-exhausted error surfaces as a call failure
        // (still admitted Unsupported, not a panic), but the budget count
        // below proves repair was never attempted.
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let positions = Positions(vec![position("Our team has 8 developers on staff.")]);
        // Zero repair budget: try_spend_repair_budget must refuse.
        let out = stage(Cost(0.0)).run(positions, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 1);
        assert_eq!(out.0[0].members[0].grounding, Grounding::Unsupported);
        assert_eq!(
            sink.count(EventType::CallStarted),
            1,
            "only the extraction call should have run, never a repair call"
        );
    }

    #[tokio::test]
    async fn a_premise_cycle_resolved_by_repair_leaves_no_claim_unsupported() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        // #1 and #2 cite each other as premises.
        mock.script_json(serde_json::json!([
            {"text": "a", "kind": "inference", "grounding": {"derived_from": ["#2"], "confidence": 0.5}},
            {"text": "b", "kind": "inference", "grounding": {"derived_from": ["#1"], "confidence": 0.5}}
        ]));
        // Repair breaks the cycle: #1 gets a real quote, #2 still depends on #1.
        mock.script_json(serde_json::json!([
            {"index": "#1", "kind": "fact", "grounding": {"quote": "our team has 8 developers"}},
            {"index": "#2", "kind": "inference", "grounding": {"derived_from": ["#1"], "confidence": 0.9}}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let positions = Positions(vec![position("Our team has 8 developers on staff.")]);
        let out = stage(Cost(1.0)).run(positions, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 2);
        for claim in &out.0 {
            assert_ne!(
                claim.members[0].grounding,
                Grounding::Unsupported,
                "the repaired cycle must leave no claim unsupported"
            );
        }
    }

    #[tokio::test]
    async fn an_unresolved_cycle_falls_back_to_cutting_the_weakest_edge() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        // #2 and #3 cite each other (a cycle); #2 also depends on #1, which is
        // independently grounded by a real quote. #2's own declared
        // confidence (applied to both of its edges: to #1 and to #3) is the
        // lowest in the cycle, so the greedy cut removes #2 -> #3 first,
        // leaving #2 -> #1 intact.
        mock.script_json(serde_json::json!([
            {"text": "base fact", "kind": "fact", "grounding": {"quote": "our team has 8 developers"}},
            {"text": "a", "kind": "inference", "grounding": {"derived_from": ["#1", "#3"], "confidence": 0.1}},
            {"text": "b", "kind": "inference", "grounding": {"derived_from": ["#2"], "confidence": 0.9}}
        ]));
        // No repair scripted -- repair budget is zero, so the stage must not
        // even attempt to call it, and must fall through to the greedy cut.
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let positions = Positions(vec![position("Our team has 8 developers on staff.")]);
        let out = stage(Cost(0.0)).run(positions, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 3);
        // #1 (fact) keeps its DirectQuote grounding regardless.
        assert!(matches!(
            out.0[0].members[0].grounding,
            Grounding::DirectQuote { .. }
        ));
        // Cutting #2's weak (0.1) edge to #3 leaves #2 -> #1 intact, and #1
        // is grounded, so #2 resolves to Derived. #3's own edge to #2 then
        // also resolves, since #2 is now grounded.
        assert!(matches!(
            out.0[1].members[0].grounding,
            Grounding::Derived { .. }
        ));
        assert!(matches!(
            out.0[2].members[0].grounding,
            Grounding::Derived { .. }
        ));
    }

    #[test]
    fn the_shipped_extract_and_repair_prompts_load_and_render() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../prompts/default/v1");
        let pack = crate::prompt::PromptPack::load(&dir).unwrap();

        let extract = pack.template(&StageName::new("claims.extract")).unwrap();
        let mut vars = BTreeMap::new();
        vars.insert("position_text".to_string(), "some position".to_string());
        assert!(extract.render(&vars).unwrap().contains("some position"));

        let repair = pack.template(&StageName::new("claims.repair")).unwrap();
        let mut vars = BTreeMap::new();
        vars.insert("position_text".to_string(), "some position".to_string());
        vars.insert("failed_claims".to_string(), "#1: some claim".to_string());
        let rendered = repair.render(&vars).unwrap();
        assert!(rendered.contains("some position"));
        assert!(rendered.contains("#1: some claim"));
    }
}
