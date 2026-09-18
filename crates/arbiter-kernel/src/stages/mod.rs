//! Concrete `Stage` implementations, one module per pipeline stage
//! (ARCHITECTURE §5). `stage.rs` stays pure infrastructure (the `Stage` trait,
//! `StageContext`, the idempotency-key formula); this module tree is where each
//! `G2`–`G9` task lands its own stage as it is implemented.

pub mod challenge_plan;
pub mod challenge_run;
pub mod claims_extract;
pub mod claims_normalize;
pub mod controller_decide;
pub mod decision_synthesize;
pub mod disputes_rank;
pub mod judge_evaluate;
pub mod options_cluster;
pub mod positions_generate;
pub mod rebuttal_run;
pub mod relations_analyze;
pub(crate) mod similarity;

use crate::event::EventType;
use crate::ids::{CallId, ReservationId, StageName};
use crate::stage::StageContext;
use crate::store::Cost;

/// `BUDGET_RELEASED` — "when `reserved` falls without `committed` rising"
/// (ARCHITECTURE §8.3). [`crate::budget::ReservationGuard`]'s `Drop` performs
/// the ledger half of that, but it holds no event sink, so the release never
/// reached the transcript: every provider stage showed a `BUDGET_RESERVED`
/// with nothing to close it, and the provider's own error message — the one
/// thing that explains why the call produced nothing — was thrown away at the
/// `.ok()?`. Every path that abandons a reservation calls this on its way out.
///
/// ARCHITECTURE §8.4 gives the FAILED call state no event of its own (its
/// Event column is "—"), and INTERFACES §13's `EventType` is authoritative, so
/// the reason rides this event's payload rather than a new variant.
pub(crate) fn emit_budget_released(
    ctx: &StageContext<'_>,
    stage: &StageName,
    reservation_id: &ReservationId,
    released: Cost,
    reason: &str,
) {
    ctx.events.emit(
        EventType::BudgetReleased,
        stage,
        serde_json::json!({
            "reservation_id": reservation_id.as_str(),
            "released": released.0,
            "reason": reason,
        }),
    );
}

/// What to commit for a finished call, and whether that figure was measured.
///
/// A provider that reports its usage and whose model this build can price
/// yields a real amount. Everything else -- an aggregator, an unpriced model,
/// a response with no usage block -- yields the reservation estimate, flagged
/// as unmeasured so nothing downstream can present it as a billed figure.
///
/// This is the whole of the honesty fix: every stage used to commit
/// `estimated_cost_per_call` unconditionally, which made the run's "$" total a
/// call count wearing a currency symbol. Real usage is now used where it
/// exists, and where it does not, the estimate is still the best guess
/// available -- it just no longer claims to be more than that.
pub(crate) fn settled_cost(
    response: &crate::provider::ProviderResponse,
    estimate: crate::store::Cost,
) -> (crate::store::Cost, bool) {
    match response.cost_usd {
        Some(measured) => (crate::store::Cost(measured), true),
        None => (estimate, false),
    }
}

/// Settle the reservation for a call that failed *after the request went out*.
///
/// The distinction this draws is the whole point: a provider that answers with
/// a refusal (an HTTP status -- 401, 429, 400) has told us it did not run the
/// completion, so its reservation is free money and goes back. A transport
/// failure has told us nothing. A timeout, a reset connection, a body that
/// stopped arriving half-way: any of those can happen *after* the provider
/// accepted the request and started billing it, and releasing the reservation
/// would put money the vendor may well charge for back into the run's budget
/// to be spent a second time.
///
/// So ambiguous failures orphan instead, which holds the reservation
/// (`CallState::Orphaned` counts as non-terminal, so `reserved()` keeps
/// counting it) until an operator reconciles the run against a usage export.
/// `CALL_ORPHANED` was defined for exactly this and had no caller: every error
/// released, and "we do not know whether this was billed" was recorded as "it
/// was not".
///
/// Returns whether the reservation was orphaned rather than released.
pub(crate) fn settle_failed_call(
    ctx: &StageContext<'_>,
    stage: &StageName,
    guard: crate::budget::ReservationGuard<'_>,
    call_id: &CallId,
    reservation_id: &ReservationId,
    estimate: Cost,
    error: &crate::provider::ProviderError,
) -> bool {
    let reason = error.to_string();
    match error {
        // The provider answered. Whatever it said, it said it instead of
        // running the model.
        crate::provider::ProviderError::Http { .. } => {
            drop(guard); // releases on drop
            emit_budget_released(ctx, stage, reservation_id, estimate, &reason);
            false
        }
        crate::provider::ProviderError::Other(_) => {
            guard.mark_orphaned();
            ctx.events.emit(
                EventType::CallOrphaned,
                stage,
                serde_json::json!({
                    "call_id": call_id.as_str(),
                    "reservation_id": reservation_id.as_str(),
                    "held": estimate.0,
                    "reason": reason,
                }),
            );
            true
        }
    }
}
