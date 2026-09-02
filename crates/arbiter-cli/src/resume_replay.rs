//! `arbiter resume`/`replay` (L3) — the two commands that read a run's own
//! persisted history back into a *fresh* process's in-memory state before
//! handing off to [`orchestrator::run_pipeline`], the same executor `arbiter
//! run` uses unmodified. Two structures a fresh process starts with none of
//! turned out to matter here: `ResponseCache` (rehydrated from
//! `cache_entries`, so `run_pipeline`'s own cache-before-call order (D31)
//! serves every already-answered call for free) and `BudgetLedger` (capped
//! at what is actually left of the hard cap, not the full amount again).
//! Neither had a caller before this file (PLAN_DEVIATIONS.md D44).

use arbiter_core::{Policy, RunId};
use arbiter_kernel::budget::BudgetLedger;
use arbiter_kernel::cache::ResponseCache;
use arbiter_kernel::calls::{ResumeAction, classify_on_resume};
use arbiter_kernel::event::EventType;
use arbiter_kernel::ids::{ArtifactId, CallId, Sequence};
use arbiter_kernel::provider::{
    Provider, ProviderCapabilities, ProviderError, ProviderRequest, ProviderResponse,
};
use arbiter_kernel::stage::ProviderRegistry;
use arbiter_kernel::store::{Artifact, Cost, RunReader, RunStore, RunWriter, StoreError, Tx};
use arbiter_store::sqlite_store::SqliteRunStore;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use crate::orchestrator::{PipelineConfig, run_pipeline};
use crate::run_handle::RunHandle;

/// What a run's own `RUN_STARTED` event recorded — everything `resume`/
/// `replay` need to reconstruct the same [`PipelineConfig`] the original
/// `run` built, short of the panel (see [`Reconstructed::panel_note`]).
struct Reconstructed {
    question: String,
    policy_version: String,
    pack_hash: String,
    rng_seed: u64,
}

fn read_run_started(reader: &dyn RunReader) -> anyhow::Result<Reconstructed> {
    let event = reader
        .events()
        .map_err(|e| anyhow::anyhow!("reading events: {e}"))?
        .find(|e| e.event_type == EventType::RunStarted)
        .ok_or_else(|| anyhow::anyhow!("run has no RUN_STARTED event"))?;
    let p = &event.payload;
    let field = |name: &str| -> anyhow::Result<String> {
        p.get(name)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("RUN_STARTED payload missing '{name}'"))
    };
    Ok(Reconstructed {
        question: field("question")?,
        policy_version: field("policy_version")?,
        pack_hash: field("pack_hash")?,
        rng_seed: p
            .get("rng_seed")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow::anyhow!("RUN_STARTED payload missing 'rng_seed'"))?,
    })
}

