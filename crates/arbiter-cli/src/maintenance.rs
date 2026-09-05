//! `arbiter doctor [--gc]`, `arbiter reindex`, and `keys`/`providers` (L4).
//! `doctor`/`reindex` need nothing this codebase doesn't already have.
//! `keys list/set/rm` and `providers list` now report/act on real
//! credential state, now that P3 (`arbiter-providers::keys`) exists; `keys
//! test` and `providers test` still need P4 (real HTTP adapters, which
//! actually make the one minimal request "test" means) and stay honest
//! stubs naming that (PLAN_DEVIATIONS.md D46).

use arbiter_core::{Policy, ProviderId, RunId};
use arbiter_kernel::provider::CallState;
use arbiter_kernel::store::RunStore;
use arbiter_providers::keys::{
    CredentialSource, EnvCredentialSource, KeySource, KeyState, KeychainCredentialSource,
};
use arbiter_store::sqlite_store::SqliteRunStore;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

pub fn reindex_command(store_root: PathBuf) -> anyhow::Result<()> {
    let history_path = crate::history_db_path(&store_root);
    let conn = arbiter_store::catalog::open_history_db(
        &history_path,
        env!("CARGO_PKG_VERSION"),
        &arbiter_store::now_rfc3339(),
    )
    .map_err(|e| anyhow::anyhow!("opening history.db: {e}"))?;
    let n = arbiter_store::catalog::reindex(&conn, &store_root)
        .map_err(|e| anyhow::anyhow!("reindexing: {e}"))?;
    println!("Reindexed {n} run(s) into {}", history_path.display());
    Ok(())
}

fn run_ids_under(store_root: &Path) -> Vec<RunId> {
    let mut ids = Vec::new();
    let Ok(entries) = std::fs::read_dir(store_root) else {
        return ids;
    };
    for entry in entries.flatten() {
        if entry.path().join("run.db").is_file()
            && let Some(name) = entry.file_name().to_str()
        {
            ids.push(RunId::new(name.to_string()));
        }
    }
    ids
}

