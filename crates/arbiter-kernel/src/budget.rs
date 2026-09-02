//! The budget ledger: reservation protocol, ARCHITECTURE §7 / §8.3 / §11.
//!
//! `reserve()` returns a guard whose `Drop` releases the unused remainder — a
//! call that never got as far as `commit()` (an early return, a panic caught
//! upstream, cancellation) must not leak reserved money forever. `reserved()` is
//! never tracked as an independently-updated running total: it is always
//! *computed* as the sum of `reserved_amount` over calls still in a non-terminal
//! state (`CallState::is_non_terminal`), which is the ARCHITECTURE §8.3 invariant
//! by construction rather than something that can drift out of sync with it.
//!
//! `&self`, not `&mut self`, throughout: `StageContext` (INTERFACES §6) holds
//! `budget: &'a BudgetLedger` as a shared reference, because concurrent stages
//! (`Parallelism::PerItem`) reserve against the same ledger at once — interior
//! mutability (a `Mutex`) is what makes that sound without `unsafe`.

use crate::ids::ReservationId;
use crate::provider::CallState;
use crate::store::Cost;
use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetExhausted {
    pub requested: Cost,
    pub available: Cost,
}

#[derive(Debug, Clone, Copy)]
struct ReservedCall {
    reserved_amount: f64,
    state: CallState,
}

#[derive(Debug, Default)]
struct LedgerState {
    committed: f64,
    calls: BTreeMap<ReservationId, ReservedCall>,
}

#[derive(Debug)]
pub struct BudgetLedger {
    /// The hard cap (ARCHITECTURE §11: "hard cap aborts the run rather than
    /// exceeding it"). `None` means unbounded — useful for the tests here and
    /// for callers that enforce the cap elsewhere.
    available: Option<f64>,
    inner: Mutex<LedgerState>,
}

impl BudgetLedger {
    pub fn new(available: Option<Cost>) -> Self {
        Self {
            available: available.map(|c| c.0),
            inner: Mutex::new(LedgerState::default()),
        }
    }

    pub fn unbounded() -> Self {
        Self::new(None)
    }

    /// `Σ reserved_amount` over calls in `RESERVED | SENT | ACKNOWLEDGED |
    /// ORPHANED` — the ARCHITECTURE §8.3 ledger invariant, always true by
    /// construction here (it is what this sum *is*, not a separately maintained
    /// number that could disagree with it).
    pub fn reserved(&self) -> Cost {
        let state = self.inner.lock().unwrap();
        Cost(
            state
                .calls
                .values()
                .filter(|c| c.state.is_non_terminal())
                .map(|c| c.reserved_amount)
                .sum(),
        )
    }

    pub fn committed(&self) -> Cost {
        Cost(self.inner.lock().unwrap().committed)
    }

    /// `available − reserved − committed`. `None` (unbounded) has no meaningful
    /// remaining figure; callers checking affordability should use
    /// [`Self::reserve`]'s own `Result` instead of pre-checking this.
    pub fn remaining(&self) -> Option<Cost> {
        let available = self.available?;
        let state = self.inner.lock().unwrap();
        let reserved: f64 = state
            .calls
            .values()
            .filter(|c| c.state.is_non_terminal())
            .map(|c| c.reserved_amount)
            .sum();
        Some(Cost(available - reserved - state.committed))
    }

