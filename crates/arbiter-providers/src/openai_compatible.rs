//! OpenAI-compatible Chat Completions adapter (P4).
//!
//! One adapter, three providers: OpenAI, xAI (Grok) and DeepSeek all publish
//! the same Chat Completions request/response shape, so they differ here only
//! by base URL, `ProviderId`, and default model. Three near-identical files
//! would drift apart the first time one of them needed a fix.
//!
//! Unlike Anthropic, this family *does* document an `Idempotency-Key` header,
//! so these declare `Some(Header("Idempotency-Key"))` and the kernel is free
//! to retry a call it keyed — the difference between a retry and an
//! `ORPHANED` call (ARCHITECTURE §8.4).

use crate::http;
use crate::keys::SecretString;
use arbiter_core::{ModelId, ProviderId};
use arbiter_kernel::provider::{
    IdempotencyStyle, Provider, ProviderCapabilities, ProviderError, ProviderRequest,
    ProviderResponse,
};
use std::future::Future;
use std::pin::Pin;

/// Which member of the family an adapter instance is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    OpenAi,
    XAi,
    DeepSeek,
}

impl Flavor {
    pub fn provider_id(self) -> ProviderId {
        ProviderId::new(match self {
            Flavor::OpenAi => "openai",
            Flavor::XAi => "xai",
            Flavor::DeepSeek => "deepseek",
        })
    }

    fn base_url(self) -> &'static str {
        match self {
            Flavor::OpenAi => "https://api.openai.com/v1/chat/completions",
            Flavor::XAi => "https://api.x.ai/v1/chat/completions",
            Flavor::DeepSeek => "https://api.deepseek.com/chat/completions",
        }
    }

    /// Only a default for a panel that names the provider without a model.
    pub fn default_model(self) -> ModelId {
        ModelId::new(match self {
            Flavor::OpenAi => "gpt-4o",
            Flavor::XAi => "grok-2-latest",
            Flavor::DeepSeek => "deepseek-chat",
        })
    }
}

pub struct OpenAiCompatibleProvider {
    flavor: Flavor,
    api_key: SecretString,
    client: reqwest::Client,
    base_url: String,
}

impl std::fmt::Debug for OpenAiCompatibleProvider {
    /// Hand-written: a derived `Debug` here would print the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatibleProvider")
            .field("flavor", &self.flavor)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatibleProvider {
    pub fn new(flavor: Flavor, api_key: SecretString) -> Result<Self, ProviderError> {
        Ok(Self {
            flavor,
            api_key,
            client: http::client()?,
            base_url: flavor.base_url().to_string(),
        })
    }

    /// See [`crate::anthropic::AnthropicProvider::with_base_url`] — local
    /// recorded responses in tests, never a live vendor call in CI.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    fn body(request: &ProviderRequest) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": request.model.as_str(),
            "messages": [{ "role": "user", "content": request.prompt }],
        });
        // Pass through max_tokens/temperature when the caller canonicalised
        // them into `params`, so the cache key and the wire request agree.
        if let Ok(params) = serde_json::from_str::<serde_json::Value>(&request.params) {
            for key in ["max_tokens", "temperature"] {
                if let Some(value) = params.get(key) {
                    body[key] = value.clone();
                }
            }
        }
        body
    }

    fn parse(
        flavor: Flavor,
        body: &serde_json::Value,
        request_id: Option<String>,
    ) -> Result<ProviderResponse, ProviderError> {
        let name = flavor.provider_id();
        let text = http::required(body, "/choices/0/message/content", name.as_str())?
            .as_str()
            .ok_or_else(|| {
                ProviderError::Other(format!(
                    "{name}: choices[0].message.content is not a string"
                ))
            })?
            .to_string();
        Ok(ProviderResponse {
            text,
            prompt_tokens: http::optional_u64(body, "/usage/prompt_tokens"),
            completion_tokens: http::optional_u64(body, "/usage/completion_tokens"),
            request_id,
        })
    }
}

