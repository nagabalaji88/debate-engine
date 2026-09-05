//! `arbiter serve` (U1, ARCHITECTURE §17.1 / INTERFACES §24): one embedded
//! HTML page and the loopback API it talks to. **It is a renderer, not a
//! second engine** — every handler either calls straight into
//! `arbiter-store`/`arbiter-core` the same way the CLI's own `render.rs`
//! does, or spawns the same `run_pipeline` `arbiter run` uses. Nothing here
//! computes a decision term of its own.
//!
//! Security is not a default here, it is a requirement (§17.1's own table):
//! this is the first and only thing in this workspace that opens a socket,
//! and `POST /api/runs` spends real money. Every request — `GET /`
//! included — passes through [`admission`] before any handler, any route
//! lookup even, ever reads run state; a probe that guesses a run id learns
//! nothing it could not already see, because rejection happens identically
//! whether that run exists or not.

mod admission;
mod compare;
mod handlers;
mod page;

use crate::run_handle::RunHandle;
use arbiter_core::{Policy, RunId};
use arbiter_kernel::bounds::{Bounds, Depth};
use arbiter_kernel::prompt::PromptPack;
use arbiter_kernel::stage::ProviderRegistry;
use arbiter_kernel::store::{Manifest, RunStore};
use arbiter_store::sqlite_store::SqliteRunStore;
use axum::Router;
use axum::routing::{get, post};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

/// The one loopback address this server will ever bind — ARCHITECTURE
/// §17.1's own first requirement, enforced here rather than merely
/// documented: any other address is a hard refusal, not a warning
/// (`serve_localhost_only`, F2).
const LOOPBACK: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

/// Shared, read-only server state — every handler and [`admission`] borrow
/// this via `axum::extract::State`. Holds the store paths and the
/// per-process admission secret; nothing here is mutated after `serve`
/// starts (a run's own state lives in its `run.db`, not in this struct).
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) store_root: PathBuf,
    /// 128-bit, minted once per process at startup (`admission::mint_token`)
    /// — never persisted anywhere, never logged, compared in constant time
    /// against every request (`token_absent_from_store_and_log`, F2).
    pub(crate) token: Arc<str>,
    /// `http://127.0.0.1:<port>`, this server's own origin — the one value
    /// `Origin`/`Sec-Fetch-Site` admission compares an inbound request
    /// against.
    pub(crate) origin: Arc<str>,
}

/// `arbiter serve [--bind ADDR] [--port N] [--store DIR] [--open]`.
/// `bind_addr`, parsed by the caller, is checked here rather than trusted:
/// refusing to start is the "REFUSED, not warned" ARCHITECTURE §17.1 asks
/// for, and the caller (`main.rs`) has no other chokepoint that would catch
/// a typo'd `--bind 0.0.0.0`.
pub async fn serve_command(
    bind_addr: Option<String>,
    port: u16,
    store_root: PathBuf,
    open: bool,
) -> anyhow::Result<()> {
    let addr = match bind_addr.as_deref() {
        None | Some("127.0.0.1") => LOOPBACK,
        Some(other) => anyhow::bail!(
            "refusing to bind {other}: arbiter serve only ever binds 127.0.0.1 (ARCHITECTURE \
             §17.1) -- this machine holds provider credentials and can spend money, so a \
             non-loopback bind is refused outright, never merely warned about"
        ),
    };

    let listener = tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(addr), port))
        .await
        .map_err(|e| anyhow::anyhow!("binding 127.0.0.1:{port}: {e}"))?;
    let bound_port = listener.local_addr()?.port();

    let token = admission::mint_token()?;
    let origin: Arc<str> = format!("http://127.0.0.1:{bound_port}").into();
    let state = AppState {
        store_root,
        token: token.clone(),
        origin: origin.clone(),
    };

    let url = format!("{origin}/?token={token}");
    println!("arbiter serve listening on {origin}");
    println!("Open: {url}");
    if open {
        let _ = webbrowser_open(&url);
    }

    let app = router(state);
    axum::serve(listener, app)
        .await
        .map_err(|e| anyhow::anyhow!("serve: {e}"))
}

