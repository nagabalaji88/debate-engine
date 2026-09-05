//! Turns `--panel` into a real roster plus the adapters that serve it (P4b).
//!
//! Before P4 this was a single hard-coded mock roster and a `bail!` for
//! anything else. Now a panel is a comma-separated list of providers, each
//! optionally pinning a model:
//!
//! ```text
//! --panel mock                                  the synthetic panel (no keys, no network)
//! --panel anthropic,openai,gemini               one model each, provider defaults
//! --panel anthropic:claude-sonnet-4-5,openai:gpt-4o
//! ```
//!
//! A provider named here but holding no resolvable credential is a hard
//! error, not a silent drop: ARCHITECTURE §6.2's independence term is
//! computed over the panel that actually ran, so quietly running a
//! three-model panel as two would inflate confidence against a panel the
//! operator never approved.

use arbiter_core::{ModelId, ProviderId};
use arbiter_kernel::stage::ProviderRegistry;
use arbiter_providers::keys::{CredentialSource, EnvCredentialSource, KeychainCredentialSource};
use arbiter_providers::{build_provider, default_model_for};

pub(crate) type Roster = Vec<(ModelId, ProviderId)>;

pub(crate) struct ResolvedPanel {
    pub panel: Roster,
    pub judges: Roster,
    pub providers: ProviderRegistry,
}

/// One `provider[:model]` entry, parsed but not yet resolved to a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PanelEntry {
    provider: ProviderId,
    model: ModelId,
}

fn parse_spec(spec: &str) -> anyhow::Result<Vec<PanelEntry>> {
    let mut entries = Vec::new();
    for raw in spec.split(',') {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        let (provider_str, model_str) = match part.split_once(':') {
            Some((p, m)) => (p.trim(), Some(m.trim())),
            None => (part, None),
        };
        if provider_str.is_empty() {
            anyhow::bail!("panel entry `{part}` has no provider before the `:`");
        }
        let provider = ProviderId::new(provider_str);
        let model = match model_str {
            Some(m) if !m.is_empty() => ModelId::new(m),
            _ => default_model_for(&provider).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown provider `{provider_str}` — known providers: {}, mock",
                    arbiter_providers::REAL_PROVIDER_IDS.join(", ")
                )
            })?,
        };
        entries.push(PanelEntry { provider, model });
    }
    if entries.is_empty() {
        anyhow::bail!("--panel is empty; name at least one provider (or `mock`)");
    }
    Ok(entries)
}

/// The synthetic panel: three models and a judge, no keys, no sockets. Kept
/// as its own path because every CI fixture and every offline demo depends on
/// it behaving exactly as it did before P4.
pub(crate) fn mock_panel() -> (Roster, Roster, ProviderId) {
    let provider = ProviderId::new("mock");
    let panel = vec![
        (ModelId::new("model-a"), provider.clone()),
        (ModelId::new("model-b"), provider.clone()),
        (ModelId::new("model-c"), provider.clone()),
    ];
    let judges = vec![(ModelId::new("judge-1"), provider.clone())];
    (panel, judges, provider)
}

/// How many models one named provider contributes to a panel. Every real
/// provider is one model; `mock` is the whole synthetic roster in a single
/// name. Screen 1's estimate needs this to turn a set of ticked boxes into a
/// model count, and reading it from [`mock_panel`] rather than writing `3`
/// means the two can never disagree.
pub(crate) fn models_contributed_by(provider: &ProviderId) -> usize {
    if provider.as_str() == "mock" {
        mock_panel().0.len()
    } else {
        1
    }
}