/// `depth` is not part of `RUN_STARTED`'s payload (`Manifest` never carried
/// it -- L1's own scope note) and no artifact carries it either; it only
/// ever reaches durable storage via `history.db`'s `run_catalog.depth`
/// column (L2's own write path). A missing or unwritten catalogue entry
/// (a best-effort write, L2's D43) degrades to `Standard` with a loud
/// warning rather than failing outright -- `Standard` is this codebase's
/// own default (PLAN_DEVIATIONS.md D44).
fn read_depth(store_root: &Path, run_id: &RunId) -> arbiter_kernel::bounds::Depth {
    let history_path = store_root
        .parent()
        .map(|p| p.join("history.db"))
        .unwrap_or_else(|| PathBuf::from("history.db"));
    let depth_str = arbiter_store::catalog::open_history_db(
        &history_path,
        env!("CARGO_PKG_VERSION"),
        &arbiter_store::now_rfc3339(),
    )
    .ok()
    .and_then(|conn| {
        conn.query_row(
            "SELECT depth FROM run_catalog WHERE run_id = ?1",
            [run_id.as_str()],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    });
    match depth_str.as_deref() {
        Some("Deep") => arbiter_kernel::bounds::Depth::Deep,
        Some("Standard") => arbiter_kernel::bounds::Depth::Standard,
        _ => {
            eprintln!(
                "(warning: this run's --depth is not recorded in history.db; \
                 defaulting to standard -- PLAN_DEVIATIONS.md D44)"
            );
            arbiter_kernel::bounds::Depth::Standard
        }
    }
}

fn rehydrate_cache(reader: &dyn RunReader) -> anyhow::Result<ResponseCache> {
    let cache = ResponseCache::new();
    for (key, response) in reader
        .cache_entries()
        .map_err(|e| anyhow::anyhow!("reading cache_entries: {e}"))?
    {
        cache.put(key, response);
    }
    Ok(cache)
}

fn build_config(
    run_id: RunId,
    reconstructed: &Reconstructed,
    depth: arbiter_kernel::bounds::Depth,
    budget_override: Option<f64>,
) -> anyhow::Result<PipelineConfig> {
    let policy = Policy::argument_v1();
    if policy.version.as_str() != reconstructed.policy_version {
        anyhow::bail!(
            "run was recorded under policy '{}' but this build only implements '{}' -- \
             re-deriving under a different policy is --repolicy, not yet implemented \
             (PLAN_DEVIATIONS.md D44)",
            reconstructed.policy_version,
            policy.version.as_str()
        );
    }

    let mut bounds = arbiter_kernel::bounds::Bounds::for_depth(depth);
    if let Some(b) = budget_override {
        bounds.max_cost = Cost(b);
    }

    let (panel, judges, _provider_id) = crate::mock_panel();

    Ok(PipelineConfig {
        run_id,
        question: reconstructed.question.clone(),
        panel,
        judges,
        bounds,
        policy,
        rng_seed: reconstructed.rng_seed,
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

fn check_pack(
    pack: &arbiter_kernel::prompt::PromptPack,
    reconstructed: &Reconstructed,
) -> anyhow::Result<()> {
    if pack.hash.to_string() != reconstructed.pack_hash {
        anyhow::bail!(
            "run was recorded under prompt pack {} but the pack loaded now hashes to {} -- \
             exact replay refuses a differing pack_hash (INTERFACES §23); --repack is not yet \
             implemented (PLAN_DEVIATIONS.md D44)",
            reconstructed.pack_hash,
            pack.hash
        );
    }
    Ok(())
}

/// Always errs — the whole point of replay: every provider call must be
/// served from the rehydrated cache, and a miss is a real problem to
/// surface, never a silent network fallback. Registered under the same
/// `ProviderId::new("mock")` `--panel mock` always uses, so a cache miss on
/// replay looks exactly like a cache miss would for a real run: a stage
/// error naming the provider, not a panic or a hang.
#[derive(Debug)]
struct ReplayProvider;

impl Provider for ReplayProvider {
    fn id(&self) -> arbiter_core::ProviderId {
        arbiter_core::ProviderId::new("mock")
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            structured_output: false,
            streaming: false,
            idempotency: None,
        }
    }
    fn call(
        &self,
        _request: ProviderRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderResponse, ProviderError>> + Send + '_>> {
        Box::pin(async {
            Err(ProviderError::Other(
                "replay mode: no cached response for this call -- exact replay never opens a \
                 network connection (ARCHITECTURE §7: \"exact replay is cache-only with the \
                 network disabled\")"
                    .to_string(),
            ))
        })
    }
}

/// Discards every write. `replay` must never mutate the run it is
/// replaying (ARCHITECTURE never describes replay as a second execution of
/// the *same* run id — `--repolicy`/`--repack` are the two operations that
/// mint a new one), so [`RunHandle`] is given a writer that accepts and
/// drops everything rather than a second connection onto the real
/// `run.db`.
#[derive(Debug)]
struct NullWriter;

impl RunWriter for NullWriter {
    fn transact(
        &mut self,
        f: &mut dyn FnMut(&mut dyn Tx) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        f(&mut NullTx)
    }
}

#[derive(Debug)]
struct NullTx;

impl Tx for NullTx {
    fn append_event(&mut self, _e: &arbiter_kernel::event::Event) -> Result<Sequence, StoreError> {
        Ok(Sequence::new(0))
    }
    fn reserve_call(
        &mut self,
        _call_id: &CallId,
        _reservation_id: &arbiter_kernel::ids::ReservationId,
        _reserved_amount: Cost,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn put_artifact(&mut self, a: &dyn Artifact) -> Result<ArtifactId, StoreError> {
        Ok(ArtifactId::new(a.content_hash()))
    }
    fn put_cache(
        &mut self,
        _k: &arbiter_kernel::store::CacheKey,
        _r: &arbiter_kernel::store::CachedResponse,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn commit_budget(
        &mut self,
        _r: &arbiter_kernel::ids::ReservationId,
        _actual: Cost,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn set_call_state(
        &mut self,
        _c: &CallId,
        _s: arbiter_kernel::provider::CallState,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    fn release_reservation(
        &mut self,
        _r: &arbiter_kernel::ids::ReservationId,
        _c: &CallId,
    ) -> Result<(), StoreError> {
        Ok(())
    }
}

pub async fn replay_command(run_id: RunId, json: bool, store_root: PathBuf) -> anyhow::Result<()> {
    let store = SqliteRunStore::new(&store_root);
    let reader = store
        .reader(&run_id)
        .map_err(|e| anyhow::anyhow!("opening run {}: {e}", run_id.as_str()))?;

    let reconstructed = read_run_started(reader.as_ref())?;
    let pack = arbiter_kernel::prompt::PromptPack::load(&crate::prompts_dir())
        .map_err(|e| anyhow::anyhow!("loading prompt pack: {e}"))?;
    check_pack(&pack, &reconstructed)?;

    let depth = read_depth(&store_root, &run_id);
    let cfg = build_config(run_id.clone(), &reconstructed, depth, None)?;

    let cache = rehydrate_cache(reader.as_ref())?;
    let budget = BudgetLedger::unbounded(); // cache-served; see module doc.

    let mut providers = ProviderRegistry::new();
    providers.register(Box::new(ReplayProvider));

    // `EventId`/chain identity here are throwaway (`NullWriter` discards
    // them) -- only `run_id` needs to match, since `Stage::idempotency_key`
    // folds it in (K3) and a mismatched id would silently miss every cache
    // entry despite them being present.
    let handle = RunHandle::new(run_id.clone(), Box::new(NullWriter)).continuing_from(None);

    let replayed = run_pipeline(&cfg, &pack, &providers, &handle, &budget, &cache)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "replay could not reproduce this run from its cached responses alone: {e}"
            )
        })?;

    // Not the acceptance test's own comparison (that is a shell-level
    // `diff` against `show --json`'s output) -- a second, cheap check this
    // command itself can make since it already has both records in hand: if
    // recomputation ever disagrees with what is actually stored, that is a
    // real integrity problem worth surfacing loudly rather than silently
    // printing a decision that quietly does not match history.
    if let Ok(stored) = crate::render::read_decision_record(reader.as_ref())
        && stored != replayed.record
    {
        eprintln!(
            "(warning: replayed decision differs from the one on record for {} -- \
             this should never happen under exact replay; investigate before trusting \
             either)",
            run_id.as_str()
        );
    }

    if json {
        println!("{}", serde_json::to_string(&replayed.record)?);
    } else {
        crate::print_human(&replayed);
    }
    Ok(())
}

pub async fn resume_command(
    run_id: RunId,
    json: bool,
    budget: Option<f64>,
    store_root: PathBuf,
) -> anyhow::Result<()> {
    let store = SqliteRunStore::new(&store_root);
    let reader = store
        .reader(&run_id)
        .map_err(|e| anyhow::anyhow!("opening run {}: {e}", run_id.as_str()))?;

    if reader
        .events()
        .map_err(|e| anyhow::anyhow!("reading events: {e}"))?
        .any(|e| matches!(e.event_type, EventType::RunCompleted | EventType::RunFailed))
    {
        println!(
            "Run {} has already finished -- nothing to resume.",
            run_id.as_str()
        );
        let record = crate::render::read_decision_record(reader.as_ref())?;
        if json {
            println!("{}", serde_json::to_string(&record)?);
        } else {
            println!("Outcome: {:?}", record.outcome);
        }
        return Ok(());
    }

    match reader
        .verify_chain()
        .map_err(|e| anyhow::anyhow!("verifying chain: {e}"))?
    {
        arbiter_kernel::store::ChainStatus::Intact => {}
        arbiter_kernel::store::ChainStatus::Broken { at } => {
            anyhow::bail!(
                "this run's event chain is broken at sequence {at:?} -- it cannot be safely \
                 resumed (ARCHITECTURE §9: a chain break is not repairable)"
            );
        }
    }

    let reconstructed = read_run_started(reader.as_ref())?;

    // One reopen for the whole command: `lease::reopen` treats a second
    // reopen by the same still-live owner as `AlreadyOpen` (it cannot tell
    // "another process holds it" from "I already reopened it a moment
    // ago"), so this writer -- not a fresh reopen per released reservation
    // -- is reused for every subsequent write in this function, including
    // the `RunHandle` built further down.
    let mut writer = store
        .reopen(&run_id)
        .map_err(|e| anyhow::anyhow!("reopening run {}: {e}", run_id.as_str()))?;

    // ARCHITECTURE §8.4's own resume table: RESERVED never left the machine
    // (release it), SENT/ACKNOWLEDGED may have been billed (hold, report --
    // "orphaned spend is reported, not absorbed"), everything else needs no
    // action. `classify_on_resume` (K3) had no caller before this.
    let mut released = 0usize;
    let mut orphaned: Vec<(String, f64)> = Vec::new();
    for call in reader
        .provider_calls()
        .map_err(|e| anyhow::anyhow!("reading provider_calls: {e}"))?
    {
        match classify_on_resume(call.state) {
            ResumeAction::ReleaseAndFail => {
                writer
                    .transact(&mut |tx| tx.release_reservation(&call.reservation_id, &call.call_id))
                    .map_err(|e| anyhow::anyhow!("releasing reservation: {e}"))?;
                released += 1;
            }
            ResumeAction::HoldOrphaned | ResumeAction::HoldOrphanedReconcilable => {
                orphaned.push((call.call_id.as_str().to_string(), call.reserved_amount.0));
            }
            ResumeAction::NoAction => {}
        }
    }
    if released > 0 {
        println!("Released {released} reservation(s) that never left the machine.");
    }
    if !orphaned.is_empty() {
        let total: f64 = orphaned.iter().map(|(_, amt)| amt).sum();
        println!(
            "Orphaned spend: ${total:.2} across {} call(s) -- may have been billed, held rather \
             than released (ARCHITECTURE §8.4: reported, never absorbed):",
            orphaned.len()
        );
        for (call_id, amt) in &orphaned {
            println!("  {call_id}: ${amt:.2}");
        }
    }

    let depth = read_depth(&store_root, &run_id);
    let (_reserved, committed) = reader
        .budget_totals()
        .map_err(|e| anyhow::anyhow!("reading budget totals: {e}"))?;
    let mut cfg = build_config(run_id.clone(), &reconstructed, depth, budget)?;
    // Cap further spend at what is actually left of the hard cap -- a
    // freshly constructed `BudgetLedger` otherwise has no memory of what
    // this run already committed before it was interrupted, and would let a
    // resumed run spend the full cap a second time on top of that
    // (PLAN_DEVIATIONS.md D44).
    let remaining = (cfg.bounds.max_cost.0 - committed.0).max(0.0);
    cfg.bounds.max_cost = Cost(remaining);

    let pack = arbiter_kernel::prompt::PromptPack::load(&crate::prompts_dir())
        .map_err(|e| anyhow::anyhow!("loading prompt pack: {e}"))?;
    check_pack(&pack, &reconstructed)?;

    let cache = rehydrate_cache(reader.as_ref())?;
    let ledger = BudgetLedger::new(Some(cfg.bounds.max_cost));

    let mut providers = ProviderRegistry::new();
    let (_, _, provider_id) = crate::mock_panel();
    providers.register(Box::new(crate::synthetic::SyntheticProvider::new(
        provider_id,
    )));

    let last_event = reader
        .events()
        .map_err(|e| anyhow::anyhow!("reading events: {e}"))?
        .last();
    let handle = RunHandle::new(run_id.clone(), writer).continuing_from(last_event.as_ref());

    let result = run_pipeline(&cfg, &pack, &providers, &handle, &ledger, &cache).await;

    // Same as `arbiter run`'s own persistence pass (PLAN_DEVIATIONS.md D44):
    // whatever this attempt actually cached -- rehydrated entries the cache
    // already held plus anything genuinely new -- goes back to
    // `cache_entries` so a further resume never re-pays for it either.
    for (key, response) in cache.snapshot() {
        handle.put_cache_entry(&key, &response)?;
    }

    match &result {
        Ok(synthesized) => {
            handle.append_lifecycle_event(
                EventType::RunCompleted,
                serde_json::json!({"outcome": format!("{:?}", synthesized.record.outcome), "resumed": true}),
            )?;
        }
        Err(e) => {
            handle.append_lifecycle_event(
                EventType::RunFailed,
                serde_json::json!({"error": e.to_string(), "resumed": true}),
            )?;
        }
    }
    let synthesized = result?;

    if json {
        println!("{}", serde_json::to_string(&synthesized.record)?);
    } else {
        crate::print_human(&synthesized);
    }
    Ok(())
}
