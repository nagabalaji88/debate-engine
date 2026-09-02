//! Rebuilding `budget` and `provider_calls` from `events` alone — ARCHITECTURE
//! §8.1: "projection ... rebuilt by replay," and "where a projection and the
//! log disagree, the log wins and the projection is wrong."
//!
//! Scoped to these two tables only. `cache_entries` and `artifacts` are written
//! directly by [`crate::sqlite_store::SqliteTx::put_cache`]/`put_artifact` and
//! are not derivable from a *generic* event's payload the way `budget`/
//! `provider_calls` are — INTERFACES §5's crash-recovery sequence gives the
//! exact field lists this module replays; no equivalent per-stage payload
//! contract exists yet for cache/artifact content (PLAN_DEVIATIONS.md D29).
//! `stages` and the ten claim-graph/decision projections stay deferred to
//! G2–G9 for the same reason `cache_entries`/`artifacts` full-replay does.

use rusqlite::Connection;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// Clears and rebuilds `budget`/`provider_calls` by replaying `events` in
/// `seq` order. Field names come straight from INTERFACES §5's own brace
/// notation (`BUDGET_RESERVED{reservation_id, estimate}`, `CALL_STARTED{call_id,
/// prompt_hash, reservation_id, estimate}`, `CALL_REQUEST_ID{call_id,
/// request_id}`, `CALL_COMPLETED{call_id, response_hash, actual_cost}`) —
/// nothing invented beyond giving that notation a concrete JSON key spelling.
/// An event whose `event_type` this binary cannot parse, or whose payload is
/// missing an expected field, is skipped rather than treated as fatal — the
/// same forward-compatibility posture `RunReader::events`' typed view already
/// takes (INTERFACES §13).
///
/// Only the happy path (`RESERVED → SENT → ACKNOWLEDGED → COMPLETED`) is
/// reconstructed here. `CALL_RETRYING`/`CALL_ORPHANED`/`CALL_RECOVERED` and
/// `BUDGET_RELEASED`/`BUDGET_EXHAUSTED` are not replayed by this function —
/// their full crash-recovery semantics (INTERFACES §5's branch table) are
/// K2/L3 resume-logic's own scope, not this projection rebuild's. A
/// `BUDGET_RESERVED` with no matching `CALL_STARTED` is, correctly, simply
/// never applied: INTERFACES §5 states that case "resumes as FAILED with the
/// reservation released," i.e. contributes nothing to either table.
pub fn rebuild_operational_projections(conn: &Connection) -> Result<(), ProjectError> {
    conn.execute("DELETE FROM provider_calls", [])?;
    conn.execute("UPDATE budget SET reserved = 0, committed = 0", [])?;

    let mut stmt =
        conn.prepare("SELECT event_type, payload, timestamp FROM events ORDER BY seq")?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .filter_map(Result::ok)
        .collect();
    drop(stmt);

    // reservation_id -> estimate, for a BUDGET_RESERVED not yet matched to its
    // CALL_STARTED.
    let mut pending_reservations: BTreeMap<String, f64> = BTreeMap::new();

    for (event_type_json, payload_json, timestamp) in rows {
        let Ok(event_type) =
            serde_json::from_str::<arbiter_kernel::event::EventType>(&event_type_json)
        else {
            continue;
        };
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&payload_json) else {
            continue;
        };

        use arbiter_kernel::event::EventType;
        match event_type {
            EventType::BudgetReserved => {
                let (Some(reservation_id), Some(estimate)) = (
                    payload.get("reservation_id").and_then(|v| v.as_str()),
                    payload.get("estimate").and_then(|v| v.as_f64()),
                ) else {
                    continue;
                };
                pending_reservations.insert(reservation_id.to_string(), estimate);
            }
            EventType::CallStarted => {
                let (Some(call_id), Some(reservation_id)) = (
                    payload.get("call_id").and_then(|v| v.as_str()),
                    payload.get("reservation_id").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                let estimate = pending_reservations
                    .remove(reservation_id)
                    .or_else(|| payload.get("estimate").and_then(|v| v.as_f64()));
                let Some(estimate) = estimate else { continue };

                let state_json = serde_json::to_string(&arbiter_kernel::provider::CallState::Sent)
                    .expect("CallState serializes");
                conn.execute(
                    "INSERT INTO provider_calls (call_id, reservation_id, state, reserved_amount, actual_cost, request_id, created_at)
                     VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5)",
                    rusqlite::params![call_id, reservation_id, state_json, estimate, timestamp],
                )?;
                conn.execute("UPDATE budget SET reserved = reserved + ?1", [estimate])?;
            }
            EventType::CallRequestId => {
                let (Some(call_id), Some(request_id)) = (
                    payload.get("call_id").and_then(|v| v.as_str()),
                    payload.get("request_id").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                let state_json =
                    serde_json::to_string(&arbiter_kernel::provider::CallState::Acknowledged)
                        .expect("CallState serializes");
                conn.execute(
                    "UPDATE provider_calls SET state = ?1, request_id = ?2 WHERE call_id = ?3",
                    rusqlite::params![state_json, request_id, call_id],
                )?;
            }
            EventType::CallCompleted => {
                let (Some(call_id), Some(actual_cost)) = (
                    payload.get("call_id").and_then(|v| v.as_str()),
                    payload.get("actual_cost").and_then(|v| v.as_f64()),
                ) else {
                    continue;
                };
                let reserved_amount: Option<f64> = conn
                    .query_row(
                        "SELECT reserved_amount FROM provider_calls WHERE call_id = ?1",
                        [call_id],
                        |r| r.get(0),
                    )
                    .ok();
                let Some(reserved_amount) = reserved_amount else {
                    continue;
                };
                conn.execute(
                    "UPDATE budget SET committed = committed + ?1, reserved = reserved - ?2",
                    rusqlite::params![actual_cost, reserved_amount],
                )?;
                let state_json =
                    serde_json::to_string(&arbiter_kernel::provider::CallState::Completed)
                        .expect("CallState serializes");
                conn.execute(
                    "UPDATE provider_calls SET state = ?1, actual_cost = ?2 WHERE call_id = ?3",
                    rusqlite::params![state_json, actual_cost, call_id],
                )?;
            }
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::open_run_db;
    use arbiter_core::RunId;
    use arbiter_kernel::event::{Event, EventType};
    use arbiter_kernel::ids::{EventId, StageName};

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        open_run_db(&conn, "0.1.0", "2026-09-02T00:00:00Z").unwrap();
        conn
    }

    fn insert_event(
        conn: &Connection,
        event_type: EventType,
        payload: serde_json::Value,
        ts: &str,
    ) {
        let event = Event {
            schema_version: 1,
            event_id: EventId::new(format!("evt_{ts}_{event_type:?}")),
            run_id: RunId::new("run_1"),
            sequence: None,
            timestamp: ts.to_string(),
            stage: StageName::new("test"),
            event_type,
            durable: true,
            payload,
            content_hash: "blake3:x".to_string(),
            previous_event_hash: None,
        };
        conn.execute(
            "INSERT INTO events (seq, run_id, schema_version, event_id, timestamp, stage, event_type, durable, payload, content_hash, previous_event_hash)
             VALUES (NULL, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                event.run_id.as_str(),
                event.schema_version,
                event.event_id.as_str(),
                event.timestamp,
                event.stage.as_str(),
                serde_json::to_string(&event.event_type).unwrap(),
                event.durable,
                event.payload.to_string(),
                event.content_hash,
                event.previous_event_hash,
            ],
        )
        .unwrap();
    }

    fn read_budget(conn: &Connection) -> (f64, f64) {
        conn.query_row("SELECT reserved, committed FROM budget", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
    }

    #[test]
    fn rebuild_reconstructs_the_happy_path() {
        let conn = conn();
        insert_event(
            &conn,
            EventType::BudgetReserved,
            serde_json::json!({"reservation_id": "res_1", "estimate": 0.50}),
            "t0",
        );
        insert_event(
            &conn,
            EventType::CallStarted,
            serde_json::json!({"call_id": "call_1", "prompt_hash": "blake3:p", "reservation_id": "res_1", "estimate": 0.50}),
            "t1",
        );
        insert_event(
            &conn,
            EventType::CallRequestId,
            serde_json::json!({"call_id": "call_1", "request_id": "req_abc"}),
            "t2",
        );
        insert_event(
            &conn,
            EventType::CallCompleted,
            serde_json::json!({"call_id": "call_1", "response_hash": "blake3:r", "actual_cost": 0.30}),
            "t3",
        );

        rebuild_operational_projections(&conn).unwrap();

        let (reserved, committed) = read_budget(&conn);
        assert!((reserved - 0.0).abs() < 1e-9, "reserved: {reserved}");
        assert!((committed - 0.30).abs() < 1e-9, "committed: {committed}");

        let (state, request_id): (String, Option<String>) = conn
            .query_row(
                "SELECT state, request_id FROM provider_calls WHERE call_id = 'call_1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            state,
            serde_json::to_string(&arbiter_kernel::provider::CallState::Completed).unwrap()
        );
        assert_eq!(request_id.as_deref(), Some("req_abc"));
    }

    #[test]
    fn a_reservation_with_no_call_started_resumes_as_released() {
        // INTERFACES §5: "a crash between 0 and 1 resumes as FAILED with the
        // reservation released" -- rebuild must not leave it counted as held.
        let conn = conn();
        insert_event(
            &conn,
            EventType::BudgetReserved,
            serde_json::json!({"reservation_id": "res_orphan", "estimate": 5.00}),
            "t0",
        );

        rebuild_operational_projections(&conn).unwrap();

        let (reserved, committed) = read_budget(&conn);
        assert!((reserved - 0.0).abs() < 1e-9, "reserved: {reserved}");
        assert!((committed - 0.0).abs() < 1e-9, "committed: {committed}");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM provider_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn rebuild_is_idempotent_and_clears_stale_rows_first() {
        let conn = conn();
        insert_event(
            &conn,
            EventType::BudgetReserved,
            serde_json::json!({"reservation_id": "res_1", "estimate": 1.0}),
            "t0",
        );
        insert_event(
            &conn,
            EventType::CallStarted,
            serde_json::json!({"call_id": "call_1", "prompt_hash": "blake3:p", "reservation_id": "res_1", "estimate": 1.0}),
            "t1",
        );

        rebuild_operational_projections(&conn).unwrap();
        rebuild_operational_projections(&conn).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM provider_calls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "a second rebuild must not duplicate rows");

        let (reserved, _) = read_budget(&conn);
        assert!((reserved - 1.0).abs() < 1e-9, "reserved: {reserved}");
    }

    #[test]
    fn an_event_with_a_malformed_payload_is_skipped_not_fatal() {
        let conn = conn();
        insert_event(
            &conn,
            EventType::BudgetReserved,
            serde_json::json!({"reservation_id": "res_1"}), // missing "estimate"
            "t0",
        );

        // Must not error -- the malformed event is simply skipped.
        rebuild_operational_projections(&conn).unwrap();

        let (reserved, committed) = read_budget(&conn);
        assert!((reserved - 0.0).abs() < 1e-9);
        assert!((committed - 0.0).abs() < 1e-9);
    }
}
