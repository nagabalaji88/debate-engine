//! `claims.normalize` (ARCHITECTURE §5's own words: "cluster equivalent claims
//! across models; members preserved | cheap similarity + LLM tie-break on
//! top-K"). Clusters the singleton `CanonicalClaim`s `claims.extract` mints
//! (one per extracted claim, one member each) into multi-member canonical
//! claims — the same underlying point, stated by different models, becomes
//! one claim with every original member preserved.
//!
//! Scope note (PLAN_DEVIATIONS.md D33): ARCHITECTURE's own line for this
//! stage is one sentence; the only concrete "cheap similarity" algorithm
//! given anywhere in either spec file is INTERFACES §3's T1 (lexical:
//! normalise → trigrams → IDF-weighted cosine → top-K) and T3 (one batched
//! LLM call: "group claims that state the same underlying point" — and
//! `prompts/<pack>/<version>/claims.group.md` is that call's own named file
//! in ARCHITECTURE §15's file list). §3 sits under a "Relationship
//! detection" heading and its own worked pipeline opens with "claims ->
//! normalise" — i.e. it is written for `relations.analyze`, which runs
//! *after* this stage. This task reuses exactly the T1/T3 half of that
//! machinery (the half that needs no `options.cluster` output, unlike T2's
//! polarity sweep) because it is the only concrete algorithm INTERFACES gives
//! for "cheap similarity" anywhere, and reusing it here rather than inventing
//! a second one is the more conservative reading. T2 stays `relations.analyze`'s
//! own scope, once options exist to sweep polarity against.

use super::claims_extract::ExtractedClaims;
use crate::event::EventType;
use crate::ids::{CallId, ReservationId, StageName};
use crate::prompt::PromptTemplate;
use crate::provider::ProviderRequest;
use crate::stage::{
    CostEstimate, FailurePolicy, Key, Parallelism, RunContext, Stage, StageContext, StageError,
    idempotency_key,
};
use crate::store::{Artifact, Cost};
use arbiter_core::{CanonicalClaim, ClaimLifecycle, EvidenceKind, ModelId, ProviderId};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

/// `claims.normalize`'s output: the same claims `claims.extract` produced,
/// clustered — equivalent members merged under one surviving id, everything
/// else passed through as its own singleton claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedClaims(pub Vec<CanonicalClaim>);

impl Artifact for NormalizedClaims {
    fn artifact_type(&self) -> &'static str {
        "normalized_claims.v1"
    }
    fn content_hash(&self) -> String {
        let mut pairs: Vec<(String, serde_json::Value)> = self
            .0
            .iter()
            .map(|c| (c.id.as_str().to_string(), claim_json(c)))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let text = serde_json::to_string(&pairs).expect("claims serialize");
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
        })).collect::<Vec<_>>(),
    })
}

#[derive(Debug, Deserialize)]
struct RawGroup {
    members: Vec<String>,
    confidence: f64,
}

#[derive(Debug)]
pub struct ClaimsNormalize {
    group_template: PromptTemplate,
    grouping_model: (ModelId, ProviderId),
    estimated_cost_per_call: Cost,
    /// INTERFACES §3: `t3_merge_threshold`, default 0.75. A group below this
    /// is split rather than merged.
    merge_threshold: f64,
    /// INTERFACES §3: `t3_max_claims_per_batch`, default 60.
    max_claims_per_batch: usize,
}

impl ClaimsNormalize {
    pub fn new(
        group_template: PromptTemplate,
        grouping_model: (ModelId, ProviderId),
        estimated_cost_per_call: Cost,
    ) -> Self {
        Self {
            group_template,
            grouping_model,
            estimated_cost_per_call,
            merge_threshold: 0.75,
            max_claims_per_batch: 60,
        }
    }

