//! The eight endpoints (IMPLEMENTATION_PLAN.md's own U1 table — one more
//! than ARCHITECTURE §17.1's summary "five," see PLAN_DEVIATIONS.md D48).
//! Every handler is a thin read or a thin write against `arbiter-store`/
//! `arbiter-providers`, or a spawn of the same `run_pipeline` the CLI's own
//! `run` command uses (`super::spawn_run`) — never a second computation of
//! anything `arbiter-core` already computed.

use super::AppState;
use crate::render;
use arbiter_core::{DecisionAcceptance, DecisionOverride, OverrideId, RunId};
use arbiter_kernel::bounds::Depth;
use arbiter_kernel::event::EventType;
use arbiter_kernel::store::RunStore;
use arbiter_providers::keys::CredentialSource;
use arbiter_store::sqlite_store::SqliteRunStore;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::Stream;
use serde::Deserialize;
use std::convert::Infallible;
use std::time::Duration;

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({"error": message.into()}))).into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateRunBody {
    question: String,
    #[serde(default)]
    depth: Option<String>,
    #[serde(default)]
    budget: Option<f64>,
}

/// `POST /api/runs` — `202`, **spends money**. The one endpoint whose whole
/// point is starting a real `run_pipeline` in the background; everything
/// else in this file only ever reads or appends a small record.
pub(crate) async fn create_run(
    State(state): State<AppState>,
    Json(body): Json<CreateRunBody>,
) -> Response {
    if body.question.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "question must not be empty");
    }
    let depth = match body.depth.as_deref() {
        None | Some("standard") => Depth::Standard,
        Some("deep") => Depth::Deep,
        Some(other) => {
            return err(
                StatusCode::BAD_REQUEST,
                format!("unknown depth '{other}': expected 'standard' or 'deep'"),
            );
        }
    };

    match super::spawn_run(&state, body.question, depth, body.budget) {
        Ok(run_id) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"run_id": run_id.as_str()})),
        )
            .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct ListRunsQuery {
    outcome: Option<String>,
    since: Option<String>,
    min_confidence: Option<f64>,
    policy_version: Option<String>,
}

/// `GET /api/runs` — `run_catalog` rows, screen 4's own source. Every
/// filter but `policy_version` is `arbiter-store::catalog`'s own
/// `HistoryFilter`; `policy_version` is applied here instead, over the rows
/// that filter already returned — `HistoryFilter` has no such field (only
/// `arbiter history` ever queried it before, and never by policy version),
/// and adding one there for a single caller would widen that module's own
/// scope for no reader this task doesn't already have.
pub(crate) async fn list_runs(
    State(state): State<AppState>,
    Query(q): Query<ListRunsQuery>,
) -> Response {
    let conn = match arbiter_store::catalog::open_history_db(
        &crate::history_db_path(&state.store_root),
        env!("CARGO_PKG_VERSION"),
        &arbiter_store::now_rfc3339(),
    ) {
        Ok(c) => c,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("opening history.db: {e}"),
            );
        }
    };
    let rows = match arbiter_store::catalog::list_runs(
        &conn,
        &arbiter_store::catalog::HistoryFilter {
            outcome: q.outcome,
            since: q.since,
            min_confidence: q.min_confidence,
        },
    ) {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("querying history.db: {e}"),
            );
        }
    };

    let rows: Vec<_> = rows
        .into_iter()
        .filter(|r| {
            q.policy_version
                .as_deref()
                .is_none_or(|want| r.policy_version == want)
        })
        .map(|r| {
            serde_json::json!({
                "run_id": r.run_id, "status": r.status, "question": r.question,
                "outcome": r.outcome, "confidence": r.confidence, "margin": r.margin,
                "cost": r.cost, "orphaned_cost": r.orphaned_cost, "model_count": r.model_count,
                "depth": r.depth, "policy_version": r.policy_version, "started_at": r.started_at,
                "completed_at": r.completed_at,
            })
        })
        .collect();
    Json(rows).into_response()
}

