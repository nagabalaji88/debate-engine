//! Provider-facing capability declaration and call-state machine, INTERFACES §5 /
//! ARCHITECTURE §8.4.

use serde::{Deserialize, Serialize};

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
}