    async fn call_grouping(
        &self,
        claims_block: String,
        ctx: &StageContext<'_>,
        call_label: &str,
    ) -> Option<String> {
        let stage_name = self.name();
        let mut vars = BTreeMap::new();
        vars.insert("claims".to_string(), claims_block);
        let rendered = self.group_template.render(&vars).ok()?;
        let prompt_hash = self.group_template.prompt_hash(&rendered).to_string();

        let (model, provider_id) = self.grouping_model.clone();

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

    /// Runs one grouping call over `items` (index i -> text), unioning
    /// `uf`'s roots `base[i]` for every returned group whose confidence
    /// meets [`Self::merge_threshold`]. A group below threshold, or a
    /// response this binary cannot parse, changes nothing -- every item
    /// stays its own singleton, the conservative outcome (INTERFACES §3:
    /// "groups below `t3_merge_threshold` are split rather than merged").
    async fn group_and_union(
        &self,
        items: &[(usize, String)],
        base: &[usize],
        uf: &mut UnionFind,
        ctx: &StageContext<'_>,
        call_label: &str,
    ) {
        if items.len() < 2 {
            return;
        }
        let block = items
            .iter()
            .enumerate()
            .map(|(local_idx, (_, text))| format!("#{} {}", local_idx + 1, text))
            .collect::<Vec<_>>()
            .join("\n");

        let Some(response) = self.call_grouping(block, ctx, call_label).await else {
            return;
        };
        let Ok(groups) = serde_json::from_str::<Vec<RawGroup>>(&response) else {
            return;
        };

        for group in groups {
            if group.confidence < self.merge_threshold || group.members.len() < 2 {
                continue;
            }
            let local_indices: Vec<usize> = group
                .members
                .iter()
                .filter_map(|m| parse_local_ref(m, items.len()))
                .collect();
            let Some(&first) = local_indices.first() else {
                continue;
            };
            for &local in &local_indices[1..] {
                uf.union(base[first], base[local]);
            }
        }
    }
}

impl Stage for ClaimsNormalize {
    type In = ExtractedClaims;
    type Out = NormalizedClaims;

    fn name(&self) -> StageName {
        StageName::new("claims.normalize")
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
        let batches = input.0.len().div_ceil(self.max_claims_per_batch).max(1) as u32;
        CostEstimate {
            calls: batches,
            tokens: 0,
            cost: Cost(self.estimated_cost_per_call.0 * batches as f64),
        }
    }

    async fn run(&self, input: Self::In, ctx: &StageContext<'_>) -> Result<Self::Out, StageError> {
        let claims = input.0;
        let n = claims.len();
        ctx.events.emit(
            EventType::StageStarted,
            &self.name(),
            serde_json::json!({"claims": n}),
        );

        if n < 2 {
            ctx.events.emit(
                EventType::StageCompleted,
                &self.name(),
                serde_json::json!({"claims": n, "merged": 0}),
            );
            return Ok(NormalizedClaims(claims));
        }

        // T1: cheap lexical candidate pairs, always computed -- used to
        // partition above the batch cap, and recorded regardless (INTERFACES
        // §3: "the selected pair set is written to the log").
        let pairs = top_k_pairs(&claims);
        ctx.events.emit(
            EventType::CandidatesSelected,
            &self.name(),
            serde_json::json!({"pairs": pairs.len(), "tier": "t1"}),
        );

        let mut uf = UnionFind::new(n);

        let batches = partition_into_batches(n, &pairs, self.max_claims_per_batch);

        if batches.len() == 1 {
            let items: Vec<(usize, String)> = batches[0]
                .iter()
                .map(|&i| (i, claims[i].text.clone()))
                .collect();
            let base: Vec<usize> = batches[0].clone();
            self.group_and_union(&items, &base, &mut uf, ctx, "claims.group")
                .await;
        } else {
            // One grouping call per batch.
            for (batch_no, batch) in batches.iter().enumerate() {
                let items: Vec<(usize, String)> =
                    batch.iter().map(|&i| (i, claims[i].text.clone())).collect();
                self.group_and_union(
                    &items,
                    batch,
                    &mut uf,
                    ctx,
                    &format!("claims.group.batch{batch_no}"),
                )
                .await;
            }

            // Stitch: one representative per current root, one more grouping
            // call to catch cross-batch synonymy (INTERFACES §3 step 4).
            let mut roots: Vec<usize> = (0..n).map(|i| uf.find(i)).collect();
            roots.sort_unstable();
            roots.dedup();

            if roots.len() > 1 {
                if roots.len() <= self.max_claims_per_batch {
                    let items: Vec<(usize, String)> =
                        roots.iter().map(|&r| (r, claims[r].text.clone())).collect();
                    self.group_and_union(&items, &roots, &mut uf, ctx, "claims.group.stitch")
                        .await;
                } else {
                    // "far past any realistic debate" (INTERFACES §3) -- T1
                    // partitioning already grouped what it could; recursion
                    // beyond one stitch level is not implemented (D33).
                    ctx.events.emit(
                        EventType::CandidatesSelected,
                        &self.name(),
                        serde_json::json!({"tier": "stitch_depth_exceeded", "representatives": roots.len()}),
                    );
                }
            }
        }

        let merged = merge_claims(claims, &mut uf);
        ctx.events.emit(
            EventType::StageCompleted,
            &self.name(),
            serde_json::json!({"claims": merged.len()}),
        );

        Ok(NormalizedClaims(merged))
    }
}

fn parse_local_ref(r: &str, n: usize) -> Option<usize> {
    let idx: usize = r.strip_prefix('#')?.parse().ok()?;
    if idx == 0 || idx > n {
        return None;
    }
    Some(idx - 1)
}

// ---------------------------------------------------------------------------
// Merging
// ---------------------------------------------------------------------------

fn kind_rank(kind: EvidenceKind) -> u8 {
    match kind {
        EvidenceKind::Fact => 0,
        EvidenceKind::Inference => 1,
        EvidenceKind::Assumption => 2,
        EvidenceKind::Opinion => 3,
        EvidenceKind::Unverified => 4,
    }
}

/// Groups `claims` by `uf`'s final connected components and merges each
/// group into one [`CanonicalClaim`]. The surviving id is the
/// lexicographically smallest id in the group (deterministic, independent of
/// processing order); `kind` is the strongest (lowest [`kind_rank`]) among
/// the group's members, since corroboration should never make a claim look
/// *less* evidenced than its best-supported member alone would — neither
/// spec file states a merge rule for a mixed-kind group, so this is the
/// conservative-in-the-favourable-direction choice (PLAN_DEVIATIONS.md D33).
fn merge_claims(claims: Vec<CanonicalClaim>, uf: &mut UnionFind) -> Vec<CanonicalClaim> {
    let n = claims.len();
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        groups.entry(uf.find(i)).or_default().push(i);
    }