/// `GET /api/runs/:id` — the `explain --json` payload, **unchanged**
/// (ARCHITECTURE §17.1's own words) once a decision exists. Before that,
/// there is no such payload to return verbatim at all, so the two other
/// real states (`running`, `failed`) get their own distinct, honestly
/// different shape rather than a reshaping of the one that doesn't exist
/// yet — the "explain_endpoint_matches_cli_byte_for_byte" acceptance test
/// only constrains the one case where a payload genuinely exists.
pub(crate) async fn get_run(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let run_id = RunId::new(id);
    let store = SqliteRunStore::new(&state.store_root);
    let reader = match store.reader(&run_id) {
        Ok(r) => r,
        Err(_) => return err(StatusCode::NOT_FOUND, "unknown run"),
    };

    match render::read_decision_record(reader.as_ref()) {
        Ok(record) => match render::read_final_graph(reader.as_ref()) {
            Ok(graph) => {
                let output = render::build_explain(&record, &graph, None);
                Json(output).into_response()
            }
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        },
        Err(_) => {
            // No synthesized decision yet: still running, or it failed
            // before ever reaching decision.synthesize. The last recorded
            // event tells the two apart.
            let last_type = reader
                .events()
                .ok()
                .and_then(|events| events.last())
                .map(|e| e.event_type);
            match last_type {
                Some(EventType::RunFailed) => (
                    StatusCode::OK,
                    Json(serde_json::json!({"run_id": run_id.as_str(), "status": "failed"})),
                )
                    .into_response(),
                _ => (
                    StatusCode::OK,
                    Json(serde_json::json!({"run_id": run_id.as_str(), "status": "running"})),
                )
                    .into_response(),
            }
        }
    }
}

/// `GET /api/runs/:id/events` — SSE, each `data:` line one event envelope
/// (`arbiter show --transcript --json`'s own per-event shape, reused
/// rather than inventing a second one — see PLAN_DEVIATIONS.md D48 for why
/// there is no existing `--stream` output to match byte-for-byte, ARCHITECTURE
/// §17.1's own stated reference point). `id:` is the event's own sequence
/// number, so a reconnect's `Last-Event-ID` resumes from `sequence + 1`
/// with no buffering needed — every event is already durable in the store
/// by the time this stream can read it back.
pub(crate) async fn run_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let run_id = RunId::new(id);
    let last_seen: u64 = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let stream = futures_util::stream::unfold(
        (state, run_id, last_seen, false),
        |(state, run_id, after, done)| async move {
            if done {
                return None;
            }
            loop {
                let store = SqliteRunStore::new(&state.store_root);
                let Ok(reader) = store.reader(&run_id) else {
                    return None;
                };
                let Ok(events) = reader.events() else {
                    return None;
                };
                let mut new_events: Vec<arbiter_kernel::event::Event> = events
                    .filter(|e| e.sequence.map(|s| s.value()).unwrap_or(0) > after)
                    .collect();
                new_events.sort_by_key(|e| e.sequence.map(|s| s.value()).unwrap_or(0));

                if let Some(next) = new_events.first() {
                    let seq = next.sequence.map(|s| s.value()).unwrap_or(after);
                    let terminal = matches!(
                        next.event_type,
                        EventType::RunCompleted | EventType::RunFailed
                    );
                    let payload = serde_json::to_string(next).unwrap_or_default();
                    let sse = SseEvent::default().id(seq.to_string()).data(payload);
                    return Some((Ok(sse), (state, run_id, seq, terminal)));
                }

                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        },
    );

    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct AcceptBody {
    #[serde(default)]
    overrides: Vec<OverrideBody>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OverrideBody {
    path: String,
    to: serde_json::Value,
    reason: String,
}

/// `POST /api/runs/:id/accept` — records who and when, and (INTERFACES §17)
/// refuses an override with an empty reason exactly as `arbiter accept`
/// does, through the same `AcceptanceArtifact` (`accept.rs`, made
/// `pub(crate)` for this one caller) so the two paths can never disagree on
/// what `decision_acceptance.v1` looks like.
pub(crate) async fn accept_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<AcceptBody>>,
) -> Response {
    let run_id = RunId::new(id);
    let body = body.map(|Json(b)| b).unwrap_or_default();

    for o in &body.overrides {
        if o.reason.trim().is_empty() {
            return err(
                StatusCode::BAD_REQUEST,
                "an override with an empty reason is rejected (INTERFACES §17)",
            );
        }
    }

    let store = SqliteRunStore::new(&state.store_root);
    let reader = match store.reader(&run_id) {
        Ok(r) => r,
        Err(_) => return err(StatusCode::NOT_FOUND, "unknown run"),
    };
    match reader.artifacts_by_type("synthesized_decision.v1") {
        Ok(a) if !a.is_empty() => {}
        Ok(_) => {
            return err(
                StatusCode::CONFLICT,
                "this run has no synthesized decision yet",
            );
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }

    let decision_overrides: Vec<DecisionOverride> = body
        .overrides
        .into_iter()
        .enumerate()
        .map(|(i, o)| DecisionOverride {
            id: OverrideId::new(format!("ovr_{}_{}", run_id.as_str(), i + 1)),
            path: o.path,
            from: serde_json::Value::Null,
            to: o.to,
            reason: o.reason,
        })
        .collect();

    let accepted_by = "loopback-ui".to_string();
    let acceptance = DecisionAcceptance {
        accepted_by,
        accepted_at: arbiter_store::now_rfc3339(),
        overrides: decision_overrides,
    };

    let writer = match store.reopen(&run_id) {
        Ok(w) => w,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("reopening run: {e}"),
            );
        }
    };
    let last_event = reader.events().ok().and_then(|e| e.last());
    let handle = crate::run_handle::RunHandle::new(run_id.clone(), writer)
        .continuing_from(last_event.as_ref());

    if let Err(e) = handle.append_lifecycle_event(
        EventType::DecisionAccepted,
        serde_json::json!({"accepted_by": acceptance.accepted_by, "accepted_at": acceptance.accepted_at}),
    ) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    for o in &acceptance.overrides {
        if let Err(e) = handle.append_lifecycle_event(
            EventType::DecisionOverridden,
            serde_json::json!({"id": o.id.as_str(), "path": o.path, "to": o.to, "reason": o.reason}),
        ) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    }
    if let Err(e) = handle.put_artifact(&crate::accept::AcceptanceArtifact(acceptance.clone())) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    Json(acceptance).into_response()
}

