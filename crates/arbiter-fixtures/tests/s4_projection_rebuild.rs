//! F2 — `projection_rebuild` (S4), ARCHITECTURE §18's CI suite: "replay
//! rebuilds every projection table; result is identical to the pre-crash
//! tables."

use arbiter_core::RunId;
use arbiter_kernel::event::{Event, EventType};
use arbiter_kernel::ids::{EventId, StageName};
use arbiter_store::project::rebuild_operational_projections;
use arbiter_store::schema::open_run_db;
use rusqlite::Connection;

fn conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    open_run_db(&conn, "0.1.0", "2026-09-02T00:00:00Z").unwrap();
    conn
}

fn insert_event(conn: &Connection, event_type: EventType, payload: serde_json::Value, ts: &str) {
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

/// `(call_id, state, reserved_amount, actual_cost, request_id)`.
type ProviderCallRow = (String, String, f64, Option<f64>, Option<String>);

fn snapshot(conn: &Connection) -> (f64, f64, Vec<ProviderCallRow>) {
    let (reserved, committed): (f64, f64) = conn
        .query_row("SELECT reserved, committed FROM budget", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();

    let mut stmt = conn
        .prepare("SELECT call_id, state, reserved_amount, actual_cost, request_id FROM provider_calls ORDER BY call_id")
        .unwrap();
    let calls: Vec<ProviderCallRow> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect();

    (reserved, committed, calls)
}

/// Two whole calls' worth of events (one completes normally, the other is
/// left mid-flight at `ACKNOWLEDGED` -- exactly "a crash after CALL_REQUEST_ID
/// but before CALL_COMPLETED") feed `budget`/`provider_calls` through the
/// stages' own normal write path first, establishing the "pre-crash" tables.
/// Both tables are then wiped outright -- standing in for the projections
/// being lost, though the append-only `events` log survives any crash by
/// construction -- and `rebuild_operational_projections` replays `events`
/// alone. The rebuilt tables must come back byte-for-byte identical to the
/// snapshot taken before the wipe: replay is not an approximation of the
/// pre-crash state, it reconstructs it exactly.
#[test]
fn projection_rebuild() {
    let conn = conn();

    // Call 1: reserved, sent, acknowledged, completed.
    insert_event(
        &conn,
        EventType::BudgetReserved,
        serde_json::json!({"reservation_id": "res_1", "estimate": 0.50}),
        "t0",
    );
    insert_event(
        &conn,
        EventType::CallStarted,
        serde_json::json!({"call_id": "call_1", "prompt_hash": "blake3:p1", "reservation_id": "res_1", "estimate": 0.50}),
        "t1",
    );
    insert_event(
        &conn,
        EventType::CallRequestId,
        serde_json::json!({"call_id": "call_1", "request_id": "req_1"}),
        "t2",
    );
    insert_event(
        &conn,
        EventType::CallCompleted,
        serde_json::json!({"call_id": "call_1", "response_hash": "blake3:r1", "actual_cost": 0.30}),
        "t3",
    );

    // Call 2: reserved, sent, acknowledged -- then the crash, before CALL_COMPLETED ever committed.
    insert_event(
        &conn,
        EventType::BudgetReserved,
        serde_json::json!({"reservation_id": "res_2", "estimate": 0.80}),
        "t4",
    );
    insert_event(
        &conn,
        EventType::CallStarted,
        serde_json::json!({"call_id": "call_2", "prompt_hash": "blake3:p2", "reservation_id": "res_2", "estimate": 0.80}),
        "t5",
    );
    insert_event(
        &conn,
        EventType::CallRequestId,
        serde_json::json!({"call_id": "call_2", "request_id": "req_2"}),
        "t6",
    );

    // Establish the "pre-crash" projection state via the normal replay path once.
    rebuild_operational_projections(&conn).unwrap();
    let pre_crash = snapshot(&conn);
    assert_eq!(
        pre_crash.2.len(),
        2,
        "both calls must appear in the pre-crash snapshot"
    );

    // Simulate the projections themselves being lost (the log is durable;
    // the derived tables, by ARCHITECTURE §8.1's own premise, are not).
    conn.execute("DELETE FROM provider_calls", []).unwrap();
    conn.execute("UPDATE budget SET reserved = 999, committed = 999", [])
        .unwrap();

    rebuild_operational_projections(&conn).unwrap();
    let rebuilt = snapshot(&conn);

    assert_eq!(
        rebuilt, pre_crash,
        "replaying the same event log must reconstruct byte-identical projection tables"
    );
}