    let mut claims: Vec<Option<CanonicalClaim>> = claims.into_iter().map(Some).collect();
    let mut merged = Vec::with_capacity(groups.len());

    for members_idx in groups.values() {
        let mut group_claims: Vec<CanonicalClaim> = members_idx
            .iter()
            .filter_map(|&i| claims[i].take())
            .collect();
        if group_claims.is_empty() {
            continue;
        }
        group_claims.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        let survivor_id = group_claims[0].id.clone();
        let survivor_text = group_claims[0].text.clone();
        let kind = group_claims
            .iter()
            .map(|c| c.kind)
            .min_by_key(|&k| kind_rank(k))
            .unwrap_or(EvidenceKind::Unverified);
        let all_members = group_claims.into_iter().flat_map(|c| c.members).collect();

        merged.push(CanonicalClaim {
            id: survivor_id,
            text: survivor_text,
            kind,
            lifecycle: ClaimLifecycle::Proposed,
            members: all_members,
        });
    }

    merged.sort_by(|a: &CanonicalClaim, b: &CanonicalClaim| a.id.as_str().cmp(b.id.as_str()));
    merged
}

// ---------------------------------------------------------------------------
// Union-find
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]);
        }
        self.parent[x]
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

// ---------------------------------------------------------------------------
// T1: cheap lexical candidate pairs
// ---------------------------------------------------------------------------

/// INTERFACES §3's K-scaling formula, transcribed verbatim: `clamp(ceil(3.0 *
/// log2(n + 1)), 8, 24)`. The worked example table in that section (`n=12 ->
/// 11`, `n=32 -> 16`, ...) does not reproduce exactly under this literal
/// reading with straightforward rounding — plausibly a documentation
/// rounding inconsistency in the worked examples rather than in the formula
/// itself, which is given as an actual expression, not just examples. The
/// formula is transcribed as written rather than reverse-engineered from the
/// table (PLAN_DEVIATIONS.md D33).
fn top_k(n: usize) -> usize {
    let k = (3.0 * ((n as f64 + 1.0).log2())).ceil() as i64;
    k.clamp(8, 24) as usize
}

const MAX_CANDIDATE_PAIRS: usize = 2000;