/// No browser-launching crate is a workspace dependency (adding one for a
/// single best-effort convenience call would be exactly the unrequested
/// weight this project's own discipline avoids) — `xdg-open`/`open`/`start`
/// cover the three desktop platforms this is ever likely to run on
/// interactively; a failure here is silently ignored by the caller, since
/// the URL is already printed either way.
fn webbrowser_open(url: &str) -> std::io::Result<std::process::Child> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", url])
            .spawn()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()
    }
}

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(page::index))
        .route(
            "/api/runs",
            post(handlers::create_run).get(handlers::list_runs),
        )
        .route("/api/runs/{id}", get(handlers::get_run))
        .route("/api/runs/{id}/events", get(handlers::run_events))
        .route("/api/runs/{id}/accept", post(handlers::accept_run))
        .route("/api/providers", get(handlers::list_providers))
        // Screen 6. Like `POST /api/runs` this one spends money, so it sits
        // behind the same admission middleware as everything else below.
        .route("/api/compare", post(compare::compare))
        .route(
            "/api/providers/{provider}/test",
            post(handlers::test_provider),
        )
        // Free to call and free to answer: a model list costs nothing at any
        // vendor, which is the same property that makes it the first half of
        // key verification.
        .route(
            "/api/providers/{provider}/models",
            get(handlers::list_provider_models),
        )
        .route(
            "/api/providers/{provider}/key",
            post(handlers::set_provider_key),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admission::admission,
        ))
        .with_state(state)
}

