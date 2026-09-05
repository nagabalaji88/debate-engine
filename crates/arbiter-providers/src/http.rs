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

/// The sentence a vendor actually wrote, dug out of whatever envelope it
/// wrapped it in.
///
/// Every one of these APIs puts the useful part — "Your credit balance is too
/// low", "model not found", a rate-limit reason — in a JSON body, and each
/// wraps it differently. Passing the raw body through meant an operator read
/// `{"type":"error","error":{"type":"invalid_request_error","message":"Your
/// credit balance is too low..."},"request_id":"req_011..."}` when the useful
/// half of that is one sentence. The shapes, in the order tried:
///
/// ```jsonc
/// {"error": {"message": "..."}}   // OpenAI, xAI, DeepSeek, Gemini, Anthropic
/// {"message": "..."}              // some gateways and proxies
/// {"error": "..."}                // a bare string, seen from proxies
/// ```
///
/// Returns `None` when the body is not JSON or carries no message anywhere
/// this knows to look — an HTML error page from a load balancer, say — and
/// the caller falls back to the raw body so nothing is ever swallowed.
pub(crate) fn message_in(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    for pointer in ["/error/message", "/message", "/error/detail", "/detail"] {
        if let Some(text) = value.pointer(pointer).and_then(|v| v.as_str()) {
            let text = text.trim();
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    // `{"error": "a bare string"}`
    value
        .get("error")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A non-2xx response, rendered as the vendor's own sentence where one can be
/// found and the raw body otherwise. Truncated either way: an HTML error page
/// from a proxy in front of the API can be enormous.
pub(crate) fn status_error(
    provider: &str,
    status: reqwest::StatusCode,
    body: &str,
) -> ProviderError {
    let mut detail = message_in(body).unwrap_or_else(|| body.trim().to_string());
    if detail.chars().count() > 400 {
        detail = detail.chars().take(400).collect::<String>() + "…";
    }
    ProviderError::Http {
        status: status.as_u16(),
        message: format!("{provider} HTTP {status}: {detail}"),
    }
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

    /// The exact envelope that prompted this: a live Anthropic key with an
    /// exhausted balance. The operator needs the sentence, not the wrapper.
    #[test]
    fn anthropics_error_envelope_yields_its_sentence() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."},"request_id":"req_011CejXCLm12kCqVoBCCSuMN"}"#;
        assert_eq!(
            message_in(body).as_deref(),
            Some(
                "Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."
            )
        );
        let rendered =
            status_error("anthropic", reqwest::StatusCode::BAD_REQUEST, body).to_string();
        assert!(rendered.contains("credit balance is too low"), "{rendered}");
        assert!(
            !rendered.contains("request_id") && !rendered.contains("invalid_request_error"),
            "the envelope must not survive into the message: {rendered}"
        );
    }

    #[test]
    fn every_vendor_envelope_shape_is_understood() {
        // OpenAI / xAI / DeepSeek
        assert_eq!(
            message_in(r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#).as_deref(),
            Some("Incorrect API key provided")
        );
        // Gemini
        assert_eq!(
            message_in(r#"{"error":{"code":400,"message":"API key not valid.","status":"INVALID_ARGUMENT"}}"#).as_deref(),
            Some("API key not valid.")
        );
        // A gateway that puts it at the top level
        assert_eq!(
            message_in(r#"{"message":"upstream timed out"}"#).as_deref(),
            Some("upstream timed out")
        );
        // A bare string
        assert_eq!(
            message_in(r#"{"error":"service unavailable"}"#).as_deref(),
            Some("service unavailable")
        );
    }

    /// Nothing is ever swallowed: a body this cannot parse still reaches the
    /// operator whole, because an unrecognised shape is exactly when they most
    /// need to see it.
    #[test]
    fn an_unparseable_body_falls_back_to_itself() {
        let html = "<html><body>502 Bad Gateway</body></html>";
        assert_eq!(message_in(html), None);
        let rendered = status_error("openai", reqwest::StatusCode::BAD_GATEWAY, html).to_string();
        assert!(rendered.contains("502 Bad Gateway"), "{rendered}");
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
