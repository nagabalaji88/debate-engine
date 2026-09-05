//! Does this key authenticate at all? — asked without spending anything.
//!
//! Every one of these vendors serves a model-listing endpoint that requires a
//! valid key and *nothing else*: no credit, no quota, no paid request. That
//! makes it the right first question, and it separates two things a single
//! completion call conflates.
//!
//! The distinction is not academic. An Anthropic account with an exhausted
//! balance answers `GET /v1/models` with the full list and `POST /v1/messages`
//! with `400 ... "Your credit balance is too low"`. A tool that only lists
//! reports the key as working; a tool that only completes reports it as
//! failing. Both are telling the truth about the question they asked, and an
//! operator holding one of each has no way to reconcile them.
//!
//! So verification asks both, in order: this first — free, and conclusive
//! about the key itself — and a tiny completion afterwards only if this
//! passes. A key that fails here never costs a paid request to find out.
//!
//! (The listing endpoints come from `master-prompt-generator`'s own
//! `model_discovery`, which uses exactly these URLs for the same purpose.)

use crate::http;
use crate::keys::SecretString;
use arbiter_core::ProviderId;
use std::time::Duration;

/// Listing is a cheap request against a healthy service; a slow answer here is
/// a signal in itself, and the caller still has a completion to make.
const PROBE_TIMEOUT_SECS: u64 = 20;

/// What a key-only check established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyProbe {
    /// The vendor accepted the key and listed its models. Says nothing about
    /// whether the account can afford to *use* them.
    Authenticated { models: usize },
    /// The vendor answered and refused the key itself.
    Rejected { status: u16, message: String },
    /// The vendor answered, but with something other than an auth failure —
    /// listing is not universally free of quota, and a 429 here is about the
    /// account, not the key.
    Inconclusive { status: u16, message: String },
    /// Never reached the vendor.
    Unreachable { message: String },
}

/// Where each provider lists its models, and how it wants to be authenticated.
/// `None` for a provider with no listing endpoint this build knows — the
/// caller then falls back to the completion alone rather than guessing a URL.
fn listing_request(
    client: &reqwest::Client,
    provider: &ProviderId,
    key: &SecretString,
) -> Option<reqwest::RequestBuilder> {
    Some(match provider.as_str() {
        "anthropic" => client
            .get("https://api.anthropic.com/v1/models?limit=1")
            .header("x-api-key", key.expose())
            .header("anthropic-version", "2023-06-01"),
        "openai" => client
            .get("https://api.openai.com/v1/models")
            .bearer_auth(key.expose()),
        "xai" => client
            .get("https://api.x.ai/v1/models")
            .bearer_auth(key.expose()),
        "deepseek" => client
            .get("https://api.deepseek.com/models")
            .bearer_auth(key.expose()),
        // Gemini takes the key in a header, never the query string, for the
        // same reason the adapter does: a URL lands in logs.
        "gemini" => client
            .get("https://generativelanguage.googleapis.com/v1beta/models?pageSize=1")
            .header("x-goog-api-key", key.expose()),
        _ => return None,
    })
}

/// Counts whatever the vendor called its list. Only used to say "authenticated
/// and saw N models", so an unrecognised shape reporting 0 is harmless.
fn count_models(body: &str) -> usize {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return 0;
    };
    for key in ["data", "models"] {
        if let Some(items) = value.get(key).and_then(|v| v.as_array()) {
            return items.len();
        }
    }
    0
}

/// Asks the vendor whether this key is valid, spending nothing.
///
/// `None` when the provider has no listing endpoint this build knows about.
pub async fn probe_key(provider: &ProviderId, key: &SecretString) -> Option<KeyProbe> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(PROBE_TIMEOUT_SECS))
        .build()
        .ok()?;
    let request = listing_request(&client, provider, key)?;

    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            return Some(KeyProbe::Unreachable {
                message: http::transport_error(provider.as_str(), e).to_string(),
            });
        }
    };
    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if status.is_success() {
        return Some(KeyProbe::Authenticated {
            models: count_models(&body),
        });
    }
    let message = http::message_in(&body).unwrap_or_else(|| body.trim().to_string());
    Some(if matches!(status.as_u16(), 401 | 403) {
        KeyProbe::Rejected {
            status: status.as_u16(),
            message,
        }
    } else {
        KeyProbe::Inconclusive {
            status: status.as_u16(),
            message,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_runnable_provider_has_a_listing_endpoint() {
        let client = reqwest::Client::new();
        let key = SecretString::new("k");
        for id in crate::REAL_PROVIDER_IDS {
            assert!(
                listing_request(&client, &ProviderId::new(id), &key).is_some(),
                "{id} has no free way to check its key, so a bad one would cost a paid request"
            );
        }
    }

    #[test]
    fn mock_has_no_listing_endpoint_and_is_never_probed() {
        let client = reqwest::Client::new();
        assert!(
            listing_request(&client, &ProviderId::new("mock"), &SecretString::new("k")).is_none()
        );
    }

    #[test]
    fn model_counts_read_both_common_list_shapes() {
        // OpenAI-compatible
        assert_eq!(count_models(r#"{"data":[{"id":"a"},{"id":"b"}]}"#), 2);
        // Gemini
        assert_eq!(count_models(r#"{"models":[{"name":"a"}]}"#), 1);
        // Anything else counts as zero rather than failing the probe.
        assert_eq!(count_models("not json"), 0);
        assert_eq!(count_models(r#"{"unexpected":true}"#), 0);
    }
}
