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
use serde::Deserialize;
use std::time::Duration;

fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({"error": message.into()}))).into_response()
}

/// `SqliteRunStore::reader` opens (and, if absent, silently *creates*) a
/// run's own `run.db` -- exactly what `init` needs the first time a real
/// run starts, but wrong for every read-only handler here: without this
/// check, `GET /api/runs/<any-made-up-id>` would open a fresh, genuinely
/// empty database and read it back as `{"status": "running"}` forever,
/// rather than `404`. Checked as a plain file existence test, before the
/// reader is ever opened, so nothing gets created by the act of looking.
fn run_exists(state: &AppState, run_id: &RunId) -> bool {
    state
        .store_root
        .join(run_id.as_str())
        .join("run.db")
        .is_file()
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateRunBody {
    question: String,
    #[serde(default)]
    depth: Option<String>,
    #[serde(default)]
    budget: Option<f64>,
    /// Same spec `arbiter run --panel` takes (`mock`, or
    /// `anthropic,openai:gpt-4o,...`). Absent means `mock`, so a request
    /// written before P4 still behaves exactly as it did.
    #[serde(default)]
    panel: Option<String>,
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

    let question = match crate::resolve_question(&body.question) {
        Ok(q) => q,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };

    let panel_spec = body.panel.clone().unwrap_or_else(|| "mock".to_string());
    match super::spawn_run(&state, question, depth, body.budget, &panel_spec) {
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
/// Screen 3's own source (U4) needs more than INTERFACES §22's `explain`
/// schema carries -- outcome, the winning recommendation, the claim list
/// with standing, and run-integrity signals (`show`/`claims`/`doctor`'s
/// own read paths, not `explain`'s). Rather than a second endpoint, this
/// nests the untouched `build_explain` output under its own `"explain"`
/// key and adds the rest as sibling fields -- "returns the §22 payload
/// unchanged" (ARCHITECTURE §17.1) still holds for that nested object
/// byte-for-byte; nothing about it is reshaped. See PLAN_DEVIATIONS.md
/// D49.
pub(crate) async fn get_run(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let run_id = RunId::new(id);
    if !run_exists(&state, &run_id) {
        return err(StatusCode::NOT_FOUND, "unknown run");
    }
    let store = SqliteRunStore::new(&state.store_root);
    let reader = match store.reader(&run_id) {
        Ok(r) => r,
        Err(_) => return err(StatusCode::NOT_FOUND, "unknown run"),
    };

    match render::read_decision_record(reader.as_ref()) {
        Ok(record) => match render::read_final_graph(reader.as_ref()) {
            Ok(graph) => {
                let explain = render::build_explain(&record, &graph, None);
                let claims = render::claim_rows(&record, &graph);
                let completeness = render::read_completeness(reader.as_ref()).ok();
                let chain_verified = reader
                    .verify_chain()
                    .map(|status| matches!(status, arbiter_kernel::store::ChainStatus::Intact))
                    .unwrap_or(false);
                let fixpoint_converged = !reader
                    .events()
                    .map(|mut events| {
                        events.any(|e| e.event_type == EventType::FixpointNotConverged)
                    })
                    .unwrap_or(false);
                let orphaned_cost = orphaned_cost_for(&state, &run_id);

                Json(serde_json::json!({
                    "run_id": run_id.as_str(),
                    "status": "complete",
                    "policy_version": record.policy_version.as_str(),
                    "outcome": record.outcome,
                    "recommendation": record.recommendation,
                    "claims": claims,
                    "integrity": {
                        "chain_verified": chain_verified,
                        "fixpoint_converged": fixpoint_converged,
                        "completeness": completeness,
                        "orphaned_cost": orphaned_cost,
                    },
                    "explain": explain,
                }))
                .into_response()
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

/// `run_catalog.orphaned_cost` for one run, best-effort (`None`/`0.0` on
/// any read failure, the same "a missing catalogue entry costs a display
/// field, not the run" posture `arbiter history` already takes) --
/// currently always `0.0` in this build, since nothing yet computes a real
/// non-zero value for it (D45/D48's own precedent: `run_command` writes
/// the literal `0.0` too, honestly, not a guess).
fn orphaned_cost_for(state: &AppState, run_id: &RunId) -> f64 {
    let Ok(conn) = arbiter_store::catalog::open_history_db(
        &crate::history_db_path(&state.store_root),
        env!("CARGO_PKG_VERSION"),
        &arbiter_store::now_rfc3339(),
    ) else {
        return 0.0;
    };
    arbiter_store::catalog::list_runs(&conn, &arbiter_store::catalog::HistoryFilter::default())
        .ok()
        .and_then(|rows| rows.into_iter().find(|r| r.run_id == run_id.as_str()))
        .map(|r| r.orphaned_cost)
        .unwrap_or(0.0)
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
) -> Response {
    let run_id = RunId::new(id);
    if !run_exists(&state, &run_id) {
        return err(StatusCode::NOT_FOUND, "unknown run");
    }
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
                    return Some((
                        Ok::<_, std::convert::Infallible>(sse),
                        (state, run_id, seq, terminal),
                    ));
                }

                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        },
    );

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
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

    if !run_exists(&state, &run_id) {
        return err(StatusCode::NOT_FOUND, "unknown run");
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
            let models = crate::panel::models_contributed_by(&provider);
            if provider.as_str() == "mock" {
                return serde_json::json!({
                    "id": provider.as_str(),
                    "state": "not_required",
                    "source": null,
                    "fingerprint": null,
                    "usable": true,
                    // `mock` is a whole synthetic roster behind one name, so
                    // ticking its box adds 3 models, not 1. Screen 1 sums this
                    // to index the estimate table.
                    "models": models,
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
                        // P4 shipped the real adapters, so a resolvable key is
                        // now genuinely runnable. Still `present`, not
                        // `verified`: nothing has spent a request to prove the
                        // key works — that is what `providers test` is for.
                        "usable": true,
                        "models": models,
                    })
                }
                None => serde_json::json!({
                    "id": provider.as_str(),
                    "state": "missing",
                    "source": null,
                    "fingerprint": null,
                    "usable": false,
                    "models": models,
                }),
            }
        })
        .collect();

    // Models, not providers: `mock` alone is a three-model panel, so counting
    // ticked boxes would under-state every estimate it appears in.
    let usable_models: usize = rows
        .iter()
        .filter(|r| r["usable"].as_bool().unwrap_or(false))
        .map(|r| r["models"].as_u64().unwrap_or(1) as usize)
        .sum();

    // One estimate per possible panel size, 0..=usable, rather than a single
    // number for the whole roster. Screen 1 lets the operator pick which
    // providers sit on the panel (P4b), and U7 forbids the page computing a
    // number of its own — so the server precomputes every answer the picker
    // can produce and the page does a lookup. `estimates.standard`/`.deep`
    // stay exactly as they were: the whole-roster figure, shown before any
    // box is touched.
    let table = |depth: Depth| -> serde_json::Value {
        serde_json::Value::Array(
            (0..=usable_models)
                .map(|n| run_estimate(depth, n))
                .collect(),
        )
    };

    Json(serde_json::json!({
        "providers": rows,
        "estimates": {
            "standard": run_estimate(Depth::Standard, usable_models),
            "deep": run_estimate(Depth::Deep, usable_models),
            "per_model_count": {
                "standard": table(Depth::Standard),
                "deep": table(Depth::Deep),
            },
        },
    }))
    .into_response()
}

/// Screen 1's own "shown before the button, recomputed when the panel ...
/// changes" (U2) — computed here, server-side, so the page itself never
/// computes a number (U7's own hard requirement) even though no
/// `Stage::cost_estimate` can run yet (every one of them needs real
/// upstream input this endpoint doesn't have before a run exists). A
/// worst-case call count built from the exact same flat per-call
/// constants `spawn_run`'s own pipeline construction uses
/// (`CALL_COST`/`EXCHANGE_COST`/`JUDGE_RESERVATION`, `orchestrator.rs`),
/// not re-derived arithmetic of its own -- see PLAN_DEVIATIONS.md D49 for
/// why this is an approximation, not a literal replay of what a run will
/// actually spend.
///
/// `usable_count`, not `mock_panel().len()` unconditionally: only mock is
/// ever usable in this build (no P4 adapters, D46), so today `usable_count`
/// is always either 0 or 1, but sizing the estimate from it rather than a
/// hardcoded constant is what makes "the estimate ... must fall when
/// models are unusable" (U2) a real, testable behaviour instead of an
/// aspiration nothing in this build's own data flow could ever falsify.
/// `mock`'s own fixed 3-model panel is the per-usable-provider unit until
/// a real per-provider model count exists to read instead.
fn run_estimate(depth: Depth, model_count: usize) -> serde_json::Value {
    // The panel is however many models will actually run, not the mock
    // panel's fixed 3: before P4 every panel *was* the mock panel, so
    // borrowing its length was accurate; a real roster of 2 or 5 makes that
    // figure wrong in whichever direction costs the operator more.
    let panel_len = model_count as f64;
    // One judge, and only if anything is running at all (`panel.resolve`
    // gives the judge seat to the first-listed provider).
    let judges_len = if model_count == 0 { 0.0 } else { 1.0 };
    let bounds = arbiter_kernel::bounds::Bounds::for_depth(depth);
    let rounds = bounds.max_rounds as f64;

    let calls = panel_len // positions.generate
        + panel_len * 2.0 // claims.extract: extraction + worst-case repair
        + (if panel_len > 0.0 { 1.0 } else { 0.0 }) // claims.normalize (a single batch, the common case)
        + panel_len + (if panel_len > 0.0 { 1.0 } else { 0.0 }) // options.cluster: one cluster call + attach batches
        + (if panel_len > 0.0 { 1.0 } else { 0.0 }) // relations.analyze
        + rounds * (panel_len * 2.0) // per round: challenge.run + rebuttal.run
        + judges_len; // judge.evaluate, once, after the round loop

    let cost = calls * crate::orchestrator::CALL_COST.0;

    serde_json::json!({
        "cost": cost,
        "calls": calls as u64,
        "wall_clock_secs": bounds.max_wall_time_secs,
        "model_count": model_count,
    })
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
