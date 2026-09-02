//! Per-claim defeat chains — INTERFACES §22's `defeat_chains`: "why a claim
//! stands where it does," decomposed back into the individual `Supports`/
//! `Contradicts`/`Qualifies` edges §6.3's fixpoint folded into its standing
//! number. `arbiter explain` is the one place this decomposition is needed —
//! `arbiter-kernel`'s `fixpoint::solve` only ever returns the *final*
//! standing, not a per-edge ledger of how it got there (nothing upstream of
//! `explain` has a use for one) — so this reconstructs it from data the
//! fixpoint already computed: the final `standing` map and the relation list
//! that produced it.
//!
//! PLAN_DEVIATIONS.md D43: INTERFACES §22's worked example also carries an
//! `"evidence"` field alongside `"standing"` (`E(c)`, the fixpoint's own
//! starting value). Recomputing it exactly requires the claim's judge scores
//! and lifecycle (`decision::evidence::evidence`'s own signature), neither of
//! which is join-able from what a finished run persists per claim — so this
//! module omits it rather than guess.

use crate::config::GraphParams;
use crate::ids::ClaimId;
use crate::relation::{Relation, RelationKind};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DefeatStep {
    pub by: ClaimId,
    pub relation: RelationKind,
    pub attacker_standing: f64,
    /// The classifier's confidence in this edge — §6.3's `w`.
    pub weight: f64,
    /// This edge's own signed contribution to `claim_id`'s standing —
    /// negative for `Contradicts`/`Qualifies`, positive for `Supports`.
    /// Summing every step's `delta` for one claim reproduces the fixpoint's
    /// `support_term − attack_term − qualify_term` exactly (support/attack
    /// pro-rated across their own edges when the sum saturates its cap,
    /// since the fixpoint itself clips the *sum*, not any one edge).
    pub delta: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DefeatChain {
    pub claim_id: ClaimId,
    pub standing: f64,
    /// Steps sorted by `delta` ascending (the most damaging attacker first) —
    /// matching INTERFACES §22's own worked example ordering.
    pub steps: Vec<DefeatStep>,
    /// True when this claim's incoming attack or support sum exceeded its
    /// cap — the same predicate `fixpoint::solve`'s own `saturated` set uses,
    /// recomputed here from the same two numbers (`raw`, `cap`) since the
    /// per-run fixpoint's `saturated` set is not itself persisted.
    pub saturated: bool,
}

/// One edge kind's contribution: `gain * min(raw, cap)`, pro-rated back across
/// the edges that produced `raw` — `cap` of `f64::INFINITY` for the
/// uncapped `Qualifies` term (§6.3 states no cap for it, only attack and
/// support have one).
fn edges_into<'a>(
    claim_id: &ClaimId,
    relations: &'a [Relation],
    kind: RelationKind,
) -> Vec<&'a Relation> {
    relations
        .iter()
        .filter(|r| r.to == *claim_id && r.kind == kind)
        .collect()
}

fn signed_steps(
    edges: &[&Relation],
    standing: &BTreeMap<ClaimId, f64>,
    gain: f64,
    cap: f64,
    sign: f64,
) -> (Vec<DefeatStep>, bool) {
    let raws: Vec<(f64, &Relation)> = edges
        .iter()
        .map(|r| {
            let attacker_standing = standing.get(&r.from).copied().unwrap_or(0.0);
            (r.confidence * attacker_standing, *r)
        })
        .collect();
    let raw_sum: f64 = raws.iter().map(|(v, _)| v).sum();
    // Strict `>`, matching `fixpoint::solve`'s own saturation predicate: a sum
    // exactly at the cap was not actually clipped.
    let saturated = raw_sum > cap;
    let scale = if saturated && raw_sum > 0.0 {
        cap / raw_sum
    } else {
        1.0
    };
    let steps = raws
        .into_iter()
        .map(|(edge_raw, r)| DefeatStep {
            by: r.from.clone(),
            relation: r.kind,
            attacker_standing: standing.get(&r.from).copied().unwrap_or(0.0),
            weight: r.confidence,
            delta: sign * gain * edge_raw * scale,
        })
        .collect();
    (steps, saturated)
}

