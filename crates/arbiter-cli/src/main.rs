//! `arbiter` — the only frontend in phase 1. A renderer, not a second engine:
//! every number it prints comes out of `arbiter-core` (ARCHITECTURE.md §12).

mod accept;
mod maintenance;
mod orchestrator;
mod panel;
mod render;
mod resume_replay;
mod run_handle;
mod serve;
mod synthetic;
mod verify;

use arbiter_core::{Policy, RunId};
use arbiter_kernel::bounds::{Bounds, Depth};
use arbiter_kernel::prompt::PromptPack;
use arbiter_kernel::store::{Cost as KernelCost, Manifest, RunStore};
use arbiter_store::sqlite_store::SqliteRunStore;
use clap::{Parser, Subcommand};
use orchestrator::{PipelineConfig, run_pipeline};
use run_handle::RunHandle;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(name = "arbiter", version, about = "The AI debate & decision engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run a new debate over a question (a literal string, or a path to a
    /// file containing one).
    Run {
        question: String,
        /// Comma-separated providers, each optionally pinning a model
        /// (`anthropic,openai:gpt-4o,gemini`), or the literal `mock` to run
        /// the whole pipeline against a synthetic in-process panel with no
        /// keys and no network at all. A named provider with no resolvable
        /// key is an error, never a silent drop.
        #[arg(long, default_value = "mock")]
        panel: String,
        #[arg(long, value_enum, default_value = "standard")]
        depth: DepthArg,
        /// Overrides the default $2.00 hard cost cap.
        #[arg(long)]
        budget: Option<f64>,
        /// Print the final `DecisionRecord` as one JSON line instead of a
        /// human-readable summary.
        #[arg(long)]
        json: bool,
        /// Additionally stream every event envelope as NDJSON to stdout as
        /// the run progresses, before the final decision line.
        #[arg(long)]
        stream: bool,
        /// Root directory holding every run's own subdirectory
        /// (`<root>/<run_id>/run.db`).
        #[arg(long, default_value = ".arbiter/runs")]
        store: PathBuf,
    },
    /// Show one run — the decision (default), its claims, or its raw event
    /// transcript.
    Show {
        run_id: String,
        #[arg(long, conflicts_with_all = ["transcript"])]
        claims: bool,
        #[arg(long, conflicts_with_all = ["claims", "transcript"])]
        decision: bool,
        #[arg(long, conflicts_with_all = ["claims", "decision"])]
        transcript: bool,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = ".arbiter/runs")]
        store: PathBuf,
    },
    /// Confidence terms, defeat chains and change triggers for a run, or one
    /// claim within it.
    Explain {
        run_id: String,
        claim_id: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = ".arbiter/runs")]
        store: PathBuf,
    },
    /// List a run's claims, optionally filtered by standing.
    Claims {
        run_id: String,
        #[arg(long, value_enum)]
        state: Option<StateArg>,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = ".arbiter/runs")]
        store: PathBuf,
    },
    /// Continue an interrupted run: releases reservations that never left
    /// the machine, reports orphaned spend, then finishes the pipeline.
    Resume {
        run_id: String,
        #[arg(long)]
        json: bool,
        /// Overrides the original run's cost cap for the remainder.
        #[arg(long)]
        budget: Option<f64>,
        #[arg(long, default_value = ".arbiter/runs")]
        store: PathBuf,
    },
    /// Exact replay: re-derives a run's decision from its own cached
    /// responses, with no network access at all.
    Replay {
        run_id: String,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = ".arbiter/runs")]
        store: PathBuf,
    },
    /// Record a `DecisionAcceptance` for a run, optionally with overrides.
    Accept {
        run_id: String,
        /// A field path to override, `path=value`. Repeatable; each one
        /// needs a matching `--reason`.
        #[arg(long = "override")]
        overrides: Vec<String>,
        /// Required alongside each `--override`, in the same order.
        #[arg(long)]
        reason: Vec<String>,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = ".arbiter/runs")]
        store: PathBuf,
    },
    /// Preflight: constants, stuck runs, the budget ledger invariant,
    /// orphaned spend, orphaned blobs.
    Doctor {
        /// Also delete orphaned blobs from every run whose lease is dead.
        #[arg(long)]
        gc: bool,
        #[arg(long, default_value = ".arbiter/runs")]
        store: PathBuf,
    },
    /// Rebuild `history.db` by scanning every run under `--store`.
    Reindex {
        #[arg(long, default_value = ".arbiter/runs")]
        store: PathBuf,
    },
    /// Render a run's decision into `<run_dir>/exports/`.
    Export {
        run_id: String,
        #[arg(long)]
        format: String,
        #[arg(long, default_value = ".arbiter/runs")]
        store: PathBuf,
    },
    /// Credential sources: which providers have a key, and from where.
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },
    /// The provider roster and its per-provider key state.
    Providers {
        #[command(subcommand)]
        action: ProvidersAction,
    },
    /// The minimal loopback UI: one embedded page, bound to 127.0.0.1 only
    /// (ARCHITECTURE §17.1).
    Serve {
        /// Refused if given anything other than `127.0.0.1` — binding any
        /// other address is a hard error, never a warning.
        #[arg(long)]
        bind: Option<String>,
        /// `0` (the default) asks the OS for any free loopback port.
        #[arg(long, default_value_t = 0)]
        port: u16,
        #[arg(long, default_value = ".arbiter/runs")]
        store: PathBuf,
        /// Best-effort: opens the printed URL in the default browser.
        #[arg(long)]
        open: bool,
    },
    /// List past runs from the history catalogue.
    History {
        #[arg(long)]
        outcome: Option<String>,
        /// An RFC3339 timestamp; only runs started at or after this are
        /// shown.
        #[arg(long)]
        since: Option<String>,
        #[arg(long = "min-confidence")]
        min_confidence: Option<f64>,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value = ".arbiter/runs")]
        store: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
