//! `--panel mock`'s provider (PLAN_DEVIATIONS.md D42). `arbiter-providers`'
//! `MockProvider` is a hand-scripted `VecDeque` — correct for a fixture test
//! that already knows its exact call sequence, useless for a CLI command
//! that must run the whole 13-stage pipeline end-to-end without knowing in
//! advance how many candidate pairs T1 finds or how many rounds the
//! controller runs. `SyntheticProvider` instead inspects the *rendered*
//! prompt text of each call (which already contains the real interpolated
//! claim/position text, since rendering happens before the provider ever
//! sees it) and returns a plausible, schema-correct response by matching on
//! literal text this session's own shipped `prompts/default/v1/*.md`
//! templates are known to contain.

use arbiter_core::ProviderId;
use arbiter_kernel::provider::{
    Provider, ProviderCapabilities, ProviderError, ProviderRequest, ProviderResponse,
};
use std::future::Future;
use std::pin::Pin;

#[derive(Debug)]
pub struct SyntheticProvider {
    id: ProviderId,
}

impl SyntheticProvider {
    pub fn new(id: ProviderId) -> Self {
        Self { id }
    }
}

fn ok(text: impl Into<String>) -> Result<ProviderResponse, ProviderError> {
    Ok(ProviderResponse {
        text: text.into(),
        prompt_tokens: 0,
        completion_tokens: 0,
        request_id: None,
    })
}

/// One position's fixed synthetic text — always contains the literal
/// substring [`QUOTE`], so claims.extract's grounding check (an exact/fuzzy
/// substring match against the position text a claim was supposedly quoted
/// from) always succeeds without needing to parse anything out of the
/// rendered claims.extract prompt itself.
pub const QUOTE: &str = "reduces operational risk";

pub fn synthetic_position_text(model: &str) -> String {
    format!(
        "Reasoning: {model} finds this approach {QUOTE} and improves team velocity.\n\n\
         Recommendation: adopt the proposed approach."
    )
}

/// Counts `POSITION <letter>` occurrences in a judge dossier block —
/// `judge_evaluate.rs`'s own dossier renderer emits exactly one per
/// position, each with a distinct single-letter pseudonym.
fn dossier_pseudonyms(prompt: &str) -> Vec<char> {
    let mut found = Vec::new();
    let bytes = prompt.as_bytes();
    let marker = b"POSITION ";
    let mut i = 0;
    while i + marker.len() < bytes.len() {
        if &bytes[i..i + marker.len()] == marker {
            let c = bytes[i + marker.len()] as char;
            if c.is_ascii_uppercase() && !found.contains(&c) {
                found.push(c);
            }
        }
        i += 1;
    }
    found
}

impl Provider for SyntheticProvider {
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
        request: ProviderRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send + '_>> {
        let prompt = request.prompt;
        let model = request.model.as_str().to_string();
        Box::pin(async move {
            // positions.generate
            if prompt.contains("one independent panelist") {
                return ok(synthetic_position_text(&model));
            }

            // claims.repair -- checked before claims.extract, since both
            // mention "grounding" but only repair mentions this phrase.
            if prompt.contains("could not be automatically grounded") {
                return ok(serde_json::json!([
                    {"index": "#1", "kind": "fact", "grounding": {"quote": QUOTE}}
                ])
                .to_string());
            }

            // claims.extract
            if prompt.contains("Extract the individual factual and inferential claims") {
                return ok(serde_json::json!([
                    {"text": format!("the approach {QUOTE}"), "kind": "fact",
                     "grounding": {"quote": QUOTE}}
                ])
                .to_string());
            }

            // claims.normalize (grouping and its stitch pass share this template)
            if prompt.contains("numbered list of claims extracted from a multi-model debate") {
                return ok("[]");
            }

            // options.cluster
            if prompt.contains("Identify each position's core recommendation") {
                return ok("[]");
            }

            // options.attach
            if prompt.contains("does the claim support that option being chosen") {
                return ok("[]");
            }

            // relations.classify -- mark the first pair a contradiction so the
            // round loop has at least one real dispute to work through; the
            // rest are left unclassified (omission is a valid response,
            // relations_analyze.rs's own D35).
            if prompt.contains("For each pair, classify how claim A relates to claim B") {
                if prompt.contains("Pair #1:") {
                    return ok(serde_json::json!([
                        {"pair": "#1", "kind": "contradicts", "from": "A", "to": "B", "confidence": 0.75}
                    ]).to_string());
                }
                return ok("[]");
            }

            // challenge.issue
            if prompt.contains("YOUR OBJECTION:") {
                return ok("This claim overlooks a significant risk the objection raises.");
            }

            // rebuttal.respond -- always defend: simplest deterministic choice,
            // and at --depth standard there is only ever one round for it to
            // matter in anyway.
            if prompt.contains("You previously asserted the following claim") {
                return ok(serde_json::json!({
                    "outcome": "defend",
                    "rebuttal_text": "The original claim stands; the challenge does not undermine it."
                }).to_string());
            }

            // judge.evaluate
            if prompt.contains("You are judging a multi-model debate") {
                let scores: Vec<serde_json::Value> = dossier_pseudonyms(&prompt)
                    .into_iter()
                    .map(|p| {
                        serde_json::json!({
                            "pseudonym": p.to_string(),
                            "factual_correctness": 0.75, "logical_reasoning": 0.75,
                            "evidence_quality": 0.75, "problem_relevance": 0.75,
                            "assumption_quality": 0.75, "counterargument_handling": 0.75,
                            "risk_awareness": 0.75, "practicality": 0.75, "clarity": 0.75,
                        })
                    })
                    .collect();
                return ok(serde_json::to_string(&scores).unwrap());
            }

            Err(ProviderError::Other(format!(
                "SyntheticProvider: no synthetic response known for this prompt (first 120 chars): {}",
                &prompt.chars().take(120).collect::<String>()
            )))
        })
    }
}
