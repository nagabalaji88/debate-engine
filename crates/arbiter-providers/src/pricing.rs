//! Published list prices, for showing an operator what a call cost.
//!
//! **These numbers are hand-maintained and will drift.** Vendors change
//! prices, and a model pinned with `provider:model` may not be the one priced
//! here at all. Everything this module returns is therefore an *estimate for
//! display*, and it is deliberately not wired into [`crate`]'s budget path:
//! ARCHITECTURE §8.3's ledger is authoritative about money, reconciled against
//! a vendor's own usage export, and a stale constant in this file must never
//! be able to move a reservation. `arbiter compare` shows these figures so a
//! reader can tell a 30¢ answer from a 3¢ one; the run's own accounting does
//! not consult them.
//!
//! Prices are per million tokens, USD, for each provider's default model
//! (`default_model_for`). A provider absent from the table returns `None` —
//! "we don't know" is an honest answer and renders as a blank cell, where a
//! silent `0.0` would read as "this one was free".

use arbiter_core::ProviderId;

/// One provider's published rate, per million tokens, in USD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pricing {
    pub input_per_1m_usd: f64,
    pub output_per_1m_usd: f64,
}

impl Pricing {
    /// What a call at this rate cost, in USD. `f64` throughout: these are
    /// display figures accurate to a few significant digits at best, and
    /// pretending otherwise with a decimal type would imply a precision the
    /// underlying list price does not have.
    pub fn cost_of(&self, prompt_tokens: u64, completion_tokens: u64) -> f64 {
        (prompt_tokens as f64 / 1_000_000.0) * self.input_per_1m_usd
            + (completion_tokens as f64 / 1_000_000.0) * self.output_per_1m_usd
    }
}

/// The list price for a provider's default model, or `None` when this table
/// has no entry — including for `mock`, which never costs anything and whose
/// zero is a fact rather than a missing number, but which also never reports
/// tokens, so the distinction never reaches a reader.
pub fn pricing_for(provider: &ProviderId) -> Option<Pricing> {
    let (input_per_1m_usd, output_per_1m_usd) = match provider.as_str() {
        // claude-sonnet-4-5
        "anthropic" => (3.0, 15.0),
        // gpt-4o
        "openai" => (2.5, 10.0),
        // gemini-3.6-flash
        "gemini" => (0.10, 0.40),
        // grok-2-latest
        "xai" => (2.0, 10.0),
        // deepseek-chat
        "deepseek" => (0.27, 1.10),
        // `openrouter` and `groq` are deliberately absent: an aggregator's
        // price is whatever the routed model costs, so there is no single rate
        // to publish here. `None` renders as "cost unknown", which is honest;
        // inventing an average would put a wrong number on every answer.
        _ => return None,
    };
    Some(Pricing {
        input_per_1m_usd,
        output_per_1m_usd,
    })
}

/// What a call actually cost, or `None` when this table cannot say.
///
/// `None` is the important half. It is returned when the provider publishes no
/// single rate (an aggregator routes to whichever upstream model it likes),
/// when the model asked for is not the one [`pricing_for`] quotes, and when the
/// response carried no token counts at all. Each of those is "we do not know",
/// and the ledger records them as unmeasured rather than inventing a figure —
/// a wrong number shown with the same confidence as a right one is the failure
/// this whole module is trying to avoid.
///
/// The model check matters more than it looks: `pricing_for` quotes each
/// provider's *default* model, so `anthropic:claude-haiku` priced at Sonnet's
/// rate would overstate a cheap call several-fold. A panel that names its
/// models explicitly is the normal case, not the exception.
pub fn measured_cost(
    provider: &ProviderId,
    model: &arbiter_core::ModelId,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> Option<f64> {
    if prompt_tokens == 0 && completion_tokens == 0 {
        return None;
    }
    let default = crate::default_model_for(provider)?;
    if default.as_str() != model.as_str() {
        return None;
    }
    Some(pricing_for(provider)?.cost_of(prompt_tokens, completion_tokens))
}

/// Stamp a parsed response with what it cost, where that is knowable.
///
/// Called by each adapter at the end of `call`, where both the model that was
/// asked and the usage that came back are in scope. Leaves `cost_usd` as
/// `None` when [`measured_cost`] cannot say.
pub fn price_response(
    provider: &ProviderId,
    model: &arbiter_core::ModelId,
    mut response: arbiter_kernel::provider::ProviderResponse,
) -> arbiter_kernel::provider::ProviderResponse {
    response.cost_usd = measured_cost(
        provider,
        model,
        response.prompt_tokens,
        response.completion_tokens,
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every provider that bills at one published rate has one here. The
    /// aggregators do not, and must not: their cost depends on which upstream
    /// model a request was routed to, and a made-up average would be a wrong
    /// number shown with the same confidence as a right one.
    #[test]
    fn every_direct_provider_has_a_published_price() {
        const AGGREGATORS: [&str; 2] = ["openrouter", "groq"];
        for id in crate::REAL_PROVIDER_IDS {
            let priced = pricing_for(&ProviderId::new(id)).is_some();
            if AGGREGATORS.contains(&id) {
                assert!(!priced, "{id} routes to other vendors — it has no one rate");
            } else {
                assert!(priced, "{id} can be run but has no price to show for it");
            }
        }
    }

    #[test]
    fn an_unknown_provider_has_no_price_rather_than_a_free_one() {
        assert!(pricing_for(&ProviderId::new("mock")).is_none());
        assert!(pricing_for(&ProviderId::new("bard")).is_none());
    }

    #[test]
    fn cost_is_input_and_output_priced_separately() {
        let p = Pricing {
            input_per_1m_usd: 3.0,
            output_per_1m_usd: 15.0,
        };
        // 1M in, 1M out — the two rates must not be averaged or conflated.
        assert!((p.cost_of(1_000_000, 1_000_000) - 18.0).abs() < 1e-9);
        // Output is the expensive half, so the same token count costs more
        // as completion than as prompt.
        assert!(p.cost_of(0, 1000) > p.cost_of(1000, 0));
    }

    #[test]
    fn a_call_that_reported_no_tokens_costs_nothing_rather_than_panicking() {
        let p = pricing_for(&ProviderId::new("anthropic")).unwrap();
        assert_eq!(p.cost_of(0, 0), 0.0);
    }
    #[test]
    fn a_model_this_table_does_not_quote_has_no_measured_cost() {
        let anthropic = ProviderId::new("anthropic");
        let priced = crate::default_model_for(&anthropic).unwrap();
        assert!(measured_cost(&anthropic, &priced, 1000, 1000).is_some());
        // Sonnet's rate must not be charged to a Haiku call.
        assert!(
            measured_cost(
                &anthropic,
                &arbiter_core::ModelId::new("claude-haiku-4-5"),
                1000,
                1000
            )
            .is_none()
        );
    }

    #[test]
    fn an_aggregator_reports_no_measured_cost_however_many_tokens_it_used() {
        let openrouter = ProviderId::new("openrouter");
        let model = crate::default_model_for(&openrouter).unwrap();
        assert!(measured_cost(&openrouter, &model, 5000, 5000).is_none());
    }

    /// A response that carried no usage block is unknown, not free. Reporting
    /// zero would let an unmetered provider look like the cheapest one.
    #[test]
    fn a_response_with_no_token_counts_is_unknown_rather_than_zero() {
        let anthropic = ProviderId::new("anthropic");
        let model = crate::default_model_for(&anthropic).unwrap();
        assert_eq!(measured_cost(&anthropic, &model, 0, 0), None);
    }
}
