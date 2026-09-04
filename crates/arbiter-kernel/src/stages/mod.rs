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
use crate::ids::{ReservationId, StageName};
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