pub fn doctor_command(gc: bool, store_root: PathBuf) -> anyhow::Result<()> {
    println!("arbiter doctor -- store: {}", store_root.display());
    println!("engine_version: {}", env!("CARGO_PKG_VERSION"));

    let policy = Policy::argument_v1();
    println!(
        "policy: {} (provisional: {})",
        policy.version.as_str(),
        policy.provisional
    );
    if policy.provisional {
        println!(
            "  warning: constants are provisional -- the tuning sweep and red-team session \
             (ARCHITECTURE §6.3) have not run against this exact constant set"
        );
    }

    println!("credentials:");
    let (env, keychain) = credential_sources();
    let sources: [&dyn CredentialSource; 2] = [&env, &keychain];
    for provider in known_providers() {
        if provider.as_str() == "mock" {
            continue;
        }
        let state = arbiter_providers::keys::resolve_state(&sources, &provider);
        println!("  {}: {}", provider.as_str(), describe_state(&state));
    }
    println!(
        "correlation table: not tracked -- no correlation table exists in this build; \
         --panel is always explicit (G2's own scope note)"
    );

    let current_boot_id = arbiter_store::lease::Owner::current().boot_id;
    let store = SqliteRunStore::new(&store_root);
    let run_ids = run_ids_under(&store_root);
    println!("runs found on disk: {}", run_ids.len());

    let mut stuck_running = Vec::new();
    let mut ledger_violations = Vec::new();
    let mut orphaned_spend: Vec<(String, String, f64)> = Vec::new();
    let mut gc_reclaimed: u64 = 0;
    let mut gc_deleted = 0usize;

    for run_id in &run_ids {
        let run_dir = store_root.join(run_id.as_str());
        let db_path = run_dir.join("run.db");
        let raw_conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        );
        let lease_live = match &raw_conn {
            Ok(conn) => {
                arbiter_store::blob::is_run_lease_live(conn, run_id.as_str(), &current_boot_id)
                    .unwrap_or(true)
            }
            Err(_) => true,
        };

        let Ok(reader) = store.reader(run_id) else {
            continue;
        };
        // A dead lease alone is not "stuck" -- a run that finished cleanly
        // also has no live owner (the process just exited), and its own
        // RunCompleted/RunFailed event already proves it isn't abandoned.
        // Only a dead lease with *no* terminal event is a run a killed
        // process actually left behind (ARCHITECTURE §8.5).
        let finished = reader.events().is_ok_and(|mut events| {
            events.any(|e| {
                matches!(
                    e.event_type,
                    arbiter_kernel::event::EventType::RunCompleted
                        | arbiter_kernel::event::EventType::RunFailed
                )
            })
        });
        if !lease_live && !finished {
            stuck_running.push(run_id.as_str().to_string());
        }

        if let Ok((reserved, _committed)) = reader.budget_totals()
            && let Ok(calls) = reader.provider_calls()
        {
            let computed: f64 = calls
                .iter()
                .filter(|c| c.state.is_non_terminal())
                .map(|c| c.reserved_amount.0)
                .sum();
            if (reserved.0 - computed).abs() > 1e-6 {
                ledger_violations.push((run_id.as_str().to_string(), reserved.0, computed));
            }
            for c in calls.iter().filter(|c| c.state == CallState::Orphaned) {
                orphaned_spend.push((
                    run_id.as_str().to_string(),
                    c.call_id.as_str().to_string(),
                    c.reserved_amount.0,
                ));
            }
        }

        if gc
            && !lease_live
            && let Ok(conn) = &raw_conn
        {
            let referenced: BTreeSet<String> = reader
                .cache_entries()
                .unwrap_or_default()
                .into_iter()
                .map(|(_, response)| response.response_hash)
                .collect();
            if let Ok(Some(report)) = arbiter_store::blob::gc_run(
                conn,
                run_id.as_str(),
                &run_dir,
                &referenced,
                &current_boot_id,
            ) {
                gc_deleted += report.deleted_hashes.len();
                gc_reclaimed += report.bytes_reclaimed;
            }
        }
    }

    if stuck_running.is_empty() {
        println!("runs stuck in 'running' (dead lease): none");
    } else {
        println!("runs stuck in 'running' (dead lease):");
        for id in &stuck_running {
            println!("  {id}");
        }
    }

    if ledger_violations.is_empty() {
        println!("ledger invariant: holds for every run scanned");
    } else {
        println!(
            "ledger invariant violations (persisted reserved != computed from non-terminal calls):"
        );
        for (id, persisted, computed) in &ledger_violations {
            println!("  {id}: persisted=${persisted:.2} computed=${computed:.2}");
        }
    }

    if orphaned_spend.is_empty() {
        println!("orphaned spend: none");
    } else {
        let total: f64 = orphaned_spend.iter().map(|(_, _, amt)| amt).sum();
        println!("orphaned spend: ${total:.2} total, reported never absorbed (ARCHITECTURE §8.4):");
        for (run_id, call_id, amt) in &orphaned_spend {
            println!("  {run_id}/{call_id}: ${amt:.2}");
        }
    }

    if gc {
        println!("--gc: deleted {gc_deleted} orphaned blob(s), reclaimed {gc_reclaimed} byte(s)");
    }

    Ok(())
}

/// `arbiter export <run_id> --format json|markdown|ndjson` (ARCHITECTURE
/// §12, §8.8). Writes to `<run_dir>/exports/`, matching the directory tree's
/// own comment ("exports/ -- anything the operator asked for", §8's own
/// layout). Not the `VACUUM INTO` whole-run copy §8.6 separately describes
/// under "Copying a run" -- that copies the run's own storage files
/// (`run.db`, `blobs/`) for backup/transport, a filesystem operation with
/// no format choice; this renders the run's *content* into one of three
/// interchange shapes, matching the `--format` flag §12's own CLI listing
/// gives it (PLAN_DEVIATIONS.md D45).
pub fn export_command(run_id: RunId, format: String, store_root: PathBuf) -> anyhow::Result<()> {
    let reader = crate::render::open_reader(&store_root, &run_id)?;
    let exports_dir = store_root.join(run_id.as_str()).join("exports");
    std::fs::create_dir_all(&exports_dir)?;

    let (filename, content) = match format.as_str() {
        "json" => {
            let record = crate::render::read_decision_record(reader.as_ref())?;
            (
                "export.json".to_string(),
                serde_json::to_string_pretty(&record)?,
            )
        }
        "markdown" => {
            let record = crate::render::read_decision_record(reader.as_ref())?;
            ("export.md".to_string(), render_markdown(&record))
        }
        "ndjson" => {
            let events: Vec<String> = reader
                .events()
                .map_err(|e| anyhow::anyhow!("reading events: {e}"))?
                .map(|e| serde_json::to_string(&e))
                .collect::<Result<_, _>>()?;
            ("export.ndjson".to_string(), events.join("\n"))
        }
        other => anyhow::bail!("unknown --format '{other}' -- expected json, markdown, or ndjson"),
    };

    let path = exports_dir.join(filename);
    std::fs::write(&path, content)?;
    println!("Wrote {}", path.display());
    Ok(())
}

