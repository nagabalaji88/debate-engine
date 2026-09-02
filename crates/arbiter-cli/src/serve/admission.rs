//! The five-step admission gate, ARCHITECTURE §17.1's own ordered table —
//! transcribed as code, not summarized. Applied as one `axum` middleware
//! layer wrapping the whole [`super::router`], so every route — `GET /`
//! included — is checked identically before any handler runs, which is
//! exactly what makes `rejection_precedes_run_lookup` true: a request for
//! `/api/runs/nonexistent-id` is rejected on the same code path, before the
//! same first byte, as one for a real run id.
//!
//! Step 1 (loopback bind) is enforced once, at startup, in
//! [`super::serve_command`] — there is no per-request socket to inspect.
//! Steps 2-5 live here.

use super::AppState;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// Mints the per-process 128-bit admission token (ARCHITECTURE §17.1: "a
/// page on any website can reach 127.0.0.1 in your browser" — this is the
/// one thing that stops it). `getrandom` alone, not the full `rand` crate:
/// one OS-random fill of 16 bytes, once, is the entire need. Hex-encoded by
/// hand rather than pulling in a hex crate for sixteen bytes.
pub(crate) fn mint_token() -> anyhow::Result<Arc<str>> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| anyhow::anyhow!("minting the admission token: {e}"))?;
    let mut hex = String::with_capacity(32);
    for b in bytes {
        hex.push_str(&format!("{b:02x}"));
    }
    Ok(hex.into())
}

/// Byte-for-byte, unconditionally over the full length of both slices —
/// never short-circuiting on the first mismatch, which is what "compared in
/// constant time" (§17.1) is actually asking for: an early `return false`
/// on the first differing byte leaks, via timing, how many leading bytes of
/// a guess were correct.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Step 2: `Host` must name this loopback server, by address or by
/// `localhost`, optionally with the bound port — anything else is exactly
/// what DNS rebinding depends on (a hostile DNS name that resolves to
/// 127.0.0.1, so the browser's same-origin policy sees a "different" host
/// than the socket it's actually talking to).
fn host_is_loopback(host: &str) -> bool {
    let name = host.split(':').next().unwrap_or(host);
    name == "127.0.0.1" || name == "localhost"
}

/// Step 3: `Origin`, when a browser sends one at all (plain navigation and
/// non-browser clients typically don't), must name exactly this server's
/// own origin.
fn origin_is_self_or_absent(origin: Option<&str>, self_origin: &str) -> bool {
    match origin {
        None => true,
        Some(o) => o == self_origin,
    }
}

/// Step 4: `Sec-Fetch-Site`. The Fetch Metadata values a real browser sends
/// on **any** request once support shipped are `same-origin` (a fetch from
/// this page), `same-site`, `cross-site`, or `none` (direct, user-initiated
/// navigation — typing the URL, following `--open`'s own link). Read
/// literally, ARCHITECTURE §17.1's "absent, or same-origin" would reject
/// `none` and so reject the very first page load every browser actually
/// performs; the reading applied here instead follows the documented Fetch
/// Metadata mitigation this table is otherwise quoting almost verbatim —
/// only `cross-site` (a request originating from another site entirely,
/// the drive-by-form-post case the requirement exists to stop) is refused.
/// Logged as a plan/spec precision gap (PLAN_DEVIATIONS.md D48), not
/// invented from nothing.
fn sec_fetch_site_is_acceptable(value: Option<&str>) -> bool {
    value != Some("cross-site")
}

/// Step 5: the token, read from `X-Arbiter-Token` (every fetch the page's
/// own JS makes) or the `token` query parameter (the one bare `GET /`
/// navigation the token's own URL carries it on, since a top-level
/// navigation cannot set a custom header) — compared in constant time
/// either way.
fn extract_token<'a>(
    headers: &'a HeaderMap,
    uri: &'a axum::http::Uri,
) -> Option<std::borrow::Cow<'a, str>> {
    if let Some(h) = header_str(headers, "x-arbiter-token") {
        return Some(std::borrow::Cow::Borrowed(h));
    }
    let query = uri.query()?;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("token=") {
            return Some(std::borrow::Cow::Owned(
                percent_decode(v).unwrap_or_else(|| v.to_string()),
            ));
        }
    }
    None
}

/// Minimal percent-decoding for the one query parameter this server ever
/// reads (`token`, itself always plain lowercase hex, so no decode should
/// ever be *needed* — this exists only so a client that percent-encodes
/// defensively still works). Not a general-purpose URL decoder: unknown
/// escapes are left as-is rather than erroring.
fn percent_decode(s: &str) -> Option<String> {
    if !s.contains('%') {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// One failure, one shape: `403`, empty body — never a reason, per
/// ARCHITECTURE §17.1's own "403 and no body at the first failure," so a
/// probe cannot distinguish "wrong token" from "wrong host" from "run does
/// not exist" by response shape either.
fn refuse() -> Response {
    StatusCode::FORBIDDEN.into_response()
}

pub(crate) async fn admission(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let headers = request.headers();

    let Some(host) = header_str(headers, "host") else {
        return refuse();
    };
    if !host_is_loopback(host) {
        return refuse();
    }

    if !origin_is_self_or_absent(header_str(headers, "origin"), &state.origin) {
        return refuse();
    }

    if !sec_fetch_site_is_acceptable(header_str(headers, "sec-fetch-site")) {
        return refuse();
    }

    let token = extract_token(headers, request.uri());
    let token_bytes = token.as_deref().unwrap_or("").as_bytes();
    if !constant_time_eq(token_bytes, state.token.as_bytes()) {
        return refuse();
    }

    let mut response = next.run(request).await;
    // No CORS headers, ever (§17.1) -- structural, not just "we never add
    // one": anything a prior layer or handler set is stripped here too, so
    // this invariant holds regardless of what runs before it.
    for name in [
        "access-control-allow-origin",
        "access-control-allow-credentials",
        "access-control-allow-methods",
        "access-control-allow-headers",
        "access-control-expose-headers",
    ] {
        response.headers_mut().remove(name);
    }
    response
}
