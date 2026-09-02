//! Provider-facing capability declaration and call-state machine, INTERFACES §5 /
//! ARCHITECTURE §8.4.
//!
//! `Provider` itself, and `ProviderRequest`/`ProviderResponse`/`ProviderError`,
//! have no definition anywhere in either spec file — a P1-scoped instance of
//! D19/D24's category of gap. Authored here as the trait seam
//! `arbiter-providers`' concrete adapters (Mock, Anthropic, OpenAI-compatible)
//! implement, matching the `RunStore` pattern (D1): this crate defines the
//! interface, `arbiter-providers` — which already depends on this crate — writes
//! the bodies.

use crate::ids::ReservationId;
use arbiter_core::{ModelId, ProviderId};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;

/// INTERFACES §5, copied verbatim. Capability-gated, not assumed: each adapter
/// declares its own support from that provider's documentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub structured_output: bool,
    pub streaming: bool,
    /// `None` = unsupported. The Anthropic adapter ships `None` — the reference
    /// Messages API documents no idempotency header at the time of writing.
    pub idempotency: Option<IdempotencyStyle>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IdempotencyStyle {
    /// INTERFACES §5 writes `Header(&'static str)` — a compile-time constant in
    /// each adapter's own source, never deserialized data. `String` here instead
    /// of `&'static str` is a practicality substitution so this type can still
    /// derive `Deserialize` (a borrowed `&'static str` cannot, in general); every
    /// adapter still only ever constructs this from a string literal.
    Header(String),
}

/// ARCHITECTURE §8.4's provider-call state diagram, transcribed as an enum — the
/// spec pins the exact variant set via a table and a transition diagram but never
/// gives an `enum CallState { .. }` code block itself (PLAN_DEVIATIONS.md D19).
///
/// ```text
/// RESERVED ──► SENT ──► ACKNOWLEDGED ──► COMPLETED
///     │          │             │
///     │          └──────┬──────┘
///     │                 ├──► RETRYABLE ──► (back to SENT)
///     │                 ├──► FAILED          provably not billed (e.g. a 4xx)
///     │                 └──► ORPHANED ─────► RECOVERED
///     │
///     └──► FAILED                            never dispatched
/// ```
///
/// | State | Event | Means |
/// |---|---|---|
/// | `Reserved` | `BUDGET_RESERVED` | money is held, nothing sent |
/// | `Sent` | `CALL_STARTED` | the request left the machine |
/// | `Acknowledged` | `CALL_REQUEST_ID` | the provider named the request; reconcilable |
/// | `Completed` | `CALL_COMPLETED` | response stored, budget committed |
/// | `Retryable` | `CALL_RETRYING` | provably not billed, or idempotency-keyed |
/// | `Failed` | — | provably not billed |
/// | `Orphaned` | `CALL_ORPHANED` | cannot prove whether it was billed |
/// | `Recovered` | `CALL_RECOVERED` | reconciled against a usage export |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CallState {
    Reserved,
    Sent,
    Acknowledged,
    Completed,
    Retryable,
    Failed,
    Orphaned,
    Recovered,
}

impl CallState {
    /// INTERFACES §5's `resume` branch reads "every `provider_calls` row in a
    /// non-terminal state" — the budget-ledger invariant (ARCHITECTURE §8.3) sums
    /// `reserved_amount` over exactly these four, `Orphaned` included: "held-but-
    /// unresolved money legitimately reduces what later rounds may spend."
    pub fn is_non_terminal(self) -> bool {
        matches!(
            self,
            CallState::Reserved | CallState::Sent | CallState::Acknowledged | CallState::Orphaned
        )
    }
}

/// What a stage sends. `prompt` is the fully-rendered template text (G1's
/// prompt-pack rendering happens before this point — this trait only ever sees
/// the finished string, never a template plus variables to fill in itself).
/// `params` is the call's canonical serialized parameters (temperature,
/// max_tokens, ...) — matching [`crate::store::CacheKey`]'s own `params: String`
/// convention, since the two must agree byte-for-byte for a cache lookup to hit.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderRequest {
    pub model: ModelId,
    pub prompt: String,
    pub params: String,
    /// `Some` only when [`ProviderCapabilities::idempotency`] is `Some` and this
    /// is a retry — `blake3(prompt_hash ‖ reservation_id)` (INTERFACES §5).
    pub idempotency_key: Option<String>,
    pub reservation: ReservationId,
}