/// Resolves a panel spec into a roster plus registered adapters.
///
/// `mock` short-circuits to the synthetic path. Anything else resolves each
/// named provider's credential through P3's own precedence order
/// (`ARBITER_<PROVIDER>_API_KEY`, then the provider's conventional variable,
/// then the OS keychain) — this never reads a key itself.
pub(crate) fn resolve(spec: &str) -> anyhow::Result<ResolvedPanel> {
    if spec.trim() == "mock" {
        let (panel, judges, provider_id) = mock_panel();
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(crate::synthetic::SyntheticProvider::new(
            provider_id,
        )));
        return Ok(ResolvedPanel {
            panel,
            judges,
            providers,
        });
    }

    let entries = parse_spec(spec)?;
    let env = EnvCredentialSource;
    let keychain = KeychainCredentialSource;
    let sources: [&dyn CredentialSource; 2] = [&env, &keychain];

    let mut providers = ProviderRegistry::new();
    let mut panel: Roster = Vec::new();

    for entry in &entries {
        // One adapter per distinct provider even when a panel names the same
        // provider twice with different models: the registry is keyed by
        // ProviderId, and re-registering would just replace the same thing.
        if providers.get(&entry.provider).is_none() {
            let (secret, _source) = sources
                .iter()
                .find_map(|s| s.resolve(&entry.provider))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no API key for `{}`. Set ARBITER_{}_API_KEY (or that provider's own \
                         variable), or run `arbiter keys set {}`. Use `--panel mock` to run \
                         without any keys.",
                        entry.provider,
                        entry.provider.as_str().to_uppercase(),
                        entry.provider
                    )
                })?;
            providers.register(build_provider(&entry.provider, secret)?);
        }
        panel.push((entry.model.clone(), entry.provider.clone()));
    }

    // The judge rides the first provider in the panel. No spec section names
    // a judge-selection rule for an operator-supplied panel (ARCHITECTURE §10
    // describes judging, not who judges), so this is the least surprising
    // default: a panel of one provider judges with itself, and a mixed panel
    // judges with the one the operator listed first.
    let first = entries
        .first()
        .ok_or_else(|| anyhow::anyhow!("--panel resolved to no entries"))?;
    let judges = vec![(first.model.clone(), first.provider.clone())];

    Ok(ResolvedPanel {
        panel,
        judges,
        providers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_provider_takes_its_default_model() {
        let entries = parse_spec("anthropic").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].provider.as_str(), "anthropic");
        assert_eq!(
            entries[0].model,
            arbiter_providers::anthropic::default_model()
        );
    }

    #[test]
    fn a_pinned_model_overrides_the_default() {
        let entries = parse_spec("openai:gpt-4o-mini").unwrap();
        assert_eq!(entries[0].model.as_str(), "gpt-4o-mini");
        assert_eq!(entries[0].provider.as_str(), "openai");
    }

    #[test]
    fn several_providers_keep_the_order_they_were_listed_in() {
        let entries = parse_spec("gemini, anthropic ,deepseek").unwrap();
        let ids: Vec<&str> = entries.iter().map(|e| e.provider.as_str()).collect();
        assert_eq!(ids, vec!["gemini", "anthropic", "deepseek"]);
    }

    /// A panel is a list of *models*, not of providers. Five entries naming
    /// one provider are five panel members on one adapter -- the only way an
    /// operator with two working keys reaches a five-model panel at all, and
    /// the reason `resolve` registers per distinct provider while pushing per
    /// entry. `PositionId` is `pos_{provider}_{model}`, so these stay five
    /// distinct positions rather than one overwritten four times.
    #[test]
    fn one_provider_can_seat_several_models() {
        let spec = "openrouter:deepseek/deepseek-chat,\
                    openrouter:meta-llama/llama-3.3-70b-instruct,\
                    openrouter:qwen/qwen-2.5-72b-instruct,\
                    openrouter:mistralai/mistral-small-3.2-24b-instruct,\
                    groq:llama-3.3-70b-versatile";
        let entries = parse_spec(spec).unwrap();
        assert_eq!(entries.len(), 5, "{entries:?}");
        let models: Vec<&str> = entries.iter().map(|e| e.model.as_str()).collect();
        assert_eq!(
            models
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            5,
            "every entry must be its own model: {models:?}"
        );
        assert_eq!(entries[0].provider.as_str(), "openrouter");
        assert_eq!(entries[4].provider.as_str(), "groq");
    }

    /// A vendor model id may itself contain a colon (OpenRouter's free tier
    /// is `vendor/model:free`), so the split is on the *first* colon only.
    #[test]
    fn a_model_id_may_contain_a_colon_of_its_own() {
        let entries = parse_spec("openrouter:deepseek/deepseek-chat-v3-0324:free").unwrap();
        assert_eq!(entries[0].provider.as_str(), "openrouter");
        assert_eq!(
            entries[0].model.as_str(),
            "deepseek/deepseek-chat-v3-0324:free"
        );
    }

    #[test]
    fn an_unknown_provider_is_refused_and_lists_the_real_ones() {
        let err = parse_spec("bard").unwrap_err().to_string();
        assert!(err.contains("bard"), "{err}");
        assert!(err.contains("anthropic"), "{err}");
    }

    #[test]
    fn an_empty_panel_is_refused() {
        assert!(parse_spec("").is_err());
        assert!(parse_spec("  , ,").is_err());
    }

    #[test]
    fn mock_still_resolves_offline_with_its_three_models_and_one_judge() {
        let resolved = resolve("mock").unwrap();
        assert_eq!(resolved.panel.len(), 3);
        assert_eq!(resolved.judges.len(), 1);
        assert_eq!(resolved.providers.len(), 1);
        assert!(resolved.providers.get(&ProviderId::new("mock")).is_some());
    }

    #[test]
    fn mock_counts_as_its_whole_roster_and_a_real_provider_as_one_model() {
        assert_eq!(
            models_contributed_by(&ProviderId::new("mock")),
            resolve("mock").unwrap().panel.len()
        );
        assert_eq!(models_contributed_by(&ProviderId::new("anthropic")), 1);
    }

    #[test]
    fn a_real_provider_without_a_key_names_what_to_do_about_it() {
        // No credential is set for this provider in the test environment, so
        // resolution must fail with an actionable message rather than
        // silently dropping the model from the panel.
        let err = resolve("anthropic:claude-sonnet-4-5")
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains("anthropic"), "{err}");
        assert!(err.contains("ARBITER_ANTHROPIC_API_KEY"), "{err}");
        assert!(
            err.contains("--panel mock"),
            "must offer the offline path: {err}"
        );
    }
}
