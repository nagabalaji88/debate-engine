//! `arbiter accept <run_id> [--override path=value --reason "…"]`,
//! INTERFACES §17. Purely a record-keeping operation — no decision logic,
//! no re-derivation, just persisting what a human decided about an
//! already-synthesized run. Build Studio (ARCHITECTURE §13, "optional,
//! isolated") is the thing this record eventually gates; it does not exist
//! in this codebase, so `accept` only ever writes the record, never checks
//! or blocks anything against it (PLAN_DEVIATIONS.md D45).

use arbiter_core::{DecisionAcceptance, DecisionOverride, OverrideId, RunId};
use arbiter_kernel::event::EventType;
use arbiter_kernel::store::{Artifact, RunStore};
use arbiter_store::sqlite_store::SqliteRunStore;
use std::path::PathBuf;

use crate::run_handle::RunHandle;

#[derive(Debug)]
struct AcceptanceArtifact(DecisionAcceptance);

impl Artifact for AcceptanceArtifact {
    fn artifact_type(&self) -> &'static str {
        "decision_acceptance.v1"
    }
    fn content_hash(&self) -> String {
        let json = serde_json::to_string(&self.0).expect("acceptance serializes");
        let text = format!("{}\u{1}{}", self.artifact_type(), json);
        format!("blake3:{}", blake3::hash(text.as_bytes()).to_hex())
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.0).expect("acceptance serializes")
    }
}

/// Parses `--override path=value` into `(path, value)`; `value` is
/// interpreted as JSON when it parses as such (a number, `true`/`false`,
/// `null`, a quoted string), and as a plain JSON string otherwise — the
/// usual CLI convention, since ARCHITECTURE names no format for it.
fn parse_override(raw: &str) -> anyhow::Result<(String, serde_json::Value)> {
    let (path, value) = raw
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("--override must be 'path=value', got '{raw}'"))?;
    let value = serde_json::from_str(value)
        .unwrap_or_else(|_| serde_json::Value::String(value.to_string()));
    Ok((path.to_string(), value))
}

pub fn accept_command(
    run_id: RunId,
    overrides: Vec<String>,
    reasons: Vec<String>,
    json: bool,
    store_root: PathBuf,
) -> anyhow::Result<()> {
    if overrides.len() != reasons.len() {
        anyhow::bail!(
            "--override and --reason must be given the same number of times ({} overrides, {} \
             reasons) -- every override needs its own reason (INTERFACES §17: \"an unexplained \
             override is rejected\")",
            overrides.len(),
            reasons.len()
        );
    }

    let store = SqliteRunStore::new(&store_root);
    let reader = store
        .reader(&run_id)
        .map_err(|e| anyhow::anyhow!("opening run {}: {e}", run_id.as_str()))?;

    if reader
        .artifacts_by_type("synthesized_decision.v1")
        .map_err(|e| anyhow::anyhow!("reading synthesized_decision.v1: {e}"))?
        .is_empty()
    {
        anyhow::bail!(
            "run {} has no synthesized decision yet -- there is nothing to accept",
            run_id.as_str()
        );
    }

    let mut decision_overrides = Vec::with_capacity(overrides.len());
    for (i, (raw_override, reason)) in overrides.into_iter().zip(reasons).enumerate() {
        if reason.trim().is_empty() {
            anyhow::bail!(
                "override #{} has an empty --reason, which INTERFACES §17 rejects",
                i + 1
            );
        }
        let (path, to) = parse_override(&raw_override)?;
        decision_overrides.push(DecisionOverride {
            id: OverrideId::new(format!("ovr_{}_{}", run_id.as_str(), i + 1)),
            path,
            // No Build Studio document exists to read a prior value from
            // (PLAN_DEVIATIONS.md D45) -- honestly absent rather than
            // guessed at.
            from: serde_json::Value::Null,
            to,
            reason,
        });
    }

    let accepted_by = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    let acceptance = DecisionAcceptance {
        accepted_by,
        accepted_at: arbiter_store::now_rfc3339(),
        overrides: decision_overrides,
    };

    let writer = store
        .reopen(&run_id)
        .map_err(|e| anyhow::anyhow!("reopening run {}: {e}", run_id.as_str()))?;
    let last_event = reader
        .events()
        .map_err(|e| anyhow::anyhow!("reading events: {e}"))?
        .last();
    let handle = RunHandle::new(run_id.clone(), writer).continuing_from(last_event.as_ref());

    handle.append_lifecycle_event(
        EventType::DecisionAccepted,
        serde_json::json!({"accepted_by": acceptance.accepted_by, "accepted_at": acceptance.accepted_at}),
    )?;
    for o in &acceptance.overrides {
        handle.append_lifecycle_event(
            EventType::DecisionOverridden,
            serde_json::json!({"id": o.id.as_str(), "path": o.path, "to": o.to, "reason": o.reason}),
        )?;
    }
    handle.put_artifact(&AcceptanceArtifact(acceptance.clone()))?;

    if json {
        println!("{}", serde_json::to_string(&acceptance)?);
    } else {
        println!(
            "Accepted run {} by {} at {}",
            run_id.as_str(),
            acceptance.accepted_by,
            acceptance.accepted_at
        );
        for o in &acceptance.overrides {
            println!(
                "  override {} {} -> {} ({})",
                o.id.as_str(),
                o.path,
                o.to,
                o.reason
            );
        }
    }
    Ok(())
}
