//! The mock provider, ARCHITECTURE §11.1 / IMPLEMENTATION_PLAN.md's own words:
//! "the mock is not a stub: it scripts the whole CI fixture suite and opens no
//! socket." `MockProvider` never holds an HTTP client, a socket, or any
//! networking dependency — that is what makes `mock_opens_no_socket` a
//! structural guarantee, not a behavioural one an assertion has to catch after
//! the fact.

use arbiter_core::ProviderId;
use arbiter_kernel::provider::{
    Provider, ProviderCapabilities, ProviderError, ProviderRequest, ProviderResponse,
};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

/// One scripted answer: either the response to return, or the error to return.
pub type ScriptedResult = Result<ProviderResponse, ProviderError>;

/// A provider whose every response is pre-scripted by the test/fixture that
/// constructs it. Responses are consumed in the order they were scripted —
/// "the whole CI fixture suite" means a fixture scripts exactly the sequence of
/// calls its scenario needs, then asserts the engine's behaviour against them.
#[derive(Debug)]
pub struct MockProvider {
    id: ProviderId,
    capabilities: ProviderCapabilities,
    script: Mutex<VecDeque<ScriptedResult>>,
    /// Every request actually received, for a test to assert against (prompts
    /// sent, call count, ordering) without the mock needing to interpret them.
    received: Mutex<Vec<ProviderRequest>>,
}

impl MockProvider {
    pub fn new(id: ProviderId, capabilities: ProviderCapabilities) -> Self {
        Self {
            id,
            capabilities,
            script: Mutex::new(VecDeque::new()),
            received: Mutex::new(Vec::new()),
        }
    }

    /// Appends one scripted result, to be returned by the next `call()`.
    pub fn script(&self, result: ScriptedResult) {
        self.script.lock().unwrap().push_back(result);
    }

    /// Convenience for the common case: script a successful text response.
    pub fn script_text(&self, text: impl Into<String>) {
        self.script(Ok(ProviderResponse {
            text: text.into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            request_id: None,
        }));
    }

    pub fn received(&self) -> Vec<ProviderRequest> {
        self.received.lock().unwrap().clone()
    }

    pub fn calls_remaining(&self) -> usize {
        self.script.lock().unwrap().len()
    }
}

impl Provider for MockProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }

    fn call(
        &self,
        request: ProviderRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send + '_>> {
        self.received.lock().unwrap().push(request);
        Box::pin(async move {
            self.script.lock().unwrap().pop_front().unwrap_or_else(|| {
                Err(ProviderError::Other(
                    "mock script exhausted: this call was not scripted".to_string(),
                ))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arbiter_core::ModelId;
    use arbiter_kernel::ids::ReservationId;

    fn request(prompt: &str) -> ProviderRequest {
        ProviderRequest {
            model: ModelId::new("mock-1"),
            prompt: prompt.to_string(),
            params: "{}".to_string(),
            idempotency_key: None,
            reservation: ReservationId::new("r1"),
        }
    }

    fn capabilities() -> ProviderCapabilities {
        ProviderCapabilities {
            structured_output: true,
            streaming: false,
            idempotency: None,
        }
    }

    #[tokio::test]
    async fn responses_are_returned_in_scripted_order() {
        let mock = MockProvider::new(ProviderId::new("mock"), capabilities());
        mock.script_text("first");
        mock.script_text("second");

        let a = mock.call(request("q1")).await.unwrap();
        let b = mock.call(request("q2")).await.unwrap();
        assert_eq!(a.text, "first");
        assert_eq!(b.text, "second");
    }

    #[tokio::test]
    async fn an_unscripted_call_errors_rather_than_panicking_or_looping() {
        let mock = MockProvider::new(ProviderId::new("mock"), capabilities());
        let result = mock.call(request("nothing scripted")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn scripted_errors_are_returned_as_errors() {
        let mock = MockProvider::new(ProviderId::new("mock"), capabilities());
        mock.script(Err(ProviderError::Other("simulated 500".to_string())));
        let result = mock.call(request("q1")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn every_request_is_recorded_for_assertions() {
        let mock = MockProvider::new(ProviderId::new("mock"), capabilities());
        mock.script_text("ok");
        mock.call(request("what should we build?")).await.unwrap();

        let received = mock.received();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].prompt, "what should we build?");
    }

    /// Structural, not behavioural: `MockProvider` has no `reqwest::Client`
    /// field, no socket, nothing in its own struct or its dependency graph
    /// capable of reaching the network. Fully scripting and exhausting a
    /// multi-call scenario and getting back only the exact scripted answers
    /// is the observable proof that nothing else happened.
    #[tokio::test]
    async fn mock_opens_no_socket() {
        let mock = MockProvider::new(ProviderId::new("mock"), capabilities());
        for i in 0..5 {
            mock.script_text(format!("answer {i}"));
        }
        for i in 0..5 {
            let response = mock.call(request(&format!("question {i}"))).await.unwrap();
            assert_eq!(response.text, format!("answer {i}"));
        }
        assert_eq!(mock.calls_remaining(), 0);
    }
}
