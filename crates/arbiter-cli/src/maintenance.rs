//! `arbiter doctor [--gc]`, `arbiter reindex`, and `keys`/`providers` (L4).
//! `doctor`/`reindex` need nothing this codebase doesn't already have; the
//! `keys`/`providers` subcommands are honest stubs — P3 (credential
//! resolution: OS keychain, redaction) and P4 (real HTTP adapters) are
//! deliberately deferred to their own pass (IMPLEMENTATION_PLAN.md's own
//! P1-P4 scope note), so there is no credential state or real provider
//! roster to report (PLAN_DEVIATIONS.md D45).

use arbiter_core::{Policy, RunId};
use arbiter_kernel::provider::CallState;
use arbiter_kernel::store::RunStore;
use arbiter_store::sqlite_store::SqliteRunStore;
use std::collections::BTreeSet;
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

    println!(
        "credentials: not available -- P3 (credential resolution) is not implemented in this \
         build (PLAN_DEVIATIONS.md D45)"
    );
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

pub fn keys_list_command() -> anyhow::Result<()> {
    println!(
        "No credential sources are resolved in this build -- P3 (ARCHITECTURE §11.1: \
         ARBITER_<P>_API_KEY -> the provider's own var -> OS keychain) is not yet implemented \
         (PLAN_DEVIATIONS.md D45)."
    );
    println!(
        "The only provider this build can run against is `mock` (--panel mock), which needs no key."
    );
    Ok(())
}

pub fn keys_unimplemented(subcommand: &str) -> anyhow::Result<()> {
    anyhow::bail!(
        "`arbiter keys {subcommand}` needs P3 (credential resolution, OS keychain), which is \
         not implemented in this build (PLAN_DEVIATIONS.md D45)"
    )
}

pub fn providers_list_command() -> anyhow::Result<()> {
    println!("id: mock");
    println!("  name: Mock");
    println!("  key: not required (synthetic, no network access)");
    println!(
        "Real provider adapters (P4: Anthropic, OpenAI-compatible) are not implemented in this \
         build (PLAN_DEVIATIONS.md D45)."
    );
    Ok(())
}

pub fn providers_test_unimplemented() -> anyhow::Result<()> {
    anyhow::bail!(
        "`arbiter providers test` needs P4 (real provider adapters), which is not implemented \
         in this build (PLAN_DEVIATIONS.md D45)"
    )
}