/// Character-trigram term-frequency vector for one claim's (lowercased) text.
fn trigram_tf(text: &str) -> BTreeMap<String, u32> {
    let normalized = text.to_lowercase();
    let chars: Vec<char> = normalized.chars().collect();
    let mut tf = BTreeMap::new();
    if chars.len() < 3 {
        return tf;
    }
    for w in chars.windows(3) {
        *tf.entry(w.iter().collect::<String>()).or_insert(0) += 1;
    }
    tf
}

/// Top-K lexical candidate pairs per claim (INTERFACES §3 T1: "normalise ->
/// trigrams -> ... IDF-weighted cosine -> top-K per claim"), deduplicated
/// into an undirected pair list and capped globally at
/// `MAX_CANDIDATE_PAIRS`. SimHash blocking (the spec's own scalability
/// optimization ahead of the cosine step) is not implemented — this computes
/// cosine directly over every pair, which is correct at the claim counts a
/// debate produces before F2's fixture suite exists to stress it, and
/// blocking is purely a performance optimization, never a correctness
/// requirement (PLAN_DEVIATIONS.md D33).
fn top_k_pairs(claims: &[CanonicalClaim]) -> Vec<(usize, usize)> {
    let n = claims.len();
    let tfs: Vec<BTreeMap<String, u32>> = claims.iter().map(|c| trigram_tf(&c.text)).collect();

    let mut df: BTreeMap<&str, u32> = BTreeMap::new();
    for tf in &tfs {
        for term in tf.keys() {
            *df.entry(term.as_str()).or_insert(0) += 1;
        }
    }
    let idf = |term: &str| -> f64 {
        ((1.0 + n as f64) / (1.0 + *df.get(term).unwrap_or(&0) as f64)).ln() + 1.0
    };

    let vectors: Vec<BTreeMap<&str, f64>> = tfs
        .iter()
        .map(|tf| {
            tf.iter()
                .map(|(term, &count)| (term.as_str(), count as f64 * idf(term)))
                .collect()
        })
        .collect();

    let cosine = |a: &BTreeMap<&str, f64>, b: &BTreeMap<&str, f64>| -> f64 {
        let mut dot = 0.0;
        for (term, &wa) in a {
            if let Some(&wb) = b.get(term) {
                dot += wa * wb;
            }
        }
        let norm_a = a.values().map(|v| v * v).sum::<f64>().sqrt();
        let norm_b = b.values().map(|v| v * v).sum::<f64>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            0.0
        } else {
            dot / (norm_a * norm_b)
        }
    };

    let k = top_k(n);
    let mut pair_set: BTreeSet<(usize, usize)> = BTreeSet::new();
    for i in 0..n {
        let mut scored: Vec<(usize, f64)> = (0..n)
            .filter(|&j| j != i)
            .map(|j| (j, cosine(&vectors[i], &vectors[j])))
            .filter(|&(_, score)| score > 0.0)
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for &(j, _) in scored.iter().take(k) {
            let pair = if i < j { (i, j) } else { (j, i) };
            pair_set.insert(pair);
            if pair_set.len() >= MAX_CANDIDATE_PAIRS {
                break;
            }
        }
        if pair_set.len() >= MAX_CANDIDATE_PAIRS {
            break;
        }
    }
    pair_set.into_iter().collect()
}