    /// §8.3's reserve transaction (`BUDGET_RESERVED` + a `provider_calls` row in
    /// `RESERVED`), minus the event/row emission — that's the caller's job, once
    /// wired to a real `Tx` (this crate has no store dependency, D1). An
    /// unsatisfiable reservation is refused, matching §7: "an unsatisfiable
    /// reservation fails the call and the controller sees
    /// `StopReason::BudgetExhausted`" — `StopReason` itself is the controller's
    /// type to construct (G7), not this ledger's.
    pub fn reserve(
        &self,
        id: ReservationId,
        estimate: Cost,
    ) -> Result<ReservationGuard<'_>, BudgetExhausted> {
        let mut state = self.inner.lock().unwrap();
        if let Some(available) = self.available {
            let reserved: f64 = state
                .calls
                .values()
                .filter(|c| c.state.is_non_terminal())
                .map(|c| c.reserved_amount)
                .sum();
            if reserved + state.committed + estimate.0 > available {
                return Err(BudgetExhausted {
                    requested: estimate,
                    available: Cost(available - reserved - state.committed),
                });
            }
        }
        state.calls.insert(
            id.clone(),
            ReservedCall {
                reserved_amount: estimate.0,
                state: CallState::Reserved,
            },
        );
        drop(state);
        Ok(ReservationGuard {
            ledger: self,
            id,
            settled: false,
        })
    }

    fn set_state(&self, id: &ReservationId, new_state: CallState) {
        let mut state = self.inner.lock().unwrap();
        if let Some(call) = state.calls.get_mut(id) {
            call.state = new_state;
        }
    }

    /// §8.3's commit transaction: `reserved -= estimate, committed += actual`.
    /// Returns the `released_remainder` (`estimate − actual`, floored at 0) that
    /// `BUDGET_COMMITTED` carries — never its own second `BUDGET_RELEASED` event,
    /// which would let a naive consumer summing release events double-count the
    /// refund (§8.3's own stated reason).
    fn commit(&self, id: &ReservationId, actual: Cost) -> Cost {
        let mut state = self.inner.lock().unwrap();
        let reserved_amount = state
            .calls
            .get(id)
            .map(|c| c.reserved_amount)
            .unwrap_or(0.0);
        state.committed += actual.0;
        if let Some(call) = state.calls.get_mut(id) {
            call.state = CallState::Completed;
        }
        Cost((reserved_amount - actual.0).max(0.0))
    }

    /// Releases a reservation that will never be committed — `BUDGET_RELEASED`
    /// fires *only* here, "when `reserved` falls without `committed` rising"
    /// (§8.3), never alongside a commit.
    fn release(&self, id: &ReservationId) {
        self.set_state(id, CallState::Failed);
    }
}

/// Held while a provider call is in flight. `Drop` releases the reservation if
/// neither [`Self::commit`] nor [`Self::mark_orphaned`] settled it first — the
/// guard's whole reason to exist (ARCHITECTURE §7).
#[derive(Debug)]
pub struct ReservationGuard<'a> {
    ledger: &'a BudgetLedger,
    id: ReservationId,
    settled: bool,
}

impl ReservationGuard<'_> {
    pub fn id(&self) -> &ReservationId {
        &self.id
    }

    /// `CALL_STARTED` committed — the request left the machine.
    pub fn mark_sent(&self) {
        self.ledger.set_state(&self.id, CallState::Sent);
    }

    /// `CALL_REQUEST_ID` committed — the provider named the request.
    pub fn mark_acknowledged(&self) {
        self.ledger.set_state(&self.id, CallState::Acknowledged);
    }

    /// The call completed. Settles the guard (no release on drop) and returns
    /// the released remainder for the caller's `BUDGET_COMMITTED` event.
    pub fn commit(mut self, actual: Cost) -> Cost {
        self.settled = true;
        self.ledger.commit(&self.id, actual)
    }

    /// The call may have been billed but its response never arrived
    /// (INTERFACES §5). Settles the guard *without* releasing — an orphaned
    /// reservation stays held until an operator or usage-export reconciliation
    /// resolves it (`CallState::is_non_terminal` counts `Orphaned`).
    pub fn mark_orphaned(mut self) {
        self.settled = true;
        self.ledger.set_state(&self.id, CallState::Orphaned);
    }
}