enum KeysAction {
    /// Which providers have a key, from which source, and its state.
    List,
    /// Read a key from stdin and store it in the OS keychain.
    Set { provider: String },
    /// One minimal request per model; caches the result for 24h.
    Test { provider: Option<String> },
    /// Remove the keychain entry.
    Rm { provider: String },
}

#[derive(Subcommand, Debug)]
enum ProvidersAction {
    List,
    Test { provider: Option<String> },
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum StateArg {
    Agreed,
    Disputed,
    Unresolved,
    Defeated,
}

impl From<StateArg> for arbiter_core::ClaimStanding {
    fn from(s: StateArg) -> Self {
        match s {
            StateArg::Agreed => arbiter_core::ClaimStanding::Agreed,
            StateArg::Disputed => arbiter_core::ClaimStanding::Disputed,
            StateArg::Unresolved => arbiter_core::ClaimStanding::Unresolved,
            StateArg::Defeated => arbiter_core::ClaimStanding::Defeated,
        }
    }
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
enum DepthArg {
    Standard,
    Deep,
}

impl From<DepthArg> for Depth {
    fn from(d: DepthArg) -> Self {
        match d {
            DepthArg::Standard => Depth::Standard,
            DepthArg::Deep => Depth::Deep,
        }
    }
}

/// The fixed, deterministic three-model synthetic panel. Still here and still
/// the default, because every CI fixture, `replay`, and every offline demo
/// depends on it — but no longer the *only* panel: P4's real adapters made
/// `--panel anthropic,openai,...` work, resolved by [`panel::resolve`].
pub(crate) use panel::mock_panel;

/// `pub(crate)`: `serve`'s own `POST /api/runs` (U2's "a file path is
/// accepted too") resolves the question the same way `arbiter run` does,
/// through this one function, rather than a second copy of the same
/// file-vs-literal check.
pub(crate) fn resolve_question(arg: &str) -> anyhow::Result<String> {
    let path = Path::new(arg);
    if path.is_file() {
        Ok(std::fs::read_to_string(path)?)
    } else {
        Ok(arg.to_string())
    }
}

pub(crate) fn prompts_dir() -> PathBuf {
    // No spec section pins this resolution order (PLAN_DEVIATIONS.md D42):
    // an env override first, then the workspace-relative dev path this
    // binary's own `CARGO_MANIFEST_DIR` was compiled against.
    if let Ok(dir) = std::env::var("ARBITER_PROMPTS_DIR") {
        return PathBuf::from(dir);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../prompts/default/v1")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            question,
            panel,
            depth,
            budget,
            json,
            stream,
            store,
        } => run_command(question, panel, depth.into(), budget, json, stream, store).await,
        Command::Show {
            run_id,
            claims,
            decision: _,
            transcript,
            json,
            store,
        } => {
            let view = if claims {
                ShowView::Claims
            } else if transcript {
                ShowView::Transcript
            } else {
                ShowView::Decision
            };
            show_command(RunId::new(run_id), view, json, store)
        }
        Command::Explain {
            run_id,
            claim_id,
            json,
            store,
        } => explain_command(
            RunId::new(run_id),
            claim_id.map(arbiter_core::ClaimId::new),
            json,
            store,
        ),
        Command::Claims {
            run_id,
            state,
            json,
            store,
        } => claims_command(RunId::new(run_id), state, json, store),
        Command::Resume {
            run_id,
            json,
            budget,
            store,
        } => resume_replay::resume_command(RunId::new(run_id), json, budget, store).await,
        Command::Replay {
            run_id,
            json,
            store,
        } => resume_replay::replay_command(RunId::new(run_id), json, store).await,
        Command::Accept {
            run_id,
            overrides,
            reason,
            json,
            store,
        } => accept::accept_command(RunId::new(run_id), overrides, reason, json, store),
        Command::Doctor { gc, store } => maintenance::doctor_command(gc, store),
        Command::Reindex { store } => maintenance::reindex_command(store),
        Command::Export {
            run_id,
            format,
            store,
        } => maintenance::export_command(RunId::new(run_id), format, store),
        Command::Keys { action } => match action {
            KeysAction::List => maintenance::keys_list_command(),
            KeysAction::Set { provider } => maintenance::keys_set_command(provider),
            KeysAction::Test { provider } => maintenance::keys_test_command(provider).await,
            KeysAction::Rm { provider } => maintenance::keys_rm_command(provider),
        },
        Command::Providers { action } => match action {
            ProvidersAction::List => maintenance::providers_list_command(),
            ProvidersAction::Test { provider } => {
                maintenance::providers_test_command(provider).await
            }
        },
        Command::Serve {
            bind,
            port,
            store,
            open,
        } => serve::serve_command(bind, port, store, open).await,
        Command::History {
            outcome,
            since,
            min_confidence,
            json,
            store,
        } => history_command(outcome, since, min_confidence, json, store),
    }
}

/// `store_root` (`--store`, default `.arbiter/runs`) is where every run's own
/// directory lives; `history.db` is its sibling, matching ARCHITECTURE §8's
/// own layout (`~/.arbiter/{history.db, runs/<run_id>/run.db}`) one level
/// down from wherever `--store` is rooted.
pub(crate) fn history_db_path(store_root: &Path) -> PathBuf {
    store_root
        .parent()
        .map(|p| p.join("history.db"))
        .unwrap_or_else(|| PathBuf::from("history.db"))
}

#[allow(clippy::too_many_arguments)]
async fn run_command(
    question_arg: String,
    panel_arg: String,
    depth: Depth,
    budget: Option<f64>,
    json: bool,
    stream: bool,
    store_root: PathBuf,
) -> anyhow::Result<()> {
    // P4: any provider with a resolvable key is runnable now, not just mock.
    let panel::ResolvedPanel {
        panel,
        judges,
        providers,
    } = panel::resolve(&panel_arg)?;

    let question = resolve_question(&question_arg)?;

    let mut bounds = Bounds::for_depth(depth);
    if let Some(b) = budget {
        bounds.max_cost = KernelCost(b);
    }

    let pack = PromptPack::load(&prompts_dir())
        .map_err(|e| anyhow::anyhow!("loading prompt pack from {:?}: {e}", prompts_dir()))?;

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
        // panel.resolve (the correlation-table consumer) is not part of this
        // pipeline's explicit-panel path (G2's own scope note) -- there is
        // no correlation table for an explicit `--panel` run to pin a
        // version of yet.
        correlation_table_version: "none".to_string(),
        rng_seed,
    };

