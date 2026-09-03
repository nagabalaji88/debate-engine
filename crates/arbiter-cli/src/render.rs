//! Read paths for `show`, `explain`, `claims` and `history` (L2). The CLI is
//! a renderer (ARCHITECTURE §12: "no decision logic lives here — every
//! number it prints comes out of `arbiter-core`") — every function here
//! either reads an already-computed value straight off a persisted artifact,
//! or calls a pure `arbiter-core` function (`defeat_chain_for`) with data
//! read verbatim from one. It computes nothing of its own.
//!
//! PLAN_DEVIATIONS.md D43 covers the gaps this module works around: no
//! prior task had built a way to read an artifact back out of the store, or
//! a persisted claim-by-claim standing classification, or a defeat-chain
//! decomposition at all.

use arbiter_core::{
    ClaimId, ClaimStanding, DecisionRecord, DefeatChain, OptionId, Policy, Relation, RelationKind,
    RunId, defeat_chain_for,
};
use arbiter_kernel::store::{RunReader, RunStore, StoreError};
use arbiter_store::sqlite_store::SqliteRunStore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub fn open_reader(store_root: &Path, run_id: &RunId) -> anyhow::Result<Box<dyn RunReader>> {
    let store = SqliteRunStore::new(store_root);
    store
        .reader(run_id)
        .map_err(|e| anyhow::anyhow!("opening run {}: {e}", run_id.as_str()))
}

fn map_store_err(context: &str) -> impl FnOnce(StoreError) -> anyhow::Error + '_ {
    move |e| anyhow::anyhow!("{context}: {e}")
}

/// The final `DecisionRecord`, straight off `synthesized_decision.v1`'s own
/// `"record"` field — serialized with real `serde` (not the Debug-formatted
/// `to_json()` every kernel-stage artifact otherwise uses), so it round-trips
/// through `serde_json::from_value` exactly, with no local view struct
/// needed.
pub fn read_decision_record(reader: &dyn RunReader) -> anyhow::Result<DecisionRecord> {
    let mut payloads = reader
        .artifacts_by_type("synthesized_decision.v1")
        .map_err(map_store_err("reading synthesized_decision.v1"))?;
    let payload = payloads.pop().ok_or_else(|| {
        anyhow::anyhow!(
            "this run has no synthesized decision yet -- it may still be running, or it \
             failed before decision.synthesize"
        )
    })?;
    serde_json::from_value(payload["record"].clone())
        .map_err(|e| anyhow::anyhow!("parsing the stored decision record: {e}"))
}

/// `Serialize`, not just used internally: `serve`'s own `GET /api/runs/:id`
/// (U1/U4) embeds it directly in the Result screen's "integrity" object.
#[derive(Debug, Clone, Serialize)]
pub struct CompletenessView {
    pub status: String,
    pub reason: Option<String>,
}

pub fn read_completeness(reader: &dyn RunReader) -> anyhow::Result<CompletenessView> {
    let mut payloads = reader
        .artifacts_by_type("synthesized_decision.v1")
        .map_err(map_store_err("reading synthesized_decision.v1"))?;
    let payload = payloads
        .pop()
        .ok_or_else(|| anyhow::anyhow!("this run has no synthesized decision yet"))?;
    let completeness = &payload["completeness"];
    Ok(CompletenessView {
        status: completeness["status"]
            .as_str()
            .unwrap_or("unknown")
            .to_string(),
        reason: completeness["reason"].as_str().map(|s| s.to_string()),
    })
}