impl Provider for OpenAiCompatibleProvider {
    fn id(&self) -> ProviderId {
        self.flavor.provider_id()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            structured_output: true,
            streaming: true,
            idempotency: Some(IdempotencyStyle::Header("Idempotency-Key".to_string())),
        }
    }

    fn call(
        &self,
        request: ProviderRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send + '_>> {
        Box::pin(async move {
            let mut builder = self
                .client
                .post(&self.base_url)
                .bearer_auth(self.api_key.expose())
                .json(&Self::body(&request));
            // Only sent on a retry, and only because `capabilities()` above
            // declares the header — the kernel decides when one exists.
            if let Some(key) = &request.idempotency_key {
                builder = builder.header("Idempotency-Key", key);
            }

            let response = builder
                .send()
                .await
                .map_err(|e| http::transport_error(self.flavor.provider_id().as_str(), e))?;

            let request_id = http::request_id_of(&response);
            let status = response.status();
            let text = response.text().await.map_err(|e| {
                ProviderError::Other(format!("{}: reading body: {e}", self.flavor.provider_id()))
            })?;
            if !status.is_success() {
                return Err(http::status_error(
                    self.flavor.provider_id().as_str(),
                    status,
                    &text,
                ));
            }
            let body: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                ProviderError::Other(format!(
                    "{}: response was not JSON: {e}",
                    self.flavor.provider_id()
                ))
            })?;
            Self::parse(self.flavor, &body, request_id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arbiter_kernel::ids::ReservationId;

    fn request() -> ProviderRequest {
        ProviderRequest {
            model: ModelId::new("gpt-4o"),
            prompt: "Say hello".to_string(),
            params: "{}".to_string(),
            idempotency_key: None,
            reservation: ReservationId::new("r1"),
        }
    }

    #[test]
    fn each_flavor_has_its_own_id_and_endpoint() {
        assert_eq!(Flavor::OpenAi.provider_id().as_str(), "openai");
        assert_eq!(Flavor::XAi.provider_id().as_str(), "xai");
        assert_eq!(Flavor::DeepSeek.provider_id().as_str(), "deepseek");
        assert!(Flavor::XAi.base_url().contains("x.ai"));
        assert!(Flavor::DeepSeek.base_url().contains("deepseek.com"));
    }

    #[test]
    fn body_carries_model_and_prompt() {
        let body = OpenAiCompatibleProvider::body(&request());
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["messages"][0]["content"], "Say hello");
    }

    #[test]
    fn params_pass_through_so_the_cache_key_and_the_wire_agree() {
        let mut r = request();
        r.params = r#"{"max_tokens": 32, "temperature": 0.2, "ignored": 1}"#.to_string();
        let body = OpenAiCompatibleProvider::body(&r);
        assert_eq!(body["max_tokens"], 32);
        assert_eq!(body["temperature"], 0.2);
        assert!(
            body.get("ignored").is_none(),
            "only known params are forwarded"
        );
    }

    #[test]
    fn parse_reads_text_and_both_token_counts() {
        // A recorded Chat Completions response shape.
        let body = serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "Hello there." },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 9, "completion_tokens": 4, "total_tokens": 13 }
        });
        let parsed =
            OpenAiCompatibleProvider::parse(Flavor::OpenAi, &body, Some("req_1".into())).unwrap();
        assert_eq!(parsed.text, "Hello there.");
        assert_eq!(parsed.prompt_tokens, 9);
        assert_eq!(parsed.completion_tokens, 4);
        assert_eq!(parsed.request_id.as_deref(), Some("req_1"));
    }

    #[test]
    fn parse_error_names_the_provider_that_changed_shape() {
        let body = serde_json::json!({ "choices": [] });
        let err = OpenAiCompatibleProvider::parse(Flavor::DeepSeek, &body, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("deepseek"), "{msg}");
        assert!(msg.contains("/choices/0/message/content"), "{msg}");
    }

    #[test]
    fn the_family_declares_idempotency_support_unlike_anthropic() {
        let p = OpenAiCompatibleProvider::new(Flavor::OpenAi, SecretString::new("k")).unwrap();
        assert_eq!(
            p.capabilities().idempotency,
            Some(IdempotencyStyle::Header("Idempotency-Key".to_string()))
        );
    }

    #[test]
    fn debug_never_reveals_the_key() {
        let p = OpenAiCompatibleProvider::new(Flavor::XAi, SecretString::new("xai-secret-value"))
            .unwrap();
        assert!(!format!("{p:?}").contains("xai-secret-value"));
    }
}
