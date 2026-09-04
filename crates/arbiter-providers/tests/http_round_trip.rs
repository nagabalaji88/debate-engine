//! P4's adapters against a real HTTP round trip.
//!
//! The unit tests inside each adapter cover request-building and
//! response-parsing as pure functions. They cannot catch the failure that
//! actually bites in production: an auth header attached to the wrong place,
//! or not at all. So these stand up a real socket on loopback, serve a
//! recorded vendor response, and assert on what the adapter actually put on
//! the wire.
//!
//! No vendor is contacted. CI has no API key and opens no outbound socket —
//! `with_base_url`/`with_api_root` point each adapter at this local server.

use arbiter_core::ModelId;
use arbiter_kernel::ids::ReservationId;
use arbiter_kernel::provider::{Provider, ProviderRequest};
use arbiter_providers::anthropic::AnthropicProvider;
use arbiter_providers::gemini::GeminiProvider;
use arbiter_providers::keys::SecretString;
use arbiter_providers::openai_compatible::{Flavor, OpenAiCompatibleProvider};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What the adapter sent us, captured for assertions.
#[derive(Default, Clone)]
struct Captured {
    request_line: String,
    headers: String,
    body: String,
}

/// Serves exactly one request, replying with `status` and `response_body`,
/// and hands back what it received. Minimal hand-rolled HTTP rather than a
/// test-server dependency: one request, one response, no keep-alive.
async fn serve_once(
    status: u16,
    response_body: &'static str,
    extra_header: Option<(&'static str, &'static str)>,
) -> (String, Arc<Mutex<Captured>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let captured = Arc::new(Mutex::new(Captured::default()));
    let sink = Arc::clone(&captured);

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buf = vec![0u8; 65536];
        let n = socket.read(&mut buf).await.unwrap_or(0);
        let raw = String::from_utf8_lossy(&buf[..n]).to_string();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let (request_line, headers) = head.split_once("\r\n").unwrap_or((head, ""));
        if let Ok(mut c) = sink.lock() {
            c.request_line = request_line.to_string();
            c.headers = headers.to_lowercase();
            c.body = body.to_string();
        }
        let extra = extra_header
            .map(|(k, v)| format!("{k}: {v}\r\n"))
            .unwrap_or_default();
        let response = format!(
            "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra}connection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.flush().await;
    });

    (format!("http://{addr}"), captured)
}

fn request(model: &str) -> ProviderRequest {
    ProviderRequest {
        model: ModelId::new(model),
        prompt: "Say hello".to_string(),
        params: "{}".to_string(),
        idempotency_key: None,
        reservation: ReservationId::new("r1"),
    }
}

const ANTHROPIC_OK: &str = r#"{"content":[{"type":"text","text":"Hello there."}],"usage":{"input_tokens":9,"output_tokens":4}}"#;
const OPENAI_OK: &str = r#"{"choices":[{"message":{"role":"assistant","content":"Hello there."}}],"usage":{"prompt_tokens":9,"completion_tokens":4}}"#;
const GEMINI_OK: &str = r#"{"candidates":[{"content":{"parts":[{"text":"Hello there."}]}}],"usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":4}}"#;

#[tokio::test]
async fn anthropic_sends_its_key_and_version_headers_and_parses_the_response() {
    let (url, captured) = serve_once(200, ANTHROPIC_OK, Some(("request-id", "req_abc"))).await;
    let provider = AnthropicProvider::new(SecretString::new("sk-ant-test"))
        .unwrap()
        .with_base_url(url);

    let response = provider
        .call(request("claude-sonnet-4-5"))
        .await
        .expect("call succeeds");

    let sent = captured.lock().unwrap().clone();
    assert!(
        sent.request_line.starts_with("POST "),
        "{}",
        sent.request_line
    );
    assert!(
        sent.headers.contains("x-api-key: sk-ant-test"),
        "key header missing: {}",
        sent.headers
    );
    assert!(
        sent.headers.contains("anthropic-version: 2023-06-01"),
        "version header missing: {}",
        sent.headers
    );
    // Anthropic authenticates with x-api-key, NOT a bearer token.
    assert!(
        !sent.headers.contains("authorization:"),
        "must not send a bearer token: {}",
        sent.headers
    );
    assert!(sent.body.contains("\"claude-sonnet-4-5\""), "{}", sent.body);
    assert!(sent.body.contains("Say hello"), "{}", sent.body);

    assert_eq!(response.text, "Hello there.");
    assert_eq!(response.prompt_tokens, 9);
    assert_eq!(response.completion_tokens, 4);
    assert_eq!(
        response.request_id.as_deref(),
        Some("req_abc"),
        "request-id header must be captured"
    );
}

