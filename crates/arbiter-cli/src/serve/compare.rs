//! `POST /api/compare` — one prompt, every keyed provider, side by side.
//!
//! This is the sixth screen, and the one thing Arbiter could not do before
//! P4: ask several vendors the same question and lay the answers next to each
//! other. It replaces the standalone Node app that used to live in
//! `tools/multiplex/`, so there is one binary, one UI and one launcher rather
//! than two applications sharing a purpose and nothing else.
//!
//! Three things are deliberately unlike the debate pipeline it sits beside:
//!
//! - **A missing key is a skip, not an error.** `panel::resolve` refuses a
//!   panel it cannot run in full, because ARCHITECTURE §6.2 computes
//!   independence over the panel that actually ran. Compare makes no such
//!   claim — it asserts nothing about the answers beyond "here is what each
//!   one said" — so a keyless provider reports `model-skipped` and the rest
//!   proceed, which is also what an operator asking "who can answer this?"
//!   means by the question.
//! - **Nothing is stored.** No `RunId`, no event chain, no artifacts. A
//!   comparison is not a decision, and writing one into the run store would
//!   put something in `arbiter history` that no `explain` can account for.
//! - **No budget ledger.** §8.3's reservation protocol exists so a *run* can
//!   be resumed and reconciled after a crash; a single unstored call has
//!   nothing to resume. Cost is reported per answer from the published list
//!   prices instead, clearly as an estimate.
//!
//! Answers arrive per model rather than per token: `Provider::call` returns
//! one finished [`ProviderResponse`], so a card fills in when its model
//! finishes. The fastest model still lands first, which is the part of a
//! side-by-side race worth watching.

use arbiter_core::{ModelId, ProviderId};
use arbiter_kernel::provider::ProviderRequest;
use arbiter_providers::keys::{CredentialSource, EnvCredentialSource, KeychainCredentialSource};
use arbiter_providers::{build_provider, default_model_for, pricing};
use axum::Json;
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::time::Instant;

/// The longest a single comparison answer is allowed to take before the card
/// gives up on it. Shorter than the adapters' own 120s client timeout so a
/// wedged vendor shows as one failed card rather than holding the whole
/// screen at "thinking" for two minutes.
const ANSWER_TIMEOUT_SECS: u64 = 90;

/// Answers are capped rather than unbounded: this screen exists to *compare*
/// replies at a glance, and a model that returns an essay makes every card
/// beside it unreadable. The debate pipeline, which actually reasons over the
/// text, sets its own limits from the prompt pack and is unaffected.
const MAX_ANSWER_TOKENS: u32 = 1024;

#[derive(Debug, Deserialize, Default)]
pub(crate) struct CompareBody {
    #[serde(default)]
    prompt: String,
    /// Which providers to ask. Absent or empty means "every one that has a
    /// key", which is what the screen sends on a plain submit.
    #[serde(default)]
    providers: Vec<String>,
}

/// One provider's slot in a comparison, resolved but not yet called.
struct Contender {
    provider: ProviderId,
    model: ModelId,
}