fn render_markdown(record: &arbiter_core::DecisionRecord) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Decision: {}\n\n", record.run_id.as_str()));
    out.push_str(&format!("**Question:** {}\n\n", record.question));
    out.push_str(&format!("**Outcome:** {:?}\n\n", record.outcome));
    match &record.recommendation {
        Some(r) => out.push_str(&format!(
            "**Recommendation:** {} (`{}`)\n\n",
            r.label,
            r.option_id.as_str()
        )),
        None => out.push_str("**Recommendation:** none\n\n"),
    }
    out.push_str(&format!(
        "**Confidence:** {:.2} (base {:.2})\n\n",
        record.confidence.total, record.confidence.base
    ));
    out.push_str("## Confidence breakdown\n\n");
    out.push_str("| Term | Value | Weight | Contribution |\n|---|---|---|---|\n");
    for d in &record.confidence.dimensions {
        out.push_str(&format!(
            "| {} | {:.2} | {:.2} | {:+.4} |\n",
            d.name, d.value, d.weight, d.contribution
        ));
    }
    for p in &record.confidence.penalties {
        out.push_str(&format!(
            "| {} (penalty) | {} | {:.2} | {:+.4} |\n",
            p.name,
            p.input
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "-".to_string()),
            p.rate,
            p.contribution
        ));
    }
    out.push_str(&format!(
        "\n## Claims\n\n{} agreed, {} disputed, {} unresolved, {} defeated\n\n",
        record.claims.agreed,
        record.claims.disputed,
        record.claims.unresolved,
        record.claims.defeated
    ));
    if !record.options.is_empty() {
        out.push_str("## Options\n\n| Option | Share |\n|---|---|\n");
        for o in &record.options {
            out.push_str(&format!(
                "| {} (`{}`) | {:.2} |\n",
                o.label,
                o.id.as_str(),
                o.share
            ));
        }
        out.push('\n');
    }
    if !record.change_triggers.is_empty() {
        out.push_str("## Change triggers\n\n");
        for t in &record.change_triggers {
            out.push_str(&format!("- `{}` {:?}\n", t.claim_id.as_str(), t.direction));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "---\nengine_version: {}  \ninputs_hash: {}\n",
        record.engine_version, record.inputs_hash
    ));
    out
}

/// Every provider id this build has a name for. Not a real roster --
/// `panel.resolve`/the correlation table don't exist yet (G2's own scope
/// note), so there is no discovered list of providers, only the ones named
/// anywhere in this codebase's own spec/code: `mock` (the only one `--panel`
/// can ever select, needs no key) and `anthropic` (ARCHITECTURE §11.1's own
/// worked example -- its credential *state* is real and reportable even
/// though no adapter can spend it yet, P4, PLAN_DEVIATIONS.md D46).
/// `pub(crate)`: `serve`'s own `GET /api/providers` (U1) reports the same
/// roster this command does, so the CLI and the loopback UI never name a
/// different set of providers.
/// Every provider this build can name: the synthetic one, plus each real
/// adapter P4 shipped. Sourced from `arbiter-providers` itself rather than a
/// second hand-maintained list, so adding an adapter shows up in `keys list`,
/// `providers list`, and the UI's panel picker without touching this file.
pub(crate) fn known_providers() -> Vec<ProviderId> {
    std::iter::once(ProviderId::new("mock"))
        .chain(
            arbiter_providers::REAL_PROVIDER_IDS
                .iter()
                .map(|id| ProviderId::new(*id)),
        )
        .collect()
}

pub(crate) fn credential_sources() -> (EnvCredentialSource, KeychainCredentialSource) {
    (EnvCredentialSource, KeychainCredentialSource)
}

fn describe_state(state: &KeyState) -> String {
    match state {
        KeyState::Missing => "missing".to_string(),
        KeyState::Present { source } => format!("present ({})", describe_source(source)),
        KeyState::Verified { source, at } => {
            format!("verified ({}) at {at}", describe_source(source))
        }
        KeyState::Rejected { source, status, at } => {
            format!(
                "rejected ({}, HTTP {status}) at {at}",
                describe_source(source)
            )
        }
    }
}

