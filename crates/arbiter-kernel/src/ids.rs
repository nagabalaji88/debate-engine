//! Opaque identifiers this crate's trait seams reference but that neither spec file
//! ever gives a concrete Rust definition (PLAN_DEVIATIONS.md D19) — `ArtifactId`,
//! `ReservationId` and `CallId` appear only as parameter/return types in
//! INTERFACES §1's `Tx` trait, never spelled out with fields. Modelled as opaque
//! string-wrapping newtypes, matching every other `*Id` type across the workspace
//! (`arbiter-core::ids`'s `id_type!` macro) — the one convention the codebase
//! already has for "an opaque identifier that must round-trip through JSON and
//! SQLite unchanged."

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
        }
    };
}

id_type!(/// Returned by `Tx::put_artifact` (INTERFACES §1). Content-addressed in
    /// spirit — ARCHITECTURE §6 calls artifacts "content-addressed... and
    /// versioned" — but the spec never states the id's own encoding, so this stays
    /// an opaque string rather than assuming e.g. a `blake3:` prefix no worked
    /// example confirms.
    ArtifactId);

id_type!(/// The budget ledger's reservation handle (`BUDGET_RESERVED{reservation_id,
    /// estimate}`, INTERFACES §5) — committed via `Tx::commit_budget` and carried
    /// into the idempotency-key formula `blake3(prompt_hash ‖ reservation_id)`.
    ReservationId);

id_type!(/// One provider call's identity across its whole state machine
    /// (`CALL_STARTED{call_id, ...}` through `CALL_COMPLETED{call_id, ...}`,
    /// INTERFACES §5) — the key `Tx::set_call_state` and the `provider_calls`
    /// table are keyed on.
    CallId);

id_type!(/// One log entry's identity — ARCHITECTURE §9's `Event` envelope names
    /// this field `event_id` (`"evt_01J…"`) but never defines its own type; kept
    /// distinct from `Sequence`, which is the entry's *position*, not its identity
    /// (the two happen to be redundant in a single, non-merged log, but a future
    /// merged or replicated log is exactly the case that would need both).
    EventId);

id_type!(/// A pipeline stage's name (`"claims.extract"`, ARCHITECTURE §9's `Event`
    /// envelope; also `Stage::name`, INTERFACES §6, and `PromptTemplate::stage`,
    /// INTERFACES §23). The G-tasks that define the 15 concrete stages own the
    /// actual stage names; this crate only needs the type to exist.
    StageName);

/// An event's position in the append-only log. `ARCHITECTURE §8.1/§8.7`: `seq
/// INTEGER PRIMARY KEY` — a SQLite rowid alias, so this is an ordinary integer,
/// not a string like the identifiers above. `Tx::append_event` returns the
/// sequence it was assigned; `Event.sequence` (ARCHITECTURE §9's JSON envelope)
/// carries the same value once committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sequence(u64);

impl Sequence {
    pub fn new(n: u64) -> Self {
        Self(n)
    }

    pub fn value(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Sequence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_json_as_a_bare_string() {
        let id = ArtifactId::new("art_01J");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"art_01J\"");
        let back: ArtifactId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn sequence_round_trips_as_a_bare_integer() {
        let s = Sequence::new(42);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "42");
        let back: Sequence = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}