/// Reconstructs `claim_id`'s defeat chain from the final, already-solved
/// `standing` map and the relation list that produced it — the same two
/// inputs `fixpoint::solve` itself closed over, so every `delta` this
/// produces is consistent with the standing values already on record, not a
/// second, possibly-drifting computation.
pub fn defeat_chain_for(
    claim_id: &ClaimId,
    standing: &BTreeMap<ClaimId, f64>,
    relations: &[Relation],
    p: &GraphParams,
) -> DefeatChain {
    let supports = edges_into(claim_id, relations, RelationKind::Supports);
    let attacks = edges_into(claim_id, relations, RelationKind::Contradicts);
    let qualifies = edges_into(claim_id, relations, RelationKind::Qualifies);

    let (support_steps, support_saturated) =
        signed_steps(&supports, standing, p.support_gain, p.support_cap, 1.0);
    let (attack_steps, attack_saturated) =
        signed_steps(&attacks, standing, p.attack_gain, p.attack_cap, -1.0);
    // Qualifies has no stated cap (§6.3) -- an infinite cap never saturates.
    let (qualify_steps, _) =
        signed_steps(&qualifies, standing, p.qualify_gain, f64::INFINITY, -1.0);

    let mut steps = support_steps;
    steps.extend(attack_steps);
    steps.extend(qualify_steps);
    steps.sort_by(|a, b| {
        a.delta
            .partial_cmp(&b.delta)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.by.cmp(&b.by))
    });

    DefeatChain {
        claim_id: claim_id.clone(),
        standing: standing.get(claim_id).copied().unwrap_or(0.0),
        steps,
        saturated: support_saturated || attack_saturated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GraphParams;

    fn params() -> GraphParams {
        GraphParams::default()
    }

    fn rel(from: &str, to: &str, kind: RelationKind, confidence: f64) -> Relation {
        Relation {
            from: ClaimId::new(from),
            to: ClaimId::new(to),
            kind,
            confidence,
        }
    }

    /// Mirrors `fixpoint`'s own "one strong attacker" worked example: standing
    /// pinned to what the fixpoint would have produced, and this module's job
    /// is only to explain the edge that produced it.
    #[test]
    fn single_attacker_delta_matches_the_fixpoint_formula() {
        let p = params();
        let mut standing = BTreeMap::new();
        standing.insert(ClaimId::new("fact"), 0.40);
        standing.insert(ClaimId::new("attacker"), 1.0);
        let rels = vec![rel("attacker", "fact", RelationKind::Contradicts, 1.0)];

        let chain = defeat_chain_for(&ClaimId::new("fact"), &standing, &rels, &p);
        assert_eq!(chain.steps.len(), 1);
        assert!(!chain.saturated);
        let step = &chain.steps[0];
        assert_eq!(step.by, ClaimId::new("attacker"));
        assert_eq!(step.relation, RelationKind::Contradicts);
        assert!((step.attacker_standing - 1.0).abs() < 1e-9);
        assert!((step.weight - 1.0).abs() < 1e-9);
        // beta * w * standing = 0.60 * 1.0 * 1.0 = 0.60, negative.
        assert!((step.delta - (-p.attack_gain)).abs() < 1e-9);
    }

    /// Mirrors `fixpoint`'s "two strong attackers, sum 2.0 capped to 1.5"
    /// example: each edge's raw contribution is 1.0, so the pro-rated delta
    /// per edge is exactly half of the capped total.
    #[test]
    fn saturated_attackers_are_pro_rated_and_marked_saturated() {
        let p = params();
        let mut standing = BTreeMap::new();
        standing.insert(ClaimId::new("fact"), 0.10);
        standing.insert(ClaimId::new("a1"), 1.0);
        standing.insert(ClaimId::new("a2"), 1.0);
        let rels = vec![
            rel("a1", "fact", RelationKind::Contradicts, 1.0),
            rel("a2", "fact", RelationKind::Contradicts, 1.0),
        ];

        let chain = defeat_chain_for(&ClaimId::new("fact"), &standing, &rels, &p);
        assert!(chain.saturated);
        assert_eq!(chain.steps.len(), 2);
        let total: f64 = chain.steps.iter().map(|s| s.delta).sum();
        // beta * cap = 0.60 * 1.5 = 0.90, negative in total.
        assert!((total - (-p.attack_gain * p.attack_cap)).abs() < 1e-9);
        // Split evenly between two identical edges.
        for step in &chain.steps {
            assert!((step.delta - (-p.attack_gain * p.attack_cap / 2.0)).abs() < 1e-9);
        }
    }

    #[test]
    fn a_claim_with_no_incoming_edges_has_an_empty_unsaturated_chain() {
        let p = params();
        let mut standing = BTreeMap::new();
        standing.insert(ClaimId::new("lonely"), 0.73);

        let chain = defeat_chain_for(&ClaimId::new("lonely"), &standing, &[], &p);
        assert!(chain.steps.is_empty());
        assert!(!chain.saturated);
        assert!((chain.standing - 0.73).abs() < 1e-9);
    }

    #[test]
    fn steps_are_sorted_most_damaging_first() {
        let p = params();
        let mut standing = BTreeMap::new();
        standing.insert(ClaimId::new("fact"), 0.5);
        standing.insert(ClaimId::new("strong"), 0.9);
        standing.insert(ClaimId::new("weak"), 0.2);
        let rels = vec![
            rel("weak", "fact", RelationKind::Qualifies, 0.5),
            rel("strong", "fact", RelationKind::Contradicts, 0.8),
        ];

        let chain = defeat_chain_for(&ClaimId::new("fact"), &standing, &rels, &p);
        assert_eq!(chain.steps[0].by, ClaimId::new("strong"));
        assert_eq!(chain.steps[1].by, ClaimId::new("weak"));
        assert!(chain.steps[0].delta < chain.steps[1].delta);
    }
}
