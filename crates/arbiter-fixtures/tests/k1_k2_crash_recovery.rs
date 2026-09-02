//! F2 — provider-call crash recovery (K1/K2), ARCHITECTURE §18's CI suite.

use arbiter_kernel::budget::BudgetLedger;
use arbiter_kernel::calls::{ResumeAction, classify_on_resume};
use arbiter_kernel::ids::ReservationId;
use arbiter_kernel::provider::CallState;
use arbiter_kernel::store::Cost;

/// `crash_midcall`: "CALL_STARTED with no completion → reopened as ORPHANED,
/// never FAILED." A crash after `CALL_STARTED` committed (the reservation's
/// last committed state is `Sent`, per `ReservationGuard::mark_sent`) means
/// the request may already have reached the provider and possibly been
/// billed — resume must hold it as orphaned, and ARCHITECTURE §8.4's own
/// transition diagram (`arbiter-kernel/src/calls.rs::validate_transition`)
/// forbids `Orphaned -> Failed` outright, so it can never later be
/// reclassified away.
#[test]
fn crash_midcall() {
    assert_eq!(
        classify_on_resume(CallState::Sent),
        ResumeAction::HoldOrphaned
    );
    assert!(
        arbiter_kernel::calls::validate_transition(CallState::Orphaned, CallState::Failed).is_err(),
        "an orphaned call must never transition to Failed, even after this fixture's own resume path holds it"
    );
}

/// `crash_before_send`: "killed between RESERVED and SENT → FAILED,
/// reservation released, not orphaned." If the process dies before
/// `CALL_STARTED` ever committed, the request never left this machine — the
/// last committed state is `Reserved`, and resume both releases the
/// reservation and marks the call `Failed`, free to retry, never held as an
/// orphan (nothing could have been billed for a request that was never
/// sent).
#[test]
fn crash_before_send() {
    assert_eq!(
        classify_on_resume(CallState::Reserved),
        ResumeAction::ReleaseAndFail
    );

    // The live ledger side of the same scenario: a guard that only ever
    // reached Reserved, dropped without being sent, must actually release
    // its reservation (not just report the action a resume path *should*
    // take).
    let budget = BudgetLedger::new(Some(Cost(10.0)));
    {
        let _guard = budget.reserve(ReservationId::new("r1"), Cost(1.0)).unwrap();
        // Crash here: dropped before mark_sent() or commit() ever ran.
    }
    assert_eq!(
        budget.reserved(),
        Cost(0.0),
        "the reservation must be released on drop, not left outstanding"
    );
    assert_eq!(
        budget.committed(),
        Cost(0.0),
        "nothing was ever billed for a call that never sent"
    );
}

/// `budget_reconciliation`: "budget.reserved matches the sum over
/// non-terminal calls after a kill at each state." Four reservations, each
/// crashed (or completed) at a different point in the state machine:
/// `Reserved`-then-dropped (released, terminal `Failed`), `Sent`-then-crashed
/// (held, non-terminal `Orphaned`-bound), `Acknowledged`-then-crashed (held,
/// non-terminal), and a normal `Completed` commit (terminal). `reserved()`
/// must count only the two calls still genuinely outstanding.
#[test]
fn budget_reconciliation() {
    let budget = BudgetLedger::new(Some(Cost(100.0)));

    // 1. Reserved, then dropped -- released.
    {
        let _g = budget.reserve(ReservationId::new("r1"), Cost(1.0)).unwrap();
    }

    // 2. Reserved + sent, then "crashed" (the guard is forgotten rather than
    // dropped normally, standing in for a process kill that never runs Drop
    // -- the reservation must stay held, exactly as an orphaned call would).
    let g2 = budget.reserve(ReservationId::new("r2"), Cost(2.0)).unwrap();
    g2.mark_sent();
    std::mem::forget(g2);

    // 3. Reserved + sent + acknowledged, then crashed the same way.
    let g3 = budget.reserve(ReservationId::new("r3"), Cost(3.0)).unwrap();
    g3.mark_sent();
    g3.mark_acknowledged();
    std::mem::forget(g3);

    // 4. A normal, fully completed call.
    let g4 = budget.reserve(ReservationId::new("r4"), Cost(4.0)).unwrap();
    g4.mark_sent();
    g4.mark_acknowledged();
    g4.commit(Cost(4.0));

    assert_eq!(
        budget.reserved(),
        Cost(5.0),
        "only r2 (Sent) and r3 (Acknowledged) are still non-terminal: 2.0 + 3.0"
    );
    assert_eq!(
        budget.committed(),
        Cost(4.0),
        "only r4 actually completed and committed"
    );
}