/// Connected components over the T1 candidate graph, first-fit-decreasing
/// packed into batches of at most `max_batch` claims (INTERFACES §3's
/// partition-then-pack step). A single component larger than `max_batch` is
/// simply placed in its own oversized batch rather than split further — a
/// batch that large means T1 already found everything densely
/// interconnected, which the spec's own token-budget concern (truncation) is
/// a real risk for for that batch specifically, but splitting a genuinely
/// connected component would defeat the point of partitioning by it in the
/// first place.
fn partition_into_batches(n: usize, pairs: &[(usize, usize)], max_batch: usize) -> Vec<Vec<usize>> {
    if n <= max_batch {
        return vec![(0..n).collect()];
    }

    let mut uf = UnionFind::new(n);
    for &(a, b) in pairs {
        uf.union(a, b);
    }
    let mut components: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        components.entry(uf.find(i)).or_default().push(i);
    }
    let mut components: Vec<Vec<usize>> = components.into_values().collect();
    components.sort_by_key(|b| std::cmp::Reverse(b.len()));

    let mut batches: Vec<Vec<usize>> = Vec::new();
    for component in components {
        if let Some(batch) = batches
            .iter_mut()
            .find(|b| b.len() + component.len() <= max_batch)
        {
            batch.extend(component);
        } else {
            batches.push(component);
        }
    }
    batches
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::BudgetLedger;
    use crate::cache::ResponseCache;
    use crate::provider::{Provider, ProviderCapabilities, ProviderError, ProviderResponse};
    use crate::stage::{CancellationToken, DeterministicRng, EventSink, ProviderRegistry};
    use arbiter_core::{ClaimId, Grounding, PositionId};
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;

    // ---- pure-function tests -------------------------------------------------

    #[test]
    fn top_k_is_clamped_and_monotonic() {
        assert_eq!(top_k(0), 8);
        assert!(top_k(12) >= 8 && top_k(12) <= 24);
        assert_eq!(top_k(10_000), 24);
        assert!(top_k(300) <= top_k(3_000_000));
    }

    #[test]
    fn union_find_merges_transitively() {
        let mut uf = UnionFind::new(4);
        uf.union(0, 1);
        uf.union(1, 2);
        assert_eq!(uf.find(0), uf.find(2));
        assert_ne!(uf.find(0), uf.find(3));
    }

    #[test]
    fn partition_into_batches_keeps_everything_under_the_cap_when_it_fits() {
        let batches = partition_into_batches(10, &[], 60);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 10);
    }

    #[test]
    fn partition_into_batches_packs_connected_components_together() {
        // 5 claims, a 3-node connected component {0,1,2} and two singletons
        // {3},{4}; cap of 2 forces the component into its own oversized
        // batch rather than being split.
        let pairs = vec![(0, 1), (1, 2)];
        let batches = partition_into_batches(5, &pairs, 2);
        let component_batch = batches.iter().find(|b| b.contains(&0)).unwrap();
        assert!(component_batch.contains(&1) && component_batch.contains(&2));
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

    fn group_template() -> PromptTemplate {
        PromptTemplate {
            stage: StageName::new("claims.group"),
            body: "{{claims}}".to_string(),
            variables: crate::prompt::VariableSchema::new(["claims"]),
        }
    }

    fn claim(id: &str, text: &str, kind: EvidenceKind, model: &str) -> CanonicalClaim {
        let member = arbiter_core::ClaimMember::new(
            ClaimId::new(id),
            ModelId::new(model),
            ProviderId::new("mock"),
            PositionId::new(format!("pos_{model}")),
            text,
            Grounding::DirectQuote {
                span: arbiter_core::TextSpan {
                    start: 0,
                    end: text.len(),
                    quote: text.to_string(),
                },
            },
        );
        CanonicalClaim {
            id: ClaimId::new(id),
            text: text.to_string(),
            kind,
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

    fn stage() -> ClaimsNormalize {
        ClaimsNormalize::new(
            group_template(),
            (ModelId::new("model-a"), ProviderId::new("mock")),
            Cost(0.01),
        )
    }

    #[tokio::test]
    async fn a_high_confidence_group_merges_two_claims() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([
            {"members": ["#1", "#2"], "confidence": 0.95}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let claims = ExtractedClaims(vec![
            claim(
                "claim_a",
                "we have 8 developers",
                EvidenceKind::Fact,
                "model-a",
            ),
            claim(
                "claim_b",
                "our team has 8 devs",
                EvidenceKind::Fact,
                "model-b",
            ),
        ]);
        let out = stage().run(claims, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 1);
        assert_eq!(out.0[0].members.len(), 2);
        assert_eq!(out.0[0].id.as_str(), "claim_a");
    }

    #[tokio::test]
    async fn a_low_confidence_group_is_not_merged() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([
            {"members": ["#1", "#2"], "confidence": 0.4}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let claims = ExtractedClaims(vec![
            claim(
                "claim_a",
                "microservices add overhead",
                EvidenceKind::Fact,
                "model-a",
            ),
            claim(
                "claim_b",
                "the sky is blue today",
                EvidenceKind::Fact,
                "model-b",
            ),
        ]);
        let out = stage().run(claims, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 2, "a below-threshold group must stay split");
    }

    #[tokio::test]
    async fn a_single_claim_never_calls_the_provider() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        // No response scripted -- a call here would fail the test via the
        // script-exhausted error surfacing as a lost claim.
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let claims = ExtractedClaims(vec![claim(
            "claim_a",
            "we have 8 developers",
            EvidenceKind::Fact,
            "model-a",
        )]);
        let out = stage().run(claims, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 1);
        assert_eq!(sink.count(EventType::CallStarted), 0);
    }

    #[tokio::test]
    async fn an_unparseable_grouping_response_leaves_every_claim_singleton() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_text("not json");
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let claims = ExtractedClaims(vec![
            claim("claim_a", "a", EvidenceKind::Fact, "model-a"),
            claim("claim_b", "b", EvidenceKind::Fact, "model-b"),
        ]);
        let out = stage().run(claims, &ctx).await.unwrap();
        assert_eq!(out.0.len(), 2);
    }

    #[tokio::test]
    async fn a_merged_claim_keeps_the_strongest_member_kind() {
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([
            {"members": ["#1", "#2"], "confidence": 0.9}
        ]));
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let claims = ExtractedClaims(vec![
            claim(
                "claim_a",
                "we have 8 developers",
                EvidenceKind::Unverified,
                "model-a",
            ),
            claim(
                "claim_b",
                "our team has 8 devs",
                EvidenceKind::Fact,
                "model-b",
            ),
        ]);
        let out = stage().run(claims, &ctx).await.unwrap();

        assert_eq!(out.0.len(), 1);
        assert_eq!(out.0[0].kind, EvidenceKind::Fact);
    }

    #[tokio::test]
    async fn multi_batch_grouping_stitches_across_batches() {
        // 4 claims force two batches of 2 (max_claims_per_batch overridden
        // to 2 below); each batch merges its own pair, then a stitch call
        // merges the two resulting representatives together.
        let mock = ScriptedProvider::new(ProviderId::new("mock"));
        mock.script_json(serde_json::json!([{"members": ["#1", "#2"], "confidence": 0.9}])); // batch 0
        mock.script_json(serde_json::json!([{"members": ["#1", "#2"], "confidence": 0.9}])); // batch 1
        mock.script_json(serde_json::json!([{"members": ["#1", "#2"], "confidence": 0.9}])); // stitch
        let mut registry = ProviderRegistry::default();
        registry.register(Box::new(mock));
        let budget = BudgetLedger::unbounded();
        let cache = ResponseCache::new();
        let sink = RecordingSink::default();
        let ctx = stage_ctx(&registry, &budget, &cache, &sink);

        let mut normalize = stage();
        normalize.max_claims_per_batch = 2;

        // Two pairs built from disjoint alphabets ("alpha ..." vs "zzzz
        // ...") so T1's cosine similarity is exactly zero across the pair
        // boundary and nonzero within each pair -- this deterministically
        // partitions into two batches of two, rather than leaving it to
        // chance whether ordinary English claim text happens to share a
        // trigram across the intended boundary.
        let claims = ExtractedClaims(vec![
            claim(
                "claim_a",
                "alpha alpha alpha",
                EvidenceKind::Fact,
                "model-a",
            ),
            claim(
                "claim_b",
                "alpha alpha alpha",
                EvidenceKind::Fact,
                "model-b",
            ),
            claim("claim_c", "zzzz zzzz zzzz", EvidenceKind::Fact, "model-c"),
            claim("claim_d", "zzzz zzzz zzzz", EvidenceKind::Fact, "model-d"),
        ]);
        let out = normalize.run(claims, &ctx).await.unwrap();

        assert_eq!(
            out.0.len(),
            1,
            "the stitch pass must merge both batch groups into one"
        );
        assert_eq!(out.0[0].members.len(), 4);
    }

    #[test]
    fn the_shipped_group_prompt_loads_and_renders() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../prompts/default/v1");
        let pack = crate::prompt::PromptPack::load(&dir).unwrap();
        let template = pack.template(&StageName::new("claims.group")).unwrap();

        let mut vars = BTreeMap::new();
        vars.insert("claims".to_string(), "#1 some claim".to_string());
        assert!(template.render(&vars).unwrap().contains("#1 some claim"));
    }
}
