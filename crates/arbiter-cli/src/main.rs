//! `arbiter` — the only frontend in phase 1. A renderer, not a second engine:
//! every number it prints comes out of `arbiter-core` (ARCHITECTURE.md §12).

mod orchestrator;
mod run_handle;
mod synthetic;

use arbiter_core::{ModelId, Policy, ProviderId, RunId};
use arbiter_kernel::bounds::{Bounds, Depth};
use arbiter_kernel::prompt::PromptPack;
use arbiter_kernel::stage::ProviderRegistry;
use arbiter_kernel::store::{Cost as KernelCost, Manifest, RunStore};
use arbiter_store::sqlite_store::SqliteRunStore;
use clap::{Parser, Subcommand};
use orchestrator::{PipelineConfig, run_pipeline};
use run_handle::RunHandle;
use std::path::{Path, PathBuf};
use synthetic::SyntheticProvider;

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
        /// Comma-separated model names, or the literal `mock` to run the
        /// whole pipeline against a synthetic in-process panel with no
        /// network access at all (real provider adapters are P3/P4,
        /// deferred — see PLAN_DEVIATIONS.md D42).
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

/// A model/provider roster, as used for both the debate panel and the judge
/// panel.
type Roster = Vec<(ModelId, ProviderId)>;

/// A fixed, deterministic three-model synthetic panel — real credential
/// resolution and provider adapters (P3/P4) are out of scope for this task
/// (PLAN_DEVIATIONS.md D42); `--panel mock` is the one panel this binary can
/// actually run today.
fn mock_panel() -> (Roster, Roster, ProviderId) {
    let provider = ProviderId::new("mock");
    let panel = vec![
        (ModelId::new("model-a"), provider.clone()),
        (ModelId::new("model-b"), provider.clone()),
        (ModelId::new("model-c"), provider.clone()),
    ];
    let judges = vec![(ModelId::new("judge-1"), provider.clone())];
    (panel, judges, provider)
}

fn resolve_question(arg: &str) -> anyhow::Result<String> {
    let path = Path::new(arg);
    if path.is_file() {
        Ok(std::fs::read_to_string(path)?)
    } else {
        Ok(arg.to_string())
    }
}

fn prompts_dir() -> PathBuf {
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
    }
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
    if panel_arg != "mock" {
        anyhow::bail!(
            "only `--panel mock` is implemented in this build -- real provider adapters \
             (P3/P4) are not yet wired (PLAN_DEVIATIONS.md D42)"
        );
    }
    let (panel, judges, provider_id) = mock_panel();

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

    let mut providers = ProviderRegistry::new();
    providers.register(Box::new(SyntheticProvider::new(provider_id)));

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

    let result = run_pipeline(&cfg, &pack, &providers, &handle).await;

    match &result {
        Ok(synthesized) => {
            handle.append_lifecycle_event(
                arbiter_kernel::event::EventType::RunCompleted,
                serde_json::json!({"outcome": format!("{:?}", synthesized.record.outcome)}),
            )?;
        }
        Err(e) => {
            handle.append_lifecycle_event(
                arbiter_kernel::event::EventType::RunFailed,
                serde_json::json!({"error": e.to_string()}),
            )?;
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

fn print_human(synthesized: &arbiter_kernel::stages::decision_synthesize::SynthesizedDecision) {
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
