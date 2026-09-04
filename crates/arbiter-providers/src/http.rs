//! Shared HTTP plumbing for the real (network-touching) adapters, P4.
//!
//! Every adapter here issues one **non-streaming** JSON request per call.
//! That is a deliberate reading of the seam, not a shortcut:
//! [`arbiter_kernel::provider::Provider::call`] returns one
//! [`ProviderResponse`] with final `prompt_tokens`/`completion_tokens`, so a
//! streamed body would have to be reassembled into exactly that shape anyway,
//! and every one of these providers reports usage more reliably on the
//! non-streamed response than mid-stream. Streaming belongs to the UI layer
//! (`arbiter serve`'s SSE), which streams *events from the store*, not tokens
//! from a vendor.
//!
//! `request_id` is read from response headers rather than the body:
//! INTERFACES §5 wants it "the moment they arrive ... so an orphaned call is
//! reconcilable against a usage export afterwards", and headers are what a
//! provider populates even on the error paths where a body may be absent or
//! unparseable.

use arbiter_kernel::provider::ProviderError;
use std::time::Duration;

/// Headers each vendor uses for its own request identifier, tried in order.
/// Anthropic documents `request-id`; the OpenAI-compatible family documents
/// `x-request-id`. Checking both on every response costs nothing and means a
/// vendor adding one later is picked up without a code change.
const REQUEST_ID_HEADERS: [&str; 2] = ["request-id", "x-request-id"];

/// One shared client per adapter instance. `reqwest::Client` is already an
/// `Arc` internally, so cloning is cheap and pooling is preserved across calls.
pub(crate) fn client() -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| ProviderError::Other(format!("building HTTP client: {e}")))
}

/// A transport failure (DNS, TLS, connect, timeout — anything short of an
/// HTTP response). `reqwest::Error`'s own `Display` is the outermost layer
/// only, so a bare `{e}` reads "error sending request for url (…)" and throws
/// away the one sentence that says *why* — an expired certificate, a refused
/// connection, a proxy that rejected CONNECT. This walks `source()` so the
/// operator sees the actual cause; the URL is not repeated, since `reqwest`
/// already puts it in the outer message and query strings can carry secrets.
pub(crate) fn transport_error(provider: &str, e: reqwest::Error) -> ProviderError {
    let mut message = format!("{provider} request failed: {e}");
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&e);
    while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    if e.is_timeout() {
        message.push_str(" (the 120s client timeout elapsed)");
    }
    ProviderError::Other(message)
}

pub(crate) fn request_id_of(response: &reqwest::Response) -> Option<String> {
    REQUEST_ID_HEADERS.iter().find_map(|name| {
        response
            .headers()
            .get(*name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    })
}

/// A non-2xx response. The body is included because every one of these vendors
/// puts the actionable part (`model not found`, `invalid_api_key`, a rate-limit
/// reason) in the body, not the status line — but truncated, since an HTML
/// error page from a proxy in front of the API can be enormous.
pub(crate) fn status_error(
    provider: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> ProviderError {
    let mut body = body.trim().to_string();
    body.truncate(400);
    ProviderError::Other(format!("{provider} HTTP {status}: {body}"))
}

/// Reads a JSON field that must be present for the response to be usable at
/// all. A missing one means the vendor changed its response shape, so the
/// error names the field — debugging "which key moved" from a bare
/// `Option::None` is otherwise guesswork.
pub(crate) fn required<'a>(
    value: &'a serde_json::Value,
    pointer: &str,
    provider: &str,
) -> Result<&'a serde_json::Value, ProviderError> {
    value.pointer(pointer).ok_or_else(|| {
        ProviderError::Other(format!(
            "{provider}: response is missing `{pointer}` — the API's response shape may have changed"
        ))
    })
}

/// Token counts are read leniently, unlike the response text: a usage block
/// that a vendor omits or renames costs accounting accuracy, which is a
/// degraded run, while a missing *body* is a failed one. Zero here is honest
/// ("the provider told us nothing"), and the budget ledger already treats a
/// zero-cost call as a real call that happened.
pub(crate) fn optional_u64(value: &serde_json::Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(|v| v.as_u64()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_names_the_missing_pointer() {
        let body = serde_json::json!({"content": []});
        let err = required(&body, "/content/0/text", "anthropic").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("/content/0/text"), "{msg}");
        assert!(msg.contains("anthropic"), "{msg}");
    }

    #[test]
    fn optional_u64_defaults_to_zero_rather_than_failing() {
        let body = serde_json::json!({"usage": {"input_tokens": 12}});
        assert_eq!(optional_u64(&body, "/usage/input_tokens"), 12);
        assert_eq!(optional_u64(&body, "/usage/output_tokens"), 0);
        assert_eq!(optional_u64(&body, "/nothing/here"), 0);
    }

    #[test]
    fn status_error_truncates_a_huge_body() {
        let err = status_error(
            "openai",
            reqwest::StatusCode::BAD_GATEWAY,
            &"x".repeat(5000),
        );
        let msg = err.to_string();
        assert!(msg.contains("502"), "{msg}");
        assert!(
            msg.len() < 500,
            "body must be truncated, got {} chars",
            msg.len()
        );
    }
}