/// Spawns `run_pipeline` in the background against `panel_spec` — the same
/// spec `arbiter run --panel` takes, resolved through the same
/// [`crate::panel::resolve`], so the UI and the CLI can never disagree about
/// what a panel string means. Returns the fresh
/// `RunId` once `init` has durably recorded `RUN_STARTED` — the caller can
/// return `202` the instant this returns, since the run genuinely exists
/// in the store from that point on, even though `run_pipeline` itself is
/// still executing.
pub(crate) fn spawn_run(
    state: &AppState,
    question: String,
    depth: Depth,
    budget: Option<f64>,
    panel_spec: &str,
) -> anyhow::Result<RunId> {
    // P4: the panel is whatever the caller asked for, credential-resolved
    // here so an unusable provider fails the request outright rather than
    // failing mid-run, once money has already been spent on the models that
    // did resolve.
    let crate::panel::ResolvedPanel {
        panel,
        judges,
        providers,
    } = crate::panel::resolve(panel_spec)?;

    let mut bounds = Bounds::for_depth(depth);
    if let Some(b) = budget {
        bounds.max_cost = arbiter_kernel::store::Cost(b);
    }

    let pack = PromptPack::load(&crate::prompts_dir())
        .map_err(|e| anyhow::anyhow!("loading prompt pack from {:?}: {e}", crate::prompts_dir()))?;
    let policy = Policy::argument_v1();
    let config_hash = format!(
        "blake3:{}",
        blake3::hash(serde_json::to_string(&policy.config)?.as_bytes()).to_hex()
    );
    let rng_seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    let run_id = RunId::new(format!(
        "run_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));

    let manifest = Manifest {
        policy_version: policy.version.clone(),
        config_hash,
        pack_hash: pack.hash.to_string(),
        correlation_table_version: "none".to_string(),
        rng_seed,
    };

    let sqlite_store = SqliteRunStore::new(&state.store_root);
    let writer = arbiter_store::init::init(&sqlite_store, &run_id, &question, &manifest)?;
    let run_started = sqlite_store
        .reader(&run_id)?
        .events()?
        .last()
        .ok_or_else(|| anyhow::anyhow!("run store did not record RUN_STARTED"))?;
    let handle = RunHandle::new(run_id.clone(), writer).continuing_from(Some(&run_started));

    let history_conn = arbiter_store::catalog::open_history_db(
        &crate::history_db_path(&state.store_root),
        env!("CARGO_PKG_VERSION"),
        &arbiter_store::now_rfc3339(),
    )
    .ok();
    if let Some(conn) = &history_conn {
        let _ = arbiter_store::catalog::insert_running(
            conn,
            &arbiter_store::catalog::NewRun {
                run_id: run_id.as_str().to_string(),
                question: question.clone(),
                policy_version: policy.version.as_str().to_string(),
                started_at: arbiter_store::now_rfc3339(),
                run_path: state
                    .store_root
                    .join(run_id.as_str())
                    .to_string_lossy()
                    .to_string(),
            },
        );
    }

    let run_id_for_task = run_id.clone();
    let store_root = state.store_root.clone();
    tokio::spawn(async move {
        run_to_completion(
            handle,
            history_conn,
            run_id_for_task,
            store_root,
            pack,
            panel,
            judges,
            providers,
            bounds,
            policy,
            rng_seed,
            question,
            depth,
        )
        .await;
    });

    Ok(run_id)
}

#[allow(clippy::too_many_arguments)]
async fn run_to_completion(
    handle: RunHandle,
    history_conn: Option<rusqlite::Connection>,
    run_id: RunId,
    _store_root: PathBuf,
    pack: PromptPack,
    panel: Vec<(arbiter_core::ModelId, arbiter_core::ProviderId)>,
    judges: Vec<(arbiter_core::ModelId, arbiter_core::ProviderId)>,
    providers: ProviderRegistry,
    bounds: Bounds,
    policy: Policy,
    rng_seed: u64,
    question: String,
    depth: Depth,
) {
    let cfg = crate::orchestrator::PipelineConfig {
        run_id: run_id.clone(),
        question,
        panel,
        judges,
        bounds,
        policy,
        rng_seed,
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
    };

    let started_at = std::time::Instant::now();
    let budget = arbiter_kernel::budget::BudgetLedger::new(Some(cfg.bounds.max_cost));
    let cache = arbiter_kernel::cache::ResponseCache::new();
    let result =
        crate::orchestrator::run_pipeline(&cfg, &pack, &providers, &handle, &budget, &cache).await;
    let duration_ms = started_at.elapsed().as_millis() as i64;

    for (key, response) in cache.snapshot() {
        let _ = handle.put_cache_entry(&key, &response);
    }

    match &result {
        Ok(synthesized) => {
            let _ = handle.append_lifecycle_event(
                arbiter_kernel::event::EventType::RunCompleted,
                serde_json::json!({"outcome": format!("{:?}", synthesized.record.outcome)}),
            );
            if let Some(conn) = &history_conn {
                let record = &synthesized.record;
                let margin = record
                    .confidence
                    .dimensions
                    .iter()
                    .find(|d| d.name == "decision_margin")
                    .map(|d| d.value);
                let _ = arbiter_store::catalog::update_completion(
                    conn,
                    &arbiter_store::catalog::Completion {
                        run_id: run_id.as_str().to_string(),
                        status: "completed".to_string(),
                        outcome: Some(format!("{:?}", record.outcome)),
                        confidence: Some(record.confidence.total),
                        margin,
                        cost: 0.0,
                        orphaned_cost: 0.0,
                        duration_ms: Some(duration_ms),
                        model_count: Some(cfg.panel.len() as i64),
                        depth: Some(format!("{depth:?}")),
                        completed_at: arbiter_store::now_rfc3339(),
                    },
                );
            }
        }
        Err(e) => {
            let _ = handle.append_lifecycle_event(
                arbiter_kernel::event::EventType::RunFailed,
                serde_json::json!({"error": e.to_string()}),
            );
            if let Some(conn) = &history_conn {
                let _ = arbiter_store::catalog::update_completion(
                    conn,
                    &arbiter_store::catalog::Completion {
                        run_id: run_id.as_str().to_string(),
                        status: "failed".to_string(),
                        outcome: None,
                        confidence: None,
                        margin: None,
                        cost: 0.0,
                        orphaned_cost: 0.0,
                        duration_ms: Some(duration_ms),
                        model_count: Some(cfg.panel.len() as i64),
                        depth: Some(format!("{depth:?}")),
                        completed_at: arbiter_store::now_rfc3339(),
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
