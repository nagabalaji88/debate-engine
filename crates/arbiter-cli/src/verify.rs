//! Key verification: one minimal, real request per provider.
//!
//! ARCHITECTURE §11.1 asks for exactly this and for its result to be cached
//! "keyed by a fingerprint, never by the key", with a 24-hour TTL. Before P4
//! there was no adapter to make the request with, so `arbiter keys test` and
//! `POST /api/providers/:p/test` both refused honestly. The adapters exist
//! now, so both do the real thing.
//!
//! **This spends money.** One completion of at most a couple of tokens, which
//! is the cheapest question that still proves the whole path works — the key
//! is accepted, the model name resolves, and the network reaches the vendor.
//! A HEAD request or a models-list call would prove less: several of these
//! providers accept a key for listing that they then reject for inference.

use arbiter_core::ProviderId;
use arbiter_kernel::ids::ReservationId;
use arbiter_kernel::provider::{ProviderError, ProviderRequest};
use arbiter_providers::keys::{
    CredentialSource, EnvCredentialSource, KeychainCredentialSource, SecretString,
    VerificationCache, VerifyResult,
};
use arbiter_providers::{build_provider, default_model_for};

/// The shortest prompt that still forces a real completion. Deliberately not
/// empty: some providers reject an empty message outright, which would look
/// like a bad key.
const PROBE_PROMPT: &str = "Reply with the single word: ok";

/// Process-global, matching `VerificationCache`'s own in-memory design. Both
/// callers (the CLI command and the serve endpoint) share it so a `Test`
/// clicked twice in the UI does not spend twice.
fn cache() -> &'static VerificationCache {
    static CACHE: std::sync::OnceLock<VerificationCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(VerificationCache::new)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What a verification attempt learned. Wider than [`VerifyResult`], which
/// has no way to say "we never reached the vendor" — and conflating a dead
/// network with a rejected key would tell an operator to replace a key that
/// is perfectly good.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verification {
    Verified {
        model: String,
        cached: bool,
    },
    /// The key itself was refused: 401 or 403, the two statuses HTTP reserves
    /// for "who are you" and "you may not". Only this means "replace the key".
    Rejected {
        status: u16,
        message: String,
    },
    /// The vendor answered, the key got *past* authentication, and the request
    /// still was not served — no credit on the account, a rate limit, a model
    /// the plan does not include, an outage.
    ///
    /// This is its own state because the first version collapsed it into
    /// `Rejected`, and a live key with an empty balance came back as
    /// "rejected: HTTP 400 ... credit balance is too low". That told the
    /// operator to replace a key that was perfectly good. Anything other than
    /// 401/403 got past auth by definition, so the key is not the problem.
    Blocked {
        status: u16,
        message: String,
    },
    /// No response at all: DNS, TLS, a proxy, a timeout.
    Unreachable {
        message: String,
    },
    NoKey,
}

impl Verification {
    /// The `state` string `GET /api/providers` and `arbiter keys list` both
    /// speak, so a verified key reads the same everywhere.
    pub(crate) fn state(&self) -> &'static str {
        match self {
            Verification::Verified { .. } => "verified",
            Verification::Rejected { .. } => "rejected",
            Verification::Blocked { .. } => "blocked",
            Verification::Unreachable { .. } => "unreachable",
            Verification::NoKey => "missing",
        }
    }

    /// One short sentence saying what this means for the operator, separate
    /// from the vendor's own words. Two lines that both said "your key is
    /// fine" read as padding; this is the only place that framing appears.
    pub(crate) fn headline(&self, provider: &str) -> String {
        match self {
            Verification::Verified { cached: true, .. } => {
                "Verified — checked within the last 24 hours.".to_string()
            }
            Verification::Verified { .. } => "Verified — the key works.".to_string(),
            Verification::Rejected { .. } => {
                format!("{provider} refused this key. Replace it.")
            }
            Verification::Blocked { .. } => {
                format!(
                    "Your key works. {provider} refused the request — check that account's billing, plan or rate limits."
                )
            }
            Verification::Unreachable { .. } => {
                format!(
                    "Could not reach {provider} at all — nothing here says whether the key is good."
                )
            }
            Verification::NoKey => "No key configured for this provider.".to_string(),
        }
    }

    /// What the vendor (or the transport) actually said, with no framing of
    /// our own wrapped around it.
    pub(crate) fn detail(&self) -> String {
        match self {
            Verification::Verified { model, .. } => model.clone(),
            Verification::Rejected { message, .. }
            | Verification::Blocked { message, .. }
            | Verification::Unreachable { message } => message.clone(),
            Verification::NoKey => String::new(),
        }
    }

    /// The HTTP status, when a vendor answered with one.
    pub(crate) fn status(&self) -> Option<u16> {
        match self {
            Verification::Rejected { status, .. } | Verification::Blocked { status, .. } => {
                Some(*status)
            }
            _ => None,
        }
    }
}

