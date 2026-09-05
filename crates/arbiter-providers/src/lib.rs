//! Speaks to models. Declares capabilities rather than assuming them
//! (ARCHITECTURE.md §7 idempotency, §11.1 credentials). `mock` scripts the whole
//! CI fixture suite and opens no socket, which is what makes CI free.
#![forbid(unsafe_code)]

pub mod anthropic;
pub mod gemini;
mod http;
pub mod keys;
pub mod mock;
pub mod openai_compatible;
pub mod pricing;
pub mod probe;

use arbiter_core::{ModelId, ProviderId};
use arbiter_kernel::provider::{Provider, ProviderError};
use keys::SecretString;
use openai_compatible::Flavor;

/// Every provider this build can actually reach, in the order a panel picker
/// should offer them. `mock` is deliberately absent: it is constructed with a
/// script by whoever needs it, never resolved from a credential.
pub const REAL_PROVIDER_IDS: [&str; 5] = ["anthropic", "openai", "gemini", "xai", "deepseek"];

/// Where an operator goes to get a key for this provider.
///
/// Borrowed from `master-prompt-generator`'s provider-family table, which
/// pairs every family with its console URL: a screen that says "no key
/// configured" and offers a box to paste one into is only half an answer if
/// the reader does not know where the key comes from.
pub fn console_url_for(provider: &ProviderId) -> Option<&'static str> {
    Some(match provider.as_str() {
        "anthropic" => "https://console.anthropic.com/settings/keys",
        "openai" => "https://platform.openai.com/api-keys",
        "gemini" => "https://aistudio.google.com/app/apikey",
        "xai" => "https://console.x.ai",
        "deepseek" => "https://platform.deepseek.com/api_keys",
        _ => return None,
    })
}

/// The default model for a provider named without one.
pub fn default_model_for(provider: &ProviderId) -> Option<ModelId> {
    Some(match provider.as_str() {
        "anthropic" => anthropic::default_model(),
        "openai" => Flavor::OpenAi.default_model(),
        "xai" => Flavor::XAi.default_model(),
        "deepseek" => Flavor::DeepSeek.default_model(),
        "gemini" => gemini::default_model(),
        _ => return None,
    })
}

/// Builds the adapter for a provider id, given its already-resolved key.
///
/// Resolution itself stays in [`keys`] (P3's precedence order: `ARBITER_*`
/// env, then the provider's own env var, then the OS keychain) — this only
/// turns "here is a key for `openai`" into "here is something implementing
/// `Provider`", so the credential path has exactly one implementation.
pub fn build_provider(
    provider: &ProviderId,
    api_key: SecretString,
) -> Result<Box<dyn Provider>, ProviderError> {
    Ok(match provider.as_str() {
        "anthropic" => Box::new(anthropic::AnthropicProvider::new(api_key)?),
        "openai" => Box::new(openai_compatible::OpenAiCompatibleProvider::new(
            Flavor::OpenAi,
            api_key,
        )?),
        "xai" => Box::new(openai_compatible::OpenAiCompatibleProvider::new(
            Flavor::XAi,
            api_key,
        )?),
        "deepseek" => Box::new(openai_compatible::OpenAiCompatibleProvider::new(
            Flavor::DeepSeek,
            api_key,
        )?),
        "gemini" => Box::new(gemini::GeminiProvider::new(api_key)?),
        other => {
            return Err(ProviderError::Other(format!(
                "unknown provider `{other}` — known providers: {}",
                REAL_PROVIDER_IDS.join(", ")
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_runnable_provider_says_where_to_get_a_key() {
        for id in REAL_PROVIDER_IDS {
            assert!(
                console_url_for(&ProviderId::new(id)).is_some(),
                "{id} can be run but the UI cannot say where its key comes from"
            );
        }
        assert!(console_url_for(&ProviderId::new("mock")).is_none());
    }

    #[test]
    fn every_listed_provider_can_actually_be_built() {
        for id in REAL_PROVIDER_IDS {
            let provider = ProviderId::new(id);
            let built = build_provider(&provider, SecretString::new("k"))
                .unwrap_or_else(|e| panic!("{id} is listed but cannot be built: {e}"));
            assert_eq!(
                built.id().as_str(),
                id,
                "adapter reports a different id than it was built for"
            );
            assert!(
                default_model_for(&provider).is_some(),
                "{id} is listed but has no default model"
            );
        }
    }

    #[test]
    fn an_unknown_provider_names_the_ones_that_exist() {
        let err = build_provider(&ProviderId::new("bard"), SecretString::new("k")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bard"), "{msg}");
        assert!(msg.contains("anthropic"), "{msg}");
    }

    #[test]
    fn mock_is_not_resolvable_from_a_credential() {
        assert!(!REAL_PROVIDER_IDS.contains(&"mock"));
        assert!(build_provider(&ProviderId::new("mock"), SecretString::new("k")).is_err());
    }
}