#[derive(Debug, Clone, Deserialize)]
struct ClaimView {
    id: ClaimId,
    text: String,
    kind: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RelationView {
    from: ClaimId,
    to: ClaimId,
    kind: String,
    confidence: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct CellView {
    claim: ClaimId,
    option: OptionId,
    polarity: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ResolvedGraphJson {
    claims: Vec<ClaimView>,
    relations: Vec<RelationView>,
    standing: BTreeMap<ClaimId, f64>,
    propagated_cells: Vec<CellView>,
}

/// The final round's resolved argument graph: claim text, relation edges,
/// final standing and the propagated attachment matrix. Sourced from the
/// *last* `controller_decision.v1` artifact — the round loop always runs at
/// least once (`disputes.rank` once, then the loop's first iteration always
/// reaches `controller.decide` before it can `Stop`, L1's own orchestrator),
/// so this always exists once a run has gotten past the round loop.
pub struct GraphView {
    claims: BTreeMap<ClaimId, ClaimView>,
    pub relations: Vec<Relation>,
    pub standing: BTreeMap<ClaimId, f64>,
    cells: Vec<CellView>,
}

impl GraphView {
    pub fn claim_text(&self, id: &ClaimId) -> Option<&str> {
        self.claims.get(id).map(|c| c.text.as_str())
    }

    pub fn claim_kind(&self, id: &ClaimId) -> Option<&str> {
        self.claims.get(id).map(|c| c.kind.as_str())
    }

    /// Claim ids with a `Supports`/`Opposes` cell on `option`, sorted.
    pub fn supported_by(&self, option: &OptionId) -> Vec<ClaimId> {
        self.cells_for(option, "Supports")
    }

    pub fn opposed_by(&self, option: &OptionId) -> Vec<ClaimId> {
        self.cells_for(option, "Opposes")
    }

    fn cells_for(&self, option: &OptionId, polarity: &str) -> Vec<ClaimId> {
        let mut ids: Vec<ClaimId> = self
            .cells
            .iter()
            .filter(|c| &c.option == option && c.polarity == polarity)
            .map(|c| c.claim.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }
}

fn parse_relation_kind(s: &str) -> RelationKind {
    // The Debug-format string `AnalyzedRelations::to_json()` writes
    // (`format!("{:?}", r.kind)`), not the serde `snake_case` rename --
    // matched literally rather than via `Deserialize`, since this JSON was
    // never written by `serde` on the `RelationKind` side.
    match s {
        "Supports" => RelationKind::Supports,
        "Contradicts" => RelationKind::Contradicts,
        "Qualifies" => RelationKind::Qualifies,
        "Uncertain" => RelationKind::Uncertain,
        _ => RelationKind::Unrelated,
    }
}

pub fn read_final_graph(reader: &dyn RunReader) -> anyhow::Result<GraphView> {
    let mut payloads = reader
        .artifacts_by_type("controller_decision.v1")
        .map_err(map_store_err("reading controller_decision.v1"))?;
    let payload = payloads
        .pop()
        .ok_or_else(|| anyhow::anyhow!("this run never reached the round loop"))?;
    let parsed: ResolvedGraphJson = serde_json::from_value(payload["resolved"].clone())
        .map_err(|e| anyhow::anyhow!("parsing the stored resolved graph: {e}"))?;

    let relations = parsed
        .relations
        .iter()
        .map(|r| Relation {
            from: r.from.clone(),
            to: r.to.clone(),
            kind: parse_relation_kind(&r.kind),
            confidence: r.confidence,
        })
        .collect();

    Ok(GraphView {
        claims: parsed
            .claims
            .into_iter()
            .map(|c| (c.id.clone(), c))
            .collect(),
        relations,
        standing: parsed.standing,
        cells: parsed.propagated_cells,
    })
}

/// `arbiter claims --state`'s and `show --claims`'s one row.
#[derive(Debug, Clone, Serialize)]
pub struct ClaimRow {
    pub id: ClaimId,
    pub text: String,
    pub kind: String,
    pub standing: ClaimStanding,
}

pub fn claim_rows(record: &DecisionRecord, graph: &GraphView) -> Vec<ClaimRow> {
    let mut rows: Vec<ClaimRow> = record
        .claim_standings
        .iter()
        .map(|(id, standing)| ClaimRow {
            id: id.clone(),
            text: graph
                .claim_text(id)
                .unwrap_or("<claim text unavailable>")
                .to_string(),
            kind: graph.claim_kind(id).unwrap_or("Unknown").to_string(),
            standing: *standing,
        })
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

/// INTERFACES §22's `explain --json` schema, so far as this run's persisted
/// artifacts support it. `confidence` and `change_triggers` are exact
/// (straight off the already-shipped, spec-conformant `DecisionRecord`);
/// `defeat_chains` carries `standing`/`steps`/`saturated` but not the
/// worked example's separate `"evidence"` field (D43: not derivable from
/// what a finished run persists); `options` carries `supported_by`/
/// `opposed_by` but not `share` twice over -- it's the same `OptionScore`
/// list `DecisionRecord.options` already has, extended in place.
#[derive(Debug, Clone, Serialize)]
pub struct ExplainSubject {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<ClaimId>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExplainOption {
    pub id: OptionId,
    pub label: String,
    pub share: f64,
    pub supported_by: Vec<ClaimId>,
    pub opposed_by: Vec<ClaimId>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExplainOutput {
    pub schema_version: u32,
    pub run_id: RunId,
    pub policy_version: String,
    pub subject: ExplainSubject,
    pub confidence: arbiter_core::ConfidenceExplain,
    pub defeat_chains: Vec<DefeatChain>,
    pub change_triggers: Vec<arbiter_core::ChangeTriggerEntry>,
    pub options: Vec<ExplainOption>,
}

/// Builds the `explain` payload. `claim_id` narrows `subject`/`defeat_chains`
/// to one claim; with none given, `defeat_chains` covers every unresolved or
/// disputed claim plus every claim named in `change_triggers` (deduplicated)
/// -- the claims actually driving "why not more confident" and "what would
/// flip this," capped at 10 so a large debate doesn't dump its entire graph
/// (PLAN_DEVIATIONS.md D43; INTERFACES §22 does not pin what a decision-level
/// `defeat_chains` should contain).
pub fn build_explain(
    record: &DecisionRecord,
    graph: &GraphView,
    claim_id: Option<&ClaimId>,
) -> ExplainOutput {
    let graph_params = &Policy::argument_v1().config.graph;

    let subject_claims: Vec<ClaimId> = match claim_id {
        Some(id) => vec![id.clone()],
        None => {
            let mut ids: Vec<ClaimId> = record.unresolved_claims.clone();
            ids.extend(record.change_triggers.iter().map(|t| t.claim_id.clone()));
            ids.extend(
                record
                    .claim_standings
                    .iter()
                    .filter(|(_, s)| **s == ClaimStanding::Disputed)
                    .map(|(id, _)| id.clone()),
            );
            ids.sort();
            ids.dedup();
            ids.truncate(10);
            ids
        }
    };

    let defeat_chains = subject_claims
        .iter()
        .map(|id| defeat_chain_for(id, &graph.standing, &graph.relations, graph_params))
        .collect();

    let options = record
        .options
        .iter()
        .map(|o| ExplainOption {
            id: o.id.clone(),
            label: o.label.clone(),
            share: o.share,
            supported_by: graph.supported_by(&o.id),
            opposed_by: graph.opposed_by(&o.id),
        })
        .collect();

    ExplainOutput {
        schema_version: arbiter_core::SCHEMA_VERSION,
        run_id: record.run_id.clone(),
        policy_version: record.policy_version.as_str().to_string(),
        subject: match claim_id {
            Some(id) => ExplainSubject {
                kind: "claim",
                id: Some(id.clone()),
            },
            None => ExplainSubject {
                kind: "decision",
                id: None,
            },
        },
        confidence: record.confidence.clone(),
        defeat_chains,
        change_triggers: record.change_triggers.clone(),
        options,
    }
}
