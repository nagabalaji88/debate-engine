//! Google Gemini (Generative Language API) adapter, P4.
//!
//! The one adapter here that shares no shape with the others: a
//! `contents`/`parts` request, a `candidates` response, `usageMetadata`
//! instead of `usage`, and the model id in the URL path rather than the body.
//! It also authenticates with an `x-goog-api-key` header rather than a bearer
//! token — the key is deliberately kept out of the query string, where it
//! would end up in server logs and browser history.

use crate::http;
use crate::keys::SecretString;
use arbiter_core::{ModelId, ProviderId};
use arbiter_kernel::provider::{
    Provider, ProviderCapabilities, ProviderError, ProviderRequest, ProviderResponse,
};
use std::future::Future;
use std::pin::Pin;

const API_ROOT: &str = "https://generativelanguage.googleapis.com/v1beta/models";

pub struct GeminiProvider {
    api_key: SecretString,
    client: reqwest::Client,
    api_root: String,
}

impl std::fmt::Debug for GeminiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiProvider")
            .field("api_root", &self.api_root)
            .finish_non_exhaustive()
    }
}

impl GeminiProvider {
    pub fn new(api_key: SecretString) -> Result<Self, ProviderError> {
        Ok(Self {
            api_key,
            client: http::client()?,
            api_root: API_ROOT.to_string(),
        })
    }

    pub fn with_api_root(mut self, url: impl Into<String>) -> Self {
        self.api_root = url.into();
        self
    }

    /// Gemini puts the model in the path, so the URL is per-call.
    fn url(&self, model: &ModelId) -> String {
        format!("{}/{}:generateContent", self.api_root, model.as_str())
    }

    fn body(request: &ProviderRequest) -> serde_json::Value {
        let mut body = serde_json::json!({
            "contents": [{ "parts": [{ "text": request.prompt }] }],
        });
        if let Ok(params) = serde_json::from_str::<serde_json::Value>(&request.params) {
            let mut config = serde_json::Map::new();
            // Gemini's own spelling for the same two knobs the others take.
            if let Some(v) = params.get("max_tokens") {
                config.insert("maxOutputTokens".to_string(), v.clone());
            }
            if let Some(v) = params.get("temperature") {
                config.insert("temperature".to_string(), v.clone());
            }
            if !config.is_empty() {
                body["generationConfig"] = serde_json::Value::Object(config);
            }
        }
        body
    }

    fn parse(
        body: &serde_json::Value,
        request_id: Option<String>,
    ) -> Result<ProviderResponse, ProviderError> {
        let text = http::required(body, "/candidates/0/content/parts/0/text", "gemini")?
            .as_str()
            .ok_or_else(|| {
                ProviderError::Other(
                    "gemini: candidates[0].content.parts[0].text is not a string".into(),
                )
            })?
            .to_string();
        Ok(ProviderResponse {
            text,
            prompt_tokens: http::optional_u64(body, "/usageMetadata/promptTokenCount"),
            completion_tokens: http::optional_u64(body, "/usageMetadata/candidatesTokenCount"),
            request_id,
        })
    }
}

impl Provider for GeminiProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new("gemini")
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            structured_output: true,
            streaming: true,
            // No idempotency header documented for generateContent.
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
                .post(self.url(&request.model))
                .header("x-goog-api-key", self.api_key.expose())
                .json(&Self::body(&request))
                .send()
                .await
                .map_err(|e| http::transport_error("gemini", e))?;

            let request_id = http::request_id_of(&response);
            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|e| ProviderError::Other(format!("gemini: reading body: {e}")))?;
            if !status.is_success() {
                return Err(http::status_error("gemini", status, &text));
            }
            let body: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| ProviderError::Other(format!("gemini: response was not JSON: {e}")))?;
            Self::parse(&body, request_id)
        })
    }
}

pub fn default_model() -> ModelId {
    ModelId::new("gemini-2.0-flash")
}

#[cfg(test)]
mod tests {
    use super::*;
    use arbiter_kernel::ids::ReservationId;

    fn request() -> ProviderRequest {
        ProviderRequest {
            model: ModelId::new("gemini-2.0-flash"),
            prompt: "Say hello".to_string(),
            params: "{}".to_string(),
            idempotency_key: None,
            reservation: ReservationId::new("r1"),
        }
    }

    #[test]
    fn model_goes_in_the_path_not_the_body() {
        let p = GeminiProvider::new(SecretString::new("k")).unwrap();
        let url = p.url(&ModelId::new("gemini-2.0-flash"));
        assert!(
            url.ends_with("/models/gemini-2.0-flash:generateContent"),
            "{url}"
        );
        assert!(GeminiProvider::body(&request()).get("model").is_none());
    }

    #[test]
    fn body_uses_gemini_spelling_for_the_shared_knobs() {
        let mut r = request();
        r.params = r#"{"max_tokens": 32, "temperature": 0.2}"#.to_string();
        let body = GeminiProvider::body(&r);
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 32);
        assert_eq!(body["generationConfig"]["temperature"], 0.2);
    }

    #[test]
    fn no_generation_config_is_sent_when_no_params_were_set() {
        assert!(
            GeminiProvider::body(&request())
                .get("generationConfig")
                .is_none()
        );
    }

    #[test]
    fn parse_reads_text_and_usage_metadata() {
        // A recorded generateContent response shape.
        let body = serde_json::json!({
            "candidates": [{
                "content": { "parts": [{ "text": "Hello there." }], "role": "model" },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 9,
                "candidatesTokenCount": 4,
                "totalTokenCount": 13
            }
        });
        let parsed = GeminiProvider::parse(&body, None).unwrap();
        assert_eq!(parsed.text, "Hello there.");
        assert_eq!(parsed.prompt_tokens, 9);
        assert_eq!(parsed.completion_tokens, 4);
    }

    #[test]
    fn a_blocked_response_with_no_candidate_fails_loudly() {
        // Safety-filtered responses come back 200 with no candidate content;
        // that must surface as an error, not an empty-string answer.
        let body = serde_json::json!({ "promptFeedback": { "blockReason": "SAFETY" } });
        let err = GeminiProvider::parse(&body, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("/candidates/0/content/parts/0/text"),
            "{err}"
        );
    }

    #[test]
    fn debug_never_reveals_the_key() {
        let p = GeminiProvider::new(SecretString::new("AIza-secret-value")).unwrap();
        assert!(!format!("{p:?}").contains("AIza-secret-value"));
    }
}