/// `POST /api/compare` — **spends money**, like `POST /api/runs`. Streams
/// `model-skipped` / `model-start` / `model-done` / `model-error`, then a
/// single `run-done`, using the same SSE carrier the run event stream uses so
/// the page has one reconnect story rather than two.
pub(crate) async fn compare(Json(body): Json<CompareBody>) -> Response {
    let prompt = body.prompt.trim().to_string();
    if prompt.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "prompt is required"})),
        )
            .into_response();
    }

    let requested: Vec<ProviderId> = if body.providers.is_empty() {
        arbiter_providers::REAL_PROVIDER_IDS
            .iter()
            .map(|id| ProviderId::new(*id))
            .collect()
    } else {
        body.providers
            .iter()
            .map(|id| ProviderId::new(id.trim()))
            .collect()
    };

    let env = EnvCredentialSource;
    let keychain = KeychainCredentialSource;
    let sources: [&dyn CredentialSource; 2] = [&env, &keychain];

    // Resolve every credential up front, on this task, so the stream can open
    // with the full list of who is being skipped and why. A card that appears
    // late saying "no key" reads as a failure; one that is greyed out from
    // the first frame reads as a setting.
    let mut ready: Vec<(Contender, Box<dyn arbiter_kernel::provider::Provider>)> = Vec::new();
    let mut skipped: Vec<(ProviderId, String)> = Vec::new();

    for provider in requested {
        let Some(model) = default_model_for(&provider) else {
            skipped.push((provider, "not a provider this build can reach".to_string()));
            continue;
        };
        let Some((secret, _source)) = sources.iter().find_map(|s| s.resolve(&provider)) else {
            skipped.push((provider, "no key configured".to_string()));
            continue;
        };
        match build_provider(&provider, secret) {
            Ok(adapter) => ready.push((Contender { provider, model }, adapter)),
            Err(e) => skipped.push((provider, e.to_string())),
        }
    }

    // One message per model, plus the skips and the terminator. The channel is
    // sized to hold all of them, so no producer ever blocks on a slow reader.
    let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(ready.len() + skipped.len() + 2);

    for (provider, reason) in skipped {
        let _ = tx
            .send(named_event(
                "model-skipped",
                serde_json::json!({"model": provider.as_str(), "reason": reason}),
            ))
            .await;
    }

    for (contender, adapter) in ready {
        let tx = tx.clone();
        let prompt = prompt.clone();
        tokio::spawn(async move {
            let Contender { provider, model } = contender;
            let _ = tx
                .send(named_event(
                    "model-start",
                    serde_json::json!({"model": provider.as_str(), "model_id": model.as_str()}),
                ))
                .await;

            let request = ProviderRequest {
                model: model.clone(),
                prompt,
                params: serde_json::json!({"max_tokens": MAX_ANSWER_TOKENS}).to_string(),
                idempotency_key: None,
                // Compare keeps no ledger, but `ProviderRequest` carries a
                // reservation id for the stages that do. A comparison's id
                // names itself so it can never be mistaken for a real
                // reservation in a log the two share.
                reservation: arbiter_kernel::ids::ReservationId::new(format!(
                    "compare_{}",
                    provider.as_str()
                )),
            };

            let started = Instant::now();
            let outcome = tokio::time::timeout(
                std::time::Duration::from_secs(ANSWER_TIMEOUT_SECS),
                adapter.call(request),
            )
            .await;
            let elapsed_ms = started.elapsed().as_millis() as u64;

            let event = match outcome {
                Ok(Ok(response)) => {
                    let cost = pricing::pricing_for(&provider)
                        .map(|p| p.cost_of(response.prompt_tokens, response.completion_tokens));
                    named_event(
                        "model-done",
                        serde_json::json!({
                            "model": provider.as_str(),
                            "model_id": model.as_str(),
                            "text": response.text,
                            "prompt_tokens": response.prompt_tokens,
                            "completion_tokens": response.completion_tokens,
                            "total_tokens": response.prompt_tokens + response.completion_tokens,
                            // `null`, not 0, when this build has no published
                            // price for the provider — a blank cell says "we
                            // don't know", a zero says "it was free".
                            "cost_usd": cost,
                            "elapsed_ms": elapsed_ms,
                        }),
                    )
                }
                Ok(Err(e)) => named_event(
                    "model-error",
                    serde_json::json!({
                        "model": provider.as_str(),
                        "error": e.to_string(),
                        "elapsed_ms": elapsed_ms,
                    }),
                ),
                Err(_) => named_event(
                    "model-error",
                    serde_json::json!({
                        "model": provider.as_str(),
                        "error": format!("no answer within {ANSWER_TIMEOUT_SECS}s"),
                        "elapsed_ms": elapsed_ms,
                    }),
                ),
            };
            let _ = tx.send(event).await;
        });
    }

    // Every producer holds a clone; dropping this one means the channel closes
    // exactly when the last model settles, which is what ends the stream.
    drop(tx);

    // `done` carries the one-shot terminator: the channel closing means every
    // model settled, and `run-done` is what tells the page to stop showing
    // spinners. Folding it into the same unfold keeps the stream a single
    // concrete type, which `Sse` needs.
    let stream = futures_util::stream::unfold((rx, false), |(mut rx, done)| async move {
        if done {
            return None;
        }
        match rx.recv().await {
            Some(event) => Some((Ok::<_, std::convert::Infallible>(event), (rx, false))),
            None => Some((
                Ok(named_event("run-done", serde_json::json!({}))),
                (rx, true),
            )),
        }
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn named_event(name: &str, payload: serde_json::Value) -> SseEvent {
    SseEvent::default().event(name).data(payload.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_of(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[tokio::test]
    async fn an_empty_prompt_is_refused_before_anything_is_spent() {
        let response = compare(Json(CompareBody {
            prompt: "   ".to_string(),
            providers: vec![],
        }))
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body_of(response).await.contains("prompt is required"));
    }

    /// No credential is configured in the test environment, so every provider
    /// must report itself skipped and the stream must still terminate — a
    /// comparison with nothing to compare ends, it does not hang.
    #[tokio::test]
    async fn every_keyless_provider_is_skipped_and_the_stream_still_ends() {
        let response = compare(Json(CompareBody {
            prompt: "Which database should we use?".to_string(),
            providers: vec![],
        }))
        .await;
        let body = body_of(response).await;
        for id in arbiter_providers::REAL_PROVIDER_IDS {
            assert!(
                body.contains(&format!(r#""model":"{id}""#)),
                "{id} must appear in the stream: {body}"
            );
        }
        assert_eq!(
            body.matches("event: model-skipped").count(),
            arbiter_providers::REAL_PROVIDER_IDS.len(),
            "{body}"
        );
        assert!(body.contains("no key configured"), "{body}");
        assert!(body.contains("event: run-done"), "{body}");
        assert!(
            !body.contains("event: model-start"),
            "nothing should be called with no keys: {body}"
        );
    }

    #[tokio::test]
    async fn a_provider_this_build_cannot_reach_is_named_rather_than_ignored() {
        let response = compare(Json(CompareBody {
            prompt: "hello".to_string(),
            providers: vec!["bard".to_string()],
        }))
        .await;
        let body = body_of(response).await;
        assert!(body.contains(r#""model":"bard""#), "{body}");
        assert!(
            body.contains("not a provider this build can reach"),
            "{body}"
        );
        assert!(body.contains("event: run-done"), "{body}");
    }
}
