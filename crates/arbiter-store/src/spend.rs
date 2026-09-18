//! What a run actually spent, read back from its own event log.
//!
//! The durable `budget` table is written through `Tx::reserve_call` /
//! `Tx::commit_budget`, and the normal run path does not call them -- it
//! appends events (PLAN_DEVIATIONS.md D61). So the events are the only place a
//! finished run's spend actually exists, and this reads them rather than
//! reporting the zero the untouched table would give.
//!
//! That zero was being written into `history.db` verbatim: every completed run
//! showed `$0.00`, and Usage added those up into a total that was always
//! nothing. A cost of zero is a strong claim -- it is what a free model looks
//! like -- and it should never be the thing a reader sees when the truth is
//! "this was never recorded".

use arbiter_kernel::event::{Event, EventType};

/// A run's spend, as its events describe it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spend {
    /// Committed by calls that completed.
    pub committed: f64,
    /// Held by calls that reached the provider and never came back. This is
    /// money that may or may not have been billed; ARCHITECTURE §8.4 keeps it
    /// held and reported rather than absorbed.
    pub orphaned: f64,
    /// Whether every committed figure came from a provider's own reported
    /// usage. False when any call fell back to its reservation estimate --
    /// an aggregator, an unpriced model, or a response with no usage block.
    pub fully_measured: bool,
}

impl Spend {
    /// Nothing spent yet. `fully_measured` starts true and only ever falls:
    /// a run with no calls in it has no unmeasured ones either.
    pub fn empty() -> Self {
        Self {
            committed: 0.0,
            orphaned: 0.0,
            fully_measured: true,
        }
    }
}

impl Default for Spend {
    fn default() -> Self {
        Self::empty()
    }
}

/// Total a run's spend from its events.
pub fn from_events<'a>(events: impl Iterator<Item = &'a Event>) -> Spend {
    events.fold(Spend::empty(), accumulate)
}

/// Fold one event into a running total, for a caller that sees events as they
/// are written rather than reading them back afterwards.
pub fn accumulate(mut spend: Spend, event: &Event) -> Spend {
    match event.event_type {
        EventType::CallCompleted => {
            spend.committed += number(event, "actual_cost");
            // Absent means an event written before the flag existed, which is
            // exactly the case we cannot vouch for either.
            if event.payload.get("cost_measured").and_then(|v| v.as_bool()) != Some(true) {
                spend.fully_measured = false;
            }
        }
        EventType::CallOrphaned => spend.orphaned += number(event, "held"),
        _ => {}
    }
    spend
}

fn number(event: &Event, key: &str) -> f64 {
    event
        .payload
        .get(key)
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arbiter_core::RunId;
    use arbiter_kernel::ids::{EventId, StageName};

    fn event(event_type: EventType, payload: serde_json::Value) -> Event {
        Event {
            schema_version: 1,
            event_id: EventId::new("evt"),
            run_id: RunId::new("run"),
            sequence: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            stage: StageName::new("positions.generate"),
            event_type,
            durable: false,
            payload,
            content_hash: String::new(),
            previous_event_hash: None,
        }
    }

    #[test]
    fn completed_calls_add_up_and_stay_marked_measured() {
        let events = [
            event(
                EventType::CallCompleted,
                serde_json::json!({"actual_cost": 0.02, "cost_measured": true}),
            ),
            event(
                EventType::CallCompleted,
                serde_json::json!({"actual_cost": 0.03, "cost_measured": true}),
            ),
        ];
        let spend = from_events(events.iter());
        assert!((spend.committed - 0.05).abs() < 1e-9);
        assert!(spend.fully_measured);
    }

    /// One estimated call is enough to stop the total claiming to be measured.
    /// A reader deciding whether to trust a figure needs the weakest link, not
    /// the average.
    #[test]
    fn one_estimated_call_makes_the_whole_total_an_estimate() {
        let events = [
            event(
                EventType::CallCompleted,
                serde_json::json!({"actual_cost": 0.02, "cost_measured": true}),
            ),
            event(
                EventType::CallCompleted,
                serde_json::json!({"actual_cost": 0.01, "cost_measured": false}),
            ),
        ];
        assert!(!from_events(events.iter()).fully_measured);
    }

    #[test]
    fn orphaned_money_is_counted_separately_from_committed() {
        let events = [
            event(
                EventType::CallCompleted,
                serde_json::json!({"actual_cost": 0.02, "cost_measured": true}),
            ),
            event(EventType::CallOrphaned, serde_json::json!({"held": 0.01})),
        ];
        let spend = from_events(events.iter());
        assert!((spend.committed - 0.02).abs() < 1e-9);
        assert!(
            (spend.orphaned - 0.01).abs() < 1e-9,
            "held money must not be folded into what was definitely spent"
        );
    }

    #[test]
    fn a_run_with_no_calls_spent_nothing_and_that_is_measured() {
        let spend = from_events([].iter());
        assert_eq!(spend.committed, 0.0);
        assert!(spend.fully_measured);
    }
}