impl Drop for ReservationGuard<'_> {
    fn drop(&mut self) {
        if !self.settled {
            self.ledger.release(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid(s: &str) -> ReservationId {
        ReservationId::new(s)
    }

    /// Never `==` on money: `0.10 + 0.20 != 0.30` in `f64`.
    fn assert_cost_eq(actual: Cost, expected: f64) {
        assert!(
            (actual.0 - expected).abs() < 1e-9,
            "expected {expected}, got {}",
            actual.0
        );
    }

    /// One "kill" simulated at each of the four states a resume can find a call
    /// in (ARCHITECTURE §8.4's table) — at every point, `reserved()` must equal
    /// the sum of `reserved_amount` over exactly `RESERVED | SENT | ACKNOWLEDGED
    /// | ORPHANED`, never drifting regardless of how far the call got.
    #[test]
    fn ledger_invariant_holds_after_kill_at_each_state() {
        let ledger = BudgetLedger::unbounded();

        // Killed right after reserve(): still RESERVED.
        let g1 = ledger.reserve(rid("r1"), Cost(0.10)).unwrap();
        assert_cost_eq(ledger.reserved(), 0.10);
        std::mem::forget(g1); // simulate: process died, guard never drops

        // Killed after CALL_STARTED: SENT.
        let g2 = ledger.reserve(rid("r2"), Cost(0.20)).unwrap();
        g2.mark_sent();
        assert_cost_eq(ledger.reserved(), 0.30);
        std::mem::forget(g2);

        // Killed after CALL_REQUEST_ID: ACKNOWLEDGED.
        let g3 = ledger.reserve(rid("r3"), Cost(0.15)).unwrap();
        g3.mark_sent();
        g3.mark_acknowledged();
        assert_cost_eq(ledger.reserved(), 0.45);
        std::mem::forget(g3);

        // Ran to completion: COMPLETED -- no longer counted in `reserved`.
        let g4 = ledger.reserve(rid("r4"), Cost(0.25)).unwrap();
        g4.mark_sent();
        g4.mark_acknowledged();
        let remainder = g4.commit(Cost(0.20));
        assert_cost_eq(remainder, 0.05);
        assert_cost_eq(ledger.reserved(), 0.45); // completed calls drop out of reserved
        assert_cost_eq(ledger.committed(), 0.20);

        // The three still-open (leaked) reservations remain counted correctly.
        assert_cost_eq(ledger.reserved(), 0.10 + 0.20 + 0.15);
    }

    #[test]
    fn a_normally_dropped_reservation_releases_and_leaves_the_ledger_at_zero() {
        let ledger = BudgetLedger::unbounded();
        {
            let _guard = ledger.reserve(rid("r1"), Cost(0.50)).unwrap();
            assert_cost_eq(ledger.reserved(), 0.50);
        } // guard drops here without commit()/mark_orphaned()
        assert_cost_eq(ledger.reserved(), 0.0); // an un-settled guard must release on drop
        assert_cost_eq(ledger.committed(), 0.0);
    }

    #[test]
    fn released_remainder_emits_no_second_event() {
        // "Emits no second event" at this layer means: committing settles the
        // guard so Drop does not *also* release -- reserved must land at exactly
        // zero from the one state transition (RESERVED -> COMPLETED), not from
        // commit() plus a leftover release().
        let ledger = BudgetLedger::unbounded();
        {
            let guard = ledger.reserve(rid("r1"), Cost(0.30)).unwrap();
            let remainder = guard.commit(Cost(0.22));
            assert_cost_eq(remainder, 0.08);
        } // guard already consumed by commit(); nothing left to drop-release
        assert_cost_eq(ledger.reserved(), 0.0);
        assert_cost_eq(ledger.committed(), 0.22);
    }

    #[test]
    fn an_orphaned_reservation_stays_held_not_released() {
        let ledger = BudgetLedger::unbounded();
        let guard = ledger.reserve(rid("r1"), Cost(0.40)).unwrap();
        guard.mark_sent();
        guard.mark_orphaned();
        // orphaned money is held, not released -- it may already be spent
        assert_cost_eq(ledger.reserved(), 0.40);
    }

    #[test]
    fn an_unsatisfiable_reservation_is_refused() {
        let ledger = BudgetLedger::new(Some(Cost(1.00)));
        let _g1 = ledger.reserve(rid("r1"), Cost(0.80)).unwrap();
        let result = ledger.reserve(rid("r2"), Cost(0.30));
        assert!(matches!(result, Err(BudgetExhausted { .. })));
    }

    #[test]
    fn concurrent_reservations_against_one_ledger_are_atomic() {
        let ledger = std::sync::Arc::new(BudgetLedger::unbounded());
        let mut handles = Vec::new();
        for i in 0..50 {
            let ledger = ledger.clone();
            handles.push(std::thread::spawn(move || {
                let g = ledger.reserve(rid(&format!("r{i}")), Cost(0.01)).unwrap();
                g.mark_sent();
                g.commit(Cost(0.01));
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_cost_eq(ledger.committed(), 0.50);
        assert_cost_eq(ledger.reserved(), 0.0);
    }
}