fn resolve_key(provider: &ProviderId) -> Option<SecretString> {
    let env = EnvCredentialSource;
    let keychain = KeychainCredentialSource;
    let sources: [&dyn CredentialSource; 2] = [&env, &keychain];
    sources
        .iter()
        .find_map(|s| s.resolve(provider))
        .map(|(k, _)| k)
}

/// Verifies one provider's key with a single real call, consulting the cache
/// first. `mock` is always verified without spending anything — it needs no
/// key and opens no socket.
pub(crate) async fn verify(provider: &ProviderId) -> Verification {
    if provider.as_str() == "mock" {
        return Verification::Verified {
            model: "synthetic panel".to_string(),
            cached: false,
        };
    }
    let Some(model) = default_model_for(provider) else {
        return Verification::Unreachable {
            message: format!("`{provider}` is not a provider this build can reach"),
        };
    };
    let Some(secret) = resolve_key(provider) else {
        return Verification::NoKey;
    };

    // §11.1's cache key: provider, model, and a fingerprint of the key --
    // never the key itself.
    let fingerprint = secret.fingerprint();
    if let Some(hit) = cache().get(provider.as_str(), model.as_str(), &fingerprint, now_unix()) {
        return match hit {
            VerifyResult::Verified => Verification::Verified {
                model: model.as_str().to_string(),
                cached: true,
            },
            VerifyResult::Rejected { status } => Verification::Rejected {
                status,
                message: format!("previously rejected with HTTP {status}"),
            },
        };
    }

    let adapter = match build_provider(provider, secret) {
        Ok(a) => a,
        Err(e) => {
            return Verification::Unreachable {
                message: e.to_string(),
            };
        }
    };
    let request = ProviderRequest {
        model: model.clone(),
        prompt: PROBE_PROMPT.to_string(),
        params: serde_json::json!({"max_tokens": 4}).to_string(),
        idempotency_key: None,
        // No ledger is involved -- this is not a run -- but the field is
        // required, so it names itself.
        reservation: ReservationId::new(format!("verify_{provider}")),
    };

    match adapter.call(request).await {
        Ok(_) => {
            cache().put(
                provider.as_str(),
                model.as_str(),
                &fingerprint,
                VerifyResult::Verified,
                now_unix(),
            );
            Verification::Verified {
                model: model.as_str().to_string(),
                cached: false,
            }
        }
        // A refusal the vendor actually answered with. Only an authentication
        // failure is cached: a 429 or a 503 says nothing about the key, and
        // caching it for 24 hours would keep telling the operator their key
        // is bad long after the rate limit cleared.
        // 401/403 are the only two statuses that mean the *key* was refused.
        // Cached, because a bad key stays bad until it is replaced.
        Err(ProviderError::Http { status, message }) if matches!(status, 401 | 403) => {
            cache().put(
                provider.as_str(),
                model.as_str(),
                &fingerprint,
                VerifyResult::Rejected { status },
                now_unix(),
            );
            Verification::Rejected { status, message }
        }
        // Any other answer: authentication succeeded and the request still
        // failed. Never cached — topping up a balance or waiting out a rate
        // limit changes the answer immediately, and a 24-hour cache would go
        // on reporting a problem the operator had already fixed.
        Err(ProviderError::Http { status, message }) => Verification::Blocked { status, message },
        // Never reached the vendor at all: DNS, TLS, a proxy, a timeout. Not
        // the key's fault, and never cached.
        Err(e) => Verification::Unreachable {
            message: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_verifies_without_a_key_or_a_socket() {
        let v = verify(&ProviderId::new("mock")).await;
        assert_eq!(v.state(), "verified");
    }

    #[tokio::test]
    async fn an_unknown_provider_is_unreachable_not_rejected() {
        // "Rejected" would imply a vendor answered and refused the key.
        let v = verify(&ProviderId::new("bard")).await;
        assert_eq!(v.state(), "unreachable");
        assert!(v.detail().contains("bard"), "{}", v.detail());
    }

    #[tokio::test]
    async fn a_provider_with_no_key_says_so_rather_than_calling_anything() {
        let v = verify(&ProviderId::new("anthropic")).await;
        assert_eq!(v.state(), "missing");
        assert_eq!(
            v.detail(),
            "",
            "nothing was called, so there is nothing to quote"
        );
        assert!(v.headline("anthropic").contains("No key configured"));
    }

    /// The defect this state exists for: a live Anthropic key with an empty
    /// balance answers `400 invalid_request_error` — "Your credit balance is
    /// too low" — and the first version reported that as `rejected`, telling
    /// the operator to replace a key that authenticated perfectly well.
    #[test]
    fn a_billing_failure_is_blocked_not_rejected() {
        let blocked = Verification::Blocked {
            status: 400,
            message: "anthropic HTTP 400 Bad Request: Your credit balance is too low".to_string(),
        };
        assert_eq!(blocked.state(), "blocked");
        assert_eq!(blocked.status(), Some(400));
        // The framing is ours and says the key is fine; the detail is the
        // vendor's own sentence, unwrapped and unparaphrased.
        let headline = blocked.headline("anthropic");
        assert!(headline.contains("Your key works"), "{headline}");
        assert!(headline.contains("billing"), "{headline}");
        assert_eq!(
            blocked.detail(),
            "anthropic HTTP 400 Bad Request: Your credit balance is too low"
        );
    }

    /// Only the two statuses HTTP reserves for authentication mean the key
    /// itself was refused. Everything else got past auth by definition.
    #[test]
    fn only_401_and_403_mean_the_key_is_the_problem() {
        for status in [400u16, 402, 404, 429, 500, 503] {
            assert_eq!(
                Verification::Blocked {
                    status,
                    message: String::new()
                }
                .state(),
                "blocked",
                "HTTP {status} authenticated, so it must not read as a bad key"
            );
        }
        for status in [401u16, 403] {
            assert_eq!(
                Verification::Rejected {
                    status,
                    message: String::new()
                }
                .state(),
                "rejected"
            );
        }
    }

    #[test]
    fn a_rejection_reports_its_status_and_a_verification_names_the_model() {
        let rejected = Verification::Rejected {
            status: 401,
            message: "anthropic HTTP 401 Unauthorized".to_string(),
        };
        assert_eq!(rejected.state(), "rejected");
        assert!(
            rejected.headline("anthropic").contains("Replace it"),
            "a refused key must say what to do: {}",
            rejected.headline("anthropic")
        );

        let cached = Verification::Verified {
            model: "claude-sonnet-4-5".to_string(),
            cached: true,
        };
        assert_eq!(cached.detail(), "claude-sonnet-4-5");
        assert!(
            cached.headline("anthropic").contains("24 hours"),
            "{}",
            cached.headline("anthropic")
        );
    }
}
