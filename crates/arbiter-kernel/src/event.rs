//! The append-only, hash-chained event log envelope. INTERFACES §13 gives
//! `EventType` verbatim as "the authoritative enum"; ARCHITECTURE §9 gives the
//! `Event` envelope's shape as a JSON example, not a Rust struct, so the field
//! list below is transcribed from that JSON rather than copied from a code block
//! (PLAN_DEVIATIONS.md D19).

use crate::ids::{EventId, Sequence, StageName};
use arbiter_core::RunId;
use serde::{Deserialize, Serialize};

/// INTERFACES §13, copied verbatim — seven families, one flat enum. Adding a
/// variant is additive (old readers skip it but keep the line in the hash chain);
/// removing or renaming one requires a `schema_version` bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventType {
    // Lifecycle
    RunStarted,
    RunCompleted,
    RunFailed,

    // Stage
    StageStarted,
    StageCompleted,
    StageFailed,
    StageCheckpoint,

    // Provider — the crash-recovery protocol (INTERFACES §5) depends on all six
    CallStarted,
    CallRequestId,
    CallCompleted,
    CallRetrying,
    CallRecovered,
    CallOrphaned,

    // Budget — the reservation protocol
    BudgetReserved,
    BudgetCommitted,
    BudgetReleased,
    BudgetExhausted,

    // Debate
    PanelResolved,
    PositionStarted,
    PositionCompleted,
    ClaimExtracted,
    ClaimUngrounded,
    ClaimNormalised,
    CandidatesSelected,
    RelationshipFound,
    DisputePrioritised,
    ChallengeIssued,
    RebuttalReceived,
    RoundStarted,
    RoundCompleted,
    ControllerDecided,

    // Decision
    JudgeScored,
    DecisionSynthesized,
    DecisionAccepted,
    DecisionOverridden,

    // Integrity
    PremiseCycleDetected,
    FixpointNotConverged,
    ChainBreakDetected,
}

/// ARCHITECTURE §9's event envelope — the same shape whether stored as an `events`
/// row or emitted as an NDJSON line on stdout ("one shape, two carriers"). Field
/// list transcribed from that section's JSON example:
/// ```jsonc
/// {
///   "schema_version": 1, "event_id": "evt_01J…", "run_id": "run_01J…",
///   "sequence": 42, "timestamp": "2026-08-31T12:04:11.221Z",
///   "stage": "claims.extract", "event_type": "CLAIM_EXTRACTED", "durable": false,
///   "payload": {}, "content_hash": "blake3:…", "previous_event_hash": "blake3:…"
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub schema_version: u32,
    pub event_id: EventId,
    pub run_id: RunId,
    /// Assigned by the store on append (`Tx::append_event`'s return value,
    /// INTERFACES §1) — `None` before that, `Some` on anything read back.
    pub sequence: Option<Sequence>,
    /// RFC3339, matching the JSON example's `"2026-08-31T12:04:11.221Z"`. Kept as
    /// a plain string rather than adding a timestamp crate for a
    /// trait-signature-only task; a concrete store implementation can parse or
    /// reject as it validates.
    pub timestamp: String,
    pub stage: StageName,
    pub event_type: EventType,
    /// Distinguishes events that fsync immediately from ones batched to the next
    /// stage boundary (ARCHITECTURE §8.3).
    pub durable: bool,
    /// Always inline `TEXT`, never blob-referenced, even past `blob_threshold`
    /// (ARCHITECTURE §8.1 — that threshold applies to artifacts and cached
    /// responses, not to event payloads).
    pub payload: serde_json::Value,
    pub content_hash: String,
    /// `None` only for the very first event in a run's chain.
    pub previous_event_hash: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_type_serializes_screaming_snake_case() {
        let json = serde_json::to_string(&EventType::ClaimExtracted).unwrap();
        assert_eq!(json, "\"CLAIM_EXTRACTED\"");
        let json = serde_json::to_string(&EventType::ChainBreakDetected).unwrap();
        assert_eq!(json, "\"CHAIN_BREAK_DETECTED\"");
    }

    #[test]
    fn event_round_trips_through_json_matching_the_spec_shape() {
        let e = Event {
            schema_version: 1,
            event_id: EventId::new("evt_01J"),
            run_id: RunId::new("run_01J"),
            sequence: Some(Sequence::new(42)),
            timestamp: "2026-08-31T12:04:11.221Z".to_string(),
            stage: StageName::new("claims.extract"),
            event_type: EventType::ClaimExtracted,
            durable: false,
            payload: serde_json::json!({}),
            content_hash: "blake3:abc".to_string(),
            previous_event_hash: Some("blake3:def".to_string()),
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["sequence"], 42);
        assert_eq!(json["event_type"], "CLAIM_EXTRACTED");
        let back: Event = serde_json::from_value(json).unwrap();
        assert_eq!(e, back);
    }
}