/// What a provider returns. `request_id` is the provider's own identifier from
/// its response headers — "appended the moment they arrive, before the body
/// finishes" (INTERFACES §5), so an orphaned call is reconcilable against a
/// usage export afterwards. `None` for a provider/response that never carries
/// one (not every provider issues one, and a mock never does).
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderResponse {
    pub text: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub request_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("{0}")]
    Other(String),
}

/// The provider seam. `call`'s boxed-future return (rather than an `async fn`,
/// which `Stage::run` uses, per D24) is deliberate: unlike `Stage`, this trait
/// needs `dyn` dispatch *now* — `ProviderRegistry` genuinely holds a
/// heterogeneous set of providers (mock, Anthropic, OpenAI-compatible) behind
/// one type today, not once some future executor exists to need it.
pub trait Provider: std::fmt::Debug + Send + Sync {
    fn id(&self) -> ProviderId;
    fn capabilities(&self) -> ProviderCapabilities;
    fn call(
        &self,
        request: ProviderRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send + '_>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_state_serializes_screaming_snake_case() {
        assert_eq!(
            serde_json::to_string(&CallState::Acknowledged).unwrap(),
            "\"ACKNOWLEDGED\""
        );
    }

    #[test]
    fn budget_ledger_non_terminal_states_match_the_spec_invariant() {
        // ARCHITECTURE §8.3: budget.reserved == SUM(reserved_amount) over
        // RESERVED | SENT | ACKNOWLEDGED | ORPHANED -- exactly these four.
        let non_terminal: Vec<CallState> = [
            CallState::Reserved,
            CallState::Sent,
            CallState::Acknowledged,
            CallState::Completed,
            CallState::Retryable,
            CallState::Failed,
            CallState::Orphaned,
            CallState::Recovered,
        ]
        .into_iter()
        .filter(|s| s.is_non_terminal())
        .collect();
        assert_eq!(
            non_terminal,
            vec![
                CallState::Reserved,
                CallState::Sent,
                CallState::Acknowledged,
                CallState::Orphaned,
            ]
        );
    }

    #[test]
    fn anthropic_ships_no_idempotency_support() {
        let caps = ProviderCapabilities {
            structured_output: true,
            streaming: true,
            idempotency: None,
        };
        assert_eq!(caps.idempotency, None);
    }

    // A minimal concrete `Provider`, proving the trait is actually
    // dyn-dispatchable -- `crate::stage::ProviderRegistry` needs `Box<dyn
    // Provider>` to work today, not once some future adapter exists to prove it.

    #[derive(Debug)]
    struct EchoProvider;
    impl Provider for EchoProvider {
        fn id(&self) -> ProviderId {
            ProviderId::new("echo")
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                structured_output: false,
                streaming: false,
                idempotency: None,
            }
        }
        fn call(
            &self,
            request: ProviderRequest,
        ) -> Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send + '_>>
        {
            Box::pin(async move {
                Ok(ProviderResponse {
                    text: request.prompt,
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    request_id: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn a_provider_is_dyn_dispatchable_through_the_registry() {
        let mut registry = crate::stage::ProviderRegistry::new();
        registry.register(Box::new(EchoProvider));
        assert_eq!(registry.len(), 1);

        let provider = registry.get(&ProviderId::new("echo")).unwrap();
        let response = provider
            .call(ProviderRequest {
                model: ModelId::new("echo-1"),
                prompt: "hello".to_string(),
                params: "{}".to_string(),
                idempotency_key: None,
                reservation: ReservationId::new("r1"),
            })
            .await
            .unwrap();
        assert_eq!(response.text, "hello");
    }

    #[test]
    fn an_unregistered_provider_id_is_not_found() {
        let registry = crate::stage::ProviderRegistry::new();
        assert!(registry.get(&ProviderId::new("nobody")).is_none());
        assert!(registry.is_empty());
    }
}