fn describe_source(source: &KeySource) -> String {
    match source {
        KeySource::ArbiterEnv(var) => format!("env:{var}"),
        KeySource::ProviderEnv(var) => format!("env:{var}"),
        KeySource::Keychain => "keychain".to_string(),
    }
}

pub fn keys_list_command() -> anyhow::Result<()> {
    let (env, keychain) = credential_sources();
    let sources: [&dyn CredentialSource; 2] = [&env, &keychain];
    for provider in known_providers() {
        if provider.as_str() == "mock" {
            println!(
                "{}: not required (synthetic, no network access)",
                provider.as_str()
            );
            continue;
        }
        let state = arbiter_providers::keys::resolve_state(&sources, &provider);
        println!("{}: {}", provider.as_str(), describe_state(&state));
    }
    println!(
        "(fingerprints and sources only -- never the key itself, ARCHITECTURE §11.1's own \
         `keys list` contract)"
    );
    Ok(())
}

/// Reads the key from stdin (never a CLI argument -- a key on the command
/// line ends up in shell history and `ps`), per ARCHITECTURE §12: "`arbiter
/// keys set <provider>` read from stdin, store in the OS keychain."
pub fn keys_set_command(provider: String) -> anyhow::Result<()> {
    let mut value = String::new();
    std::io::stdin()
        .read_to_string(&mut value)
        .map_err(|e| anyhow::anyhow!("reading key from stdin: {e}"))?;
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("no key was read from stdin");
    }
    let provider = ProviderId::new(provider);
    let secret = arbiter_providers::keys::SecretString::new(value);
    KeychainCredentialSource::set(&provider, &secret).map_err(|e| {
        anyhow::anyhow!(
            "storing the key for {} in the OS keychain: {e} -- this may mean no OS keychain \
             backend is reachable here (no D-Bus Secret Service session, no macOS/Windows), \
             PLAN_DEVIATIONS.md D46",
            provider.as_str()
        )
    })?;
    println!("Stored a key for {} in the OS keychain.", provider.as_str());
    Ok(())
}

pub fn keys_rm_command(provider: String) -> anyhow::Result<()> {
    let provider = ProviderId::new(provider);
    KeychainCredentialSource::remove(&provider).map_err(|e| {
        anyhow::anyhow!("removing the keychain entry for {}: {e}", provider.as_str())
    })?;
    println!("Removed the keychain entry for {}.", provider.as_str());
    Ok(())
}

/// `arbiter keys test [provider]` -- **spends money**: one minimal completion
/// per provider tested. With no provider named, every provider that has a key
/// is tested and the ones without are reported, not called.
pub async fn keys_test_command(provider: Option<String>) -> anyhow::Result<()> {
    let targets: Vec<ProviderId> = match provider {
        Some(p) => vec![ProviderId::new(p)],
        None => known_providers(),
    };
    let mut any_rejected = false;
    for provider in targets {
        let outcome = crate::verify::verify(&provider).await;
        println!("{}: {}", provider.as_str(), outcome.state());
        println!("  {}", outcome.headline(provider.as_str()));
        let detail = outcome.detail();
        if !detail.is_empty() {
            println!("  {detail}");
        }
        if matches!(outcome.state(), "rejected" | "blocked") {
            any_rejected = true;
        }
    }
    if any_rejected {
        anyhow::bail!(
            "at least one provider cannot serve a request — `rejected` means replace the key, \
             `blocked` means the key is fine and the account needs attention"
        );
    }
    Ok(())
}

pub fn providers_list_command() -> anyhow::Result<()> {
    let (env, keychain) = credential_sources();
    let sources: [&dyn CredentialSource; 2] = [&env, &keychain];
    for provider in known_providers() {
        println!("id: {}", provider.as_str());
        if provider.as_str() == "mock" {
            println!("  name: Mock");
            println!("  key: not required (synthetic, no network access)");
            continue;
        }
        let state = arbiter_providers::keys::resolve_state(&sources, &provider);
        println!("  key: {}", describe_state(&state));
        println!(
            "  usable: {} (a resolvable key; `arbiter providers test` spends one \
             request to prove it works)",
            matches!(state, arbiter_providers::keys::KeyState::Present { .. })
        );
    }
    Ok(())
}

/// `arbiter providers test` is `keys test` under the roster's name -- the same
/// one minimal request, reported the same way. Kept as its own command because
/// the plan names both.
pub async fn providers_test_command(provider: Option<String>) -> anyhow::Result<()> {
    keys_test_command(provider).await
}
