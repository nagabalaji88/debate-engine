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
        // gemini-2.0-flash
        "gemini" => (0.10, 0.40),
        // grok-2-latest
        "xai" => (2.0, 10.0),
        // deepseek-chat
        "deepseek" => (0.27, 1.10),
        _ => return None,
    };
    Some(Pricing {
        input_per_1m_usd,
        output_per_1m_usd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_real_provider_has_a_published_price() {
        for id in crate::REAL_PROVIDER_IDS {
            assert!(
                pricing_for(&ProviderId::new(id)).is_some(),
                "{id} can be run but has no price to show for it"
            );
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
}