#[tokio::test]
async fn openai_compatible_sends_a_bearer_token_and_parses_the_response() {
    let (url, captured) = serve_once(200, OPENAI_OK, Some(("x-request-id", "req_xyz"))).await;
    let provider =
        OpenAiCompatibleProvider::new(Flavor::OpenAi, SecretString::new("sk-openai-test"))
            .unwrap()
            .with_base_url(url);

    let response = provider
        .call(request("gpt-4o"))
        .await
        .expect("call succeeds");

    let sent = captured.lock().unwrap().clone();
    assert!(
        sent.headers
            .contains("authorization: bearer sk-openai-test"),
        "bearer missing: {}",
        sent.headers
    );
    assert!(sent.body.contains("\"gpt-4o\""), "{}", sent.body);

    assert_eq!(response.text, "Hello there.");
    assert_eq!(response.prompt_tokens, 9);
    assert_eq!(
        response.request_id.as_deref(),
        Some("req_xyz"),
        "x-request-id must be captured"
    );
}

#[tokio::test]
async fn an_idempotency_key_is_sent_only_when_the_kernel_supplies_one() {
    let (url, captured) = serve_once(200, OPENAI_OK, None).await;
    let provider = OpenAiCompatibleProvider::new(Flavor::DeepSeek, SecretString::new("k"))
        .unwrap()
        .with_base_url(url);

    let mut req = request("deepseek-chat");
    req.idempotency_key = Some("idem-123".to_string());
    provider.call(req).await.expect("call succeeds");

    let sent = captured.lock().unwrap().clone();
    assert!(
        sent.headers.contains("idempotency-key: idem-123"),
        "{}",
        sent.headers
    );
}

#[tokio::test]
async fn gemini_puts_the_model_in_the_path_and_the_key_in_a_header() {
    let (url, captured) = serve_once(200, GEMINI_OK, None).await;
    let provider = GeminiProvider::new(SecretString::new("AIza-test"))
        .unwrap()
        .with_api_root(url);

    let response = provider
        .call(request("gemini-2.0-flash"))
        .await
        .expect("call succeeds");

    let sent = captured.lock().unwrap().clone();
    assert!(
        sent.request_line
            .contains("/gemini-2.0-flash:generateContent"),
        "model belongs in the path: {}",
        sent.request_line
    );
    assert!(
        sent.headers.contains("x-goog-api-key: aiza-test"),
        "{}",
        sent.headers
    );
    // The key must never ride in the query string, where it lands in logs.
    assert!(
        !sent.request_line.contains("key="),
        "key must not be in the URL: {}",
        sent.request_line
    );

    assert_eq!(response.text, "Hello there.");
    assert_eq!(response.completion_tokens, 4);
}

#[tokio::test]
async fn a_provider_error_body_reaches_the_caller_instead_of_being_swallowed() {
    const AUTH_ERROR: &str =
        r#"{"error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
    let (url, _captured) = serve_once(401, AUTH_ERROR, None).await;
    let provider = AnthropicProvider::new(SecretString::new("bad"))
        .unwrap()
        .with_base_url(url);

    let err = provider
        .call(request("claude-sonnet-4-5"))
        .await
        .expect_err("401 must be an error");
    let msg = err.to_string();
    assert!(msg.contains("401"), "status must be reported: {msg}");
    assert!(
        msg.contains("invalid x-api-key"),
        "the actionable reason must survive: {msg}"
    );
}