/// `GET /api/providers` — the roster of INTERFACES §25: state, source,
/// fingerprint, never the key. Reuses `maintenance::known_providers`/
/// `credential_sources`, made `pub(crate)` for this one caller, so `arbiter
/// providers list` and this endpoint report the same roster from the same
/// resolution code.
pub(crate) async fn list_providers() -> Response {
    let (env, keychain) = crate::maintenance::credential_sources();
    let sources: [&dyn CredentialSource; 2] = [&env, &keychain];

    let rows: Vec<serde_json::Value> = crate::maintenance::known_providers()
        .into_iter()
        .map(|provider| {
            if provider.as_str() == "mock" {
                return serde_json::json!({
                    "id": provider.as_str(),
                    "state": "not_required",
                    "source": null,
                    "fingerprint": null,
                    "usable": true,
                });
            }
            let mut resolved = None;
            for source in &sources {
                if let Some((secret, key_source)) = source.resolve(&provider) {
                    resolved = Some((secret, key_source));
                    break;
                }
            }
            match resolved {
                Some((secret, key_source)) => {
                    let fp = secret.fingerprint();
                    serde_json::json!({
                        "id": provider.as_str(),
                        "state": "present",
                        "source": describe_source(&key_source),
                        "fingerprint": &fp[fp.len().saturating_sub(4)..],
                        // P4's real adapters don't exist in this build
                        // (PLAN_DEVIATIONS.md D46) -- a key being present is
                        // not the same as a key ever having been verified.
                        "usable": false,
                    })
                }
                None => serde_json::json!({
                    "id": provider.as_str(),
                    "state": "missing",
                    "source": null,
                    "fingerprint": null,
                    "usable": false,
                }),
            }
        })
        .collect();

    Json(rows).into_response()
}

fn describe_source(source: &arbiter_providers::keys::KeySource) -> String {
    match source {
        arbiter_providers::keys::KeySource::ArbiterEnv(var) => format!("env:{var}"),
        arbiter_providers::keys::KeySource::ProviderEnv(var) => format!("env:{var}"),
        arbiter_providers::keys::KeySource::Keychain => "keychain".to_string(),
    }
}

/// `POST /api/providers/:p/test` — **makes a paid request**, per the plan's
/// own table; honestly refused here exactly as `arbiter providers test`
/// refuses, since no real adapter (P4) exists in this build to make that
/// request with.
pub(crate) async fn test_provider(Path(_provider): Path<String>) -> Response {
    err(
        StatusCode::NOT_IMPLEMENTED,
        "provider verification needs P4 (real provider adapters), not implemented in this build \
         (PLAN_DEVIATIONS.md D46)",
    )
}