    let sqlite_store = SqliteRunStore::new(&store_root);
    let writer = arbiter_store::init::init(&sqlite_store, &run_id, &question, &manifest)?;
    // `init` seals and appends RUN_STARTED against its own, separate
    // `ChainState` -- this handle's own chain must continue from that
    // event's real hash, or its first append would wrongly claim "no
    // predecessor" a second time and break `verify_chain`.
    let run_started = sqlite_store
        .reader(&run_id)?
        .events()?
        .last()
        .ok_or_else(|| anyhow::anyhow!("run store did not record RUN_STARTED"))?;
    let handle = RunHandle::new(run_id.clone(), writer).continuing_from(Some(&run_started));

    // `arbiter history` (L2) reads this catalogue -- L1 never wrote to it,
    // since its own scope was `run` alone (PLAN_DEVIATIONS.md D43). Best
    // effort: a run that cannot open `history.db` (e.g. a read-only
    // filesystem) still completes and is still fully replayable from its own
    // `run.db` -- only its catalogue entry is missing, exactly the gap
    // `arbiter reindex` (S6) exists to repair.
    let history_conn = arbiter_store::catalog::open_history_db(
        &history_db_path(&store_root),
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
                run_path: store_root
                    .join(run_id.as_str())
                    .to_string_lossy()
                    .to_string(),
            },
        );
    }
    let run_started_at = std::time::Instant::now();

    let cfg = PipelineConfig {
        run_id: run_id.clone(),
        question,
        panel,
        judges,
        bounds,
        policy,
        rng_seed,
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
    };

    if stream {
        eprintln!(
            "(--stream requested; event lines are not yet mirrored to stdout live -- \
                    the full event log is durably recorded and readable via the store, \
                    PLAN_DEVIATIONS.md D42)"
        );
    }

    let budget = arbiter_kernel::budget::BudgetLedger::new(Some(cfg.bounds.max_cost));
    let cache = arbiter_kernel::cache::ResponseCache::new();
    let result = run_pipeline(&cfg, &pack, &providers, &handle, &budget, &cache).await;
    let duration_ms = run_started_at.elapsed().as_millis() as i64;

    // The only place `cache_entries` is ever written to (PLAN_DEVIATIONS.md
    // D44) -- every `Stage`'s own `ctx.cache.put()` call only updates this
    // process's in-memory view. Persisted regardless of outcome: even a
    // failed run's completed calls are real cache hits a later `resume`
    // should not have to re-pay for.
    for (key, response) in cache.snapshot() {
        handle.put_cache_entry(&key, &response)?;
    }

    match &result {
        Ok(synthesized) => {
            handle.append_lifecycle_event(
                arbiter_kernel::event::EventType::RunCompleted,
                serde_json::json!({"outcome": format!("{:?}", synthesized.record.outcome)}),
            )?;
            if let Some(conn) = &history_conn {
                let record = &synthesized.record;
                // The `decision_margin` dimension's own `value` *is* the
                // margin between top1 and top2 (confidence's own reading of
                // it, C6) -- reused rather than re-derived from `options`
                // (PLAN_DEVIATIONS.md D43). `cost`/`orphaned_cost` are left
                // at 0: no aggregate budget reader exists yet to source them
                // from, the same honest gap `reindex` (S6) already leaves
                // for columns it cannot yet derive.
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
            handle.append_lifecycle_event(
                arbiter_kernel::event::EventType::RunFailed,
                serde_json::json!({"error": e.to_string()}),
            )?;
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

    let synthesized = result?;

    if json {
        println!("{}", serde_json::to_string(&synthesized.record)?);
    } else {
        print_human(&synthesized);
    }

    Ok(())
}

pub(crate) fn print_human(
    synthesized: &arbiter_kernel::stages::decision_synthesize::SynthesizedDecision,
) {
    use arbiter_kernel::stages::decision_synthesize::Completeness;

    let record = &synthesized.record;
    println!("Outcome: {:?}", record.outcome);
    match &record.recommendation {
        Some(r) => println!("Recommendation: {} ({})", r.label, r.option_id.as_str()),
        None => println!("Recommendation: none"),
    }
    println!("Confidence: {:.2}", record.confidence.total);
    println!(
        "Claims: {} agreed, {} disputed, {} unresolved, {} defeated",
        record.claims.agreed,
        record.claims.disputed,
        record.claims.unresolved,
        record.claims.defeated
    );
    match &synthesized.completeness {
        Completeness::Complete => println!("Completeness: complete"),
        Completeness::Truncated { reason, .. } => {
            println!("Completeness: truncated ({reason:?})")
        }
    }
}

enum ShowView {
    Decision,
    Claims,
    Transcript,
}

fn show_command(
    run_id: RunId,
    view: ShowView,
    json: bool,
    store_root: PathBuf,
) -> anyhow::Result<()> {
    let reader = render::open_reader(&store_root, &run_id)?;
    match view {
        ShowView::Decision => {
            let record = render::read_decision_record(reader.as_ref())?;
            let completeness = render::read_completeness(reader.as_ref())?;
            if json {
                println!("{}", serde_json::to_string(&record)?);
            } else {
                println!("Run: {}", record.run_id.as_str());
                println!("Question: {}", record.question);
                println!("Outcome: {:?}", record.outcome);
                match &record.recommendation {
                    Some(r) => println!("Recommendation: {} ({})", r.label, r.option_id.as_str()),
                    None => println!("Recommendation: none"),
                }
                println!("Confidence: {:.2}", record.confidence.total);
                println!(
                    "Claims: {} agreed, {} disputed, {} unresolved, {} defeated",
                    record.claims.agreed,
                    record.claims.disputed,
                    record.claims.unresolved,
                    record.claims.defeated
                );
                match completeness.reason {
                    Some(reason) => println!("Completeness: {} ({reason})", completeness.status),
                    None => println!("Completeness: {}", completeness.status),
                }
            }
        }
        ShowView::Claims => print_claims(reader.as_ref(), None, json)?,
        ShowView::Transcript => {
            let events: Vec<arbiter_kernel::event::Event> = reader
                .events()
                .map_err(|e| anyhow::anyhow!("reading events: {e}"))?
                .collect();
            if json {
                println!("{}", serde_json::to_value(&events)?);
            } else {
                for e in &events {
                    println!(
                        "{} [{}] {:?} {}",
                        e.timestamp,
                        e.stage.as_str(),
                        e.event_type,
                        e.payload
                    );
                }
            }
        }
    }
    Ok(())
}

fn print_claims(
    reader: &dyn arbiter_kernel::store::RunReader,
    filter: Option<StateArg>,
    json: bool,
) -> anyhow::Result<()> {
    let record = render::read_decision_record(reader)?;
    let graph = render::read_final_graph(reader)?;
    let mut rows = render::claim_rows(&record, &graph);
    if let Some(state) = filter {
        let want: arbiter_core::ClaimStanding = state.into();
        rows.retain(|r| r.standing == want);
    }
    if json {
        println!("{}", serde_json::to_string(&rows)?);
    } else if rows.is_empty() {
        println!("(no claims match)");
    } else {
        for row in &rows {
            println!(
                "[{:?}] {} ({}): {}",
                row.standing,
                row.id.as_str(),
                row.kind,
                row.text
            );
        }
    }
    Ok(())
}

fn claims_command(
    run_id: RunId,
    state: Option<StateArg>,
    json: bool,
    store_root: PathBuf,
) -> anyhow::Result<()> {
    let reader = render::open_reader(&store_root, &run_id)?;
    print_claims(reader.as_ref(), state, json)
}

fn explain_command(
    run_id: RunId,
    claim_id: Option<arbiter_core::ClaimId>,
    json: bool,
    store_root: PathBuf,
) -> anyhow::Result<()> {
    let reader = render::open_reader(&store_root, &run_id)?;
    let record = render::read_decision_record(reader.as_ref())?;
    let graph = render::read_final_graph(reader.as_ref())?;
    let output = render::build_explain(&record, &graph, claim_id.as_ref());

    if json {
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    println!(
        "Run: {}  Policy: {}",
        output.run_id.as_str(),
        output.policy_version
    );
    match &output.subject.id {
        Some(id) => println!("Subject: claim {}", id.as_str()),
        None => println!("Subject: decision"),
    }
    println!(
        "\nConfidence: {:.4} (base {:.4})",
        output.confidence.total, output.confidence.base
    );
    for d in &output.confidence.dimensions {
        println!(
            "  + {:<16} value {:.2} x weight {:.2} = {:+.4}",
            d.name, d.value, d.weight, d.contribution
        );
    }
    for p in &output.confidence.penalties {
        let note = p
            .note
            .as_deref()
            .map(|n| format!("  ({n})"))
            .unwrap_or_default();
        println!("  - {:<16} {:+.4}{note}", p.name, p.contribution);
    }

    if !output.defeat_chains.is_empty() {
        println!("\nDefeat chains:");
        for chain in &output.defeat_chains {
            let text = graph
                .claim_text(&chain.claim_id)
                .unwrap_or("<unknown claim>");
            println!(
                "  {} (standing {:.2}{}): {}",
                chain.claim_id.as_str(),
                chain.standing,
                if chain.saturated { ", saturated" } else { "" },
                text
            );
            for step in &chain.steps {
                println!(
                    "    {:+.4} <- {} ({:?}, weight {:.2}, standing {:.2})",
                    step.delta,
                    step.by.as_str(),
                    step.relation,
                    step.weight,
                    step.attacker_standing
                );
            }
        }
    }

    if !output.change_triggers.is_empty() {
        println!("\nChange triggers:");
        for t in &output.change_triggers {
            println!(
                "  {} {:?} -> {:?}",
                t.claim_id.as_str(),
                t.direction,
                t.new_winner.as_ref().map(|o| o.as_str())
            );
        }
    }

    println!("\nOptions:");
    for o in &output.options {
        println!(
            "  {} ({}) share {:.2} -- supports {:?}, opposes {:?}",
            o.label,
            o.id.as_str(),
            o.share,
            o.supported_by
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>(),
            o.opposed_by.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        );
    }

    Ok(())
}

fn history_command(
    outcome: Option<String>,
    since: Option<String>,
    min_confidence: Option<f64>,
    json: bool,
    store_root: PathBuf,
) -> anyhow::Result<()> {
    let conn = arbiter_store::catalog::open_history_db(
        &history_db_path(&store_root),
        env!("CARGO_PKG_VERSION"),
        &arbiter_store::now_rfc3339(),
    )
    .map_err(|e| anyhow::anyhow!("opening history.db: {e}"))?;
    let rows = arbiter_store::catalog::list_runs(
        &conn,
        &arbiter_store::catalog::HistoryFilter {
            outcome,
            since,
            min_confidence,
        },
    )
    .map_err(|e| anyhow::anyhow!("querying history.db: {e}"))?;

    if json {
        let json_rows: Vec<serde_json::Value> = rows
            .iter()
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
        println!("{}", serde_json::to_string(&json_rows)?);
    } else if rows.is_empty() {
        println!("(no runs recorded)");
    } else {
        for r in &rows {
            println!(
                "{}  {:<10} {:<20} {:<8} conf={:<5} {}",
                r.started_at,
                r.status,
                r.outcome.as_deref().unwrap_or("-"),
                r.depth.as_deref().unwrap_or("-"),
                r.confidence
                    .map(|c| format!("{c:.2}"))
                    .unwrap_or_else(|| "-".to_string()),
                r.question,
            );
        }
    }
    Ok(())
}
