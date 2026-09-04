//! Anthropic Messages API adapter (P4).
//!
//! Capabilities are declared from Anthropic's own documentation, not assumed
//! (ARCHITECTURE §7): `idempotency: None`, which the `Provider` trait's own
//! doc comment already anticipated — "the reference Messages API documents no
//! idempotency header at the time of writing". That `None` is load-bearing:
//! it is what stops the kernel from retrying a call it cannot prove was
//! unbilled, sending it to `ORPHANED` instead (ARCHITECTURE §8.4).

use crate::http;
use crate::keys::SecretString;
use arbiter_core::{ModelId, ProviderId};
use arbiter_kernel::provider::{
    IdempotencyStyle, Provider, ProviderCapabilities, ProviderError, ProviderRequest,
    ProviderResponse,
};
use std::future::Future;
use std::pin::Pin;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u64 = 4096;

pub struct AnthropicProvider {
    api_key: SecretString,
    client: reqwest::Client,
    base_url: String,
}

impl std::fmt::Debug for AnthropicProvider {
    /// Hand-written so the key can never reach a log through a derived
    /// `Debug` three layers up — the exact failure `SecretString` exists to
    /// prevent (ARCHITECTURE §11.1).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    pub fn new(api_key: SecretString) -> Result<Self, ProviderError> {
        Ok(Self {
            api_key,
            client: http::client()?,
            base_url: API_URL.to_string(),
        })
    }

    /// Points the adapter at a different base URL. Exists for the tests in
    /// this file, which serve recorded responses from a local socket rather
    /// than reaching Anthropic — CI has no key and must open no sockets to
    /// the outside world.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Split out from `call` so the wire shape is unit-testable without a
    /// socket: this is the whole request body, given a request.
    fn body(request: &ProviderRequest) -> serde_json::Value {
        let max_tokens = serde_json::from_str::<serde_json::Value>(&request.params)
            .ok()
            .and_then(|p| p.get("max_tokens").and_then(|v| v.as_u64()))
            .unwrap_or(DEFAULT_MAX_TOKENS);
        serde_json::json!({
            "model": request.model.as_str(),
            "max_tokens": max_tokens,
            "messages": [{ "role": "user", "content": request.prompt }],
        })
    }

    /// The response half, likewise split out and tested against a recorded
    /// body. Text comes from the first `text` content block; Anthropic can
    /// return several blocks, and the ones this engine's prompts produce are
    /// single-block text.
    fn parse(
        body: &serde_json::Value,
        request_id: Option<String>,
    ) -> Result<ProviderResponse, ProviderError> {
        let text = http::required(body, "/content/0/text", "anthropic")?
            .as_str()
            .ok_or_else(|| {
                ProviderError::Other("anthropic: content[0].text is not a string".into())
            })?
            .to_string();
        Ok(ProviderResponse {
            text,
            prompt_tokens: http::optional_u64(body, "/usage/input_tokens"),
            completion_tokens: http::optional_u64(body, "/usage/output_tokens"),
            request_id,
        })
    }
}

impl Provider for AnthropicProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new("anthropic")
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            structured_output: true,
            streaming: true,
            // Documented as unsupported; see the module comment.
            idempotency: None,
        }
    }

    fn call(
        &self,
        request: ProviderRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send + '_>> {
        Box::pin(async move {
            let response = self
                .client
                .post(&self.base_url)
                .header("x-api-key", self.api_key.expose())
                .header("anthropic-version", API_VERSION)
                .json(&Self::body(&request))
                .send()
                .await
                .map_err(|e| http::transport_error("anthropic", e))?;

            let request_id = http::request_id_of(&response);
            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|e| ProviderError::Other(format!("anthropic: reading body: {e}")))?;
            if !status.is_success() {
                return Err(http::status_error("anthropic", status, &text));
            }
            let body: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                ProviderError::Other(format!("anthropic: response was not JSON: {e}"))
            })?;
            Self::parse(&body, request_id)
        })
    }
}

/// Model ids move; this is only the default when a panel names the provider
/// without a model. Callers may pass any id through `ProviderRequest::model`.
pub fn default_model() -> ModelId {
    ModelId::new("claude-sonnet-4-5")
}

#[allow(dead_code)]
fn assert_idempotency_style_is_reachable() -> IdempotencyStyle {
    // Referenced so the import stays honest if the capability is ever revised.
    IdempotencyStyle::Header("Idempotency-Key".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arbiter_kernel::ids::ReservationId;

    fn request() -> ProviderRequest {
        ProviderRequest {
            model: ModelId::new("claude-sonnet-4-5"),
            prompt: "Say hello".to_string(),
            params: "{}".to_string(),
            idempotency_key: None,
            reservation: ReservationId::new("r1"),
        }
    }

    #[test]
    fn body_carries_model_prompt_and_a_max_tokens() {
        let body = AnthropicProvider::body(&request());
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "Say hello");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn max_tokens_comes_from_params_when_the_caller_sets_one() {
        let mut r = request();
        r.params = r#"{"max_tokens": 64}"#.to_string();
        assert_eq!(AnthropicProvider::body(&r)["max_tokens"], 64);
    }

    #[test]
    fn parse_reads_text_and_both_token_counts() {
        // A recorded Messages API response shape.
        let body = serde_json::json!({
            "id": "msg_01XYZ",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": "Hello there." }],
            "usage": { "input_tokens": 9, "output_tokens": 4 }
        });
        let parsed = AnthropicProvider::parse(&body, Some("req_1".into())).unwrap();
        assert_eq!(parsed.text, "Hello there.");
        assert_eq!(parsed.prompt_tokens, 9);
        assert_eq!(parsed.completion_tokens, 4);
        assert_eq!(parsed.request_id.as_deref(), Some("req_1"));
    }

    #[test]
    fn parse_fails_loudly_when_the_content_block_is_missing() {
        let body = serde_json::json!({ "usage": { "input_tokens": 1 } });
        let err = AnthropicProvider::parse(&body, None).unwrap_err();
        assert!(err.to_string().contains("/content/0/text"), "{err}");
    }

    #[test]
    fn missing_usage_degrades_to_zero_rather_than_failing_the_call() {
        let body = serde_json::json!({ "content": [{ "type": "text", "text": "hi" }] });
        let parsed = AnthropicProvider::parse(&body, None).unwrap();
        assert_eq!(parsed.prompt_tokens, 0);
        assert_eq!(parsed.completion_tokens, 0);
        assert_eq!(parsed.text, "hi");
    }

    #[test]
    fn declares_no_idempotency_support() {
        let p = AnthropicProvider::new(SecretString::new("k")).unwrap();
        assert_eq!(p.capabilities().idempotency, None);
        assert_eq!(p.id().as_str(), "anthropic");
    }

    #[test]
    fn debug_never_reveals_the_key() {
        let p = AnthropicProvider::new(SecretString::new("sk-ant-secret-value")).unwrap();
        let rendered = format!("{p:?}");
        assert!(!rendered.contains("sk-ant-secret-value"), "{rendered}");
    }
}
