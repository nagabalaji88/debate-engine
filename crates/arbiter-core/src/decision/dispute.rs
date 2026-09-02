//! Dispute ranking, INTERFACES §21:
//!
//! ```text
//! priority(c) = w_contested·contested_mass + w_leverage·decision_leverage
//!             + w_gap·evidence_gap         − w_cost·resolution_cost
//! ```
//!
//! `decision_leverage` is not computed here — it *is*
//! [`crate::decision::triggers::CounterfactualFlip::leverage`], the exact pass
//! §21 says to reuse ("`decision_leverage` reuses the counterfactual machinery
//! already built for change triggers"). `resolution_cost` is not computed here
//! either: "estimated tokens for the exchange ÷ remaining budget" needs a real
//! `BudgetLedger`, which this crate cannot depend on (D1) — the kernel stage
//! computes that one ratio and passes it in alongside the other three.
//!
//! `dispute_priority`'s literal signature in INTERFACES §21
//! (`fn dispute_priority(c: &CanonicalClaim, g: &ResolvedGraph, cfg:
//! &PolicyConfig) -> f64`) is not reproduced verbatim: neither `ResolvedGraph`
//! nor `PolicyConfig` is given a concrete definition anywhere (D19's
//! category), and the four terms are computed by two different layers (core
//! for `contested_mass`/`decision_leverage`/`evidence_gap`, the kernel for
//! `resolution_cost`). Taking four already-computed `f64` components plus the
//! weights is the same shape as the pseudocode's formula itself, just without
//! a fictional struct standing in for values this crate cannot produce on its
//! own (PLAN_DEVIATIONS.md D36).

use crate::config::DisputeWeights;
use crate::ids::ClaimId;
use crate::relation::{Relation, RelationKind};
use std::collections::BTreeMap;

/// `Σ standing(attackers) + Σ standing(defenders) around c, normalised`
/// (INTERFACES §21). "Normalised" is not expanded into a formula there; read
/// literally as the mean standing of every claim contesting `claim_id` in
/// either direction (`Contradicts` in, `Supports` in) — bounded to `[0,1]`
/// by construction, since every individual standing already is, unlike a raw
/// sum which is not (PLAN_DEVIATIONS.md D36). A claim with no attacker and no
/// defender is not contested at all: `0.0`.
pub fn contested_mass(
    claim_id: &ClaimId,
    standing: &BTreeMap<ClaimId, f64>,
    relations: &[Relation],
) -> f64 {
    let mut sum = 0.0;
    let mut n = 0usize;
    for r in relations {
        if &r.to != claim_id {
            continue;
        }
        if !matches!(r.kind, RelationKind::Contradicts | RelationKind::Supports) {
            continue;
        }
        sum += standing.get(&r.from).copied().unwrap_or(0.0);
        n += 1;
    }
    if n == 0 {
        0.0
    } else {
        (sum / n as f64).clamp(0.0, 1.0)
    }
}

/// `1 − E(c)` (INTERFACES §21). Trivial, but named and tested so the formula
/// in [`dispute_priority`]'s callers reads the same way the spec's own table
/// does, term for term.
pub fn evidence_gap(e: f64) -> f64 {
    (1.0 - e).clamp(0.0, 1.0)
}

/// The weighted sum itself. Each component is precomputed by the caller —
/// see this module's own doc comment for why `resolution_cost` in particular
/// can only come from the kernel. Not clamped: this is a ranking score, never
/// a probability, and `dispute.rs`'s only caller sorts by it rather than
/// displaying it directly.
pub fn dispute_priority(
    contested_mass: f64,
    decision_leverage: f64,
    evidence_gap: f64,
    resolution_cost: f64,
    w: &DisputeWeights,
) -> f64 {
    w.w_contested * contested_mass + w.w_leverage * decision_leverage + w.w_gap * evidence_gap
        - w.w_cost * resolution_cost
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(from: &str, to: &str, kind: RelationKind) -> Relation {
        Relation {
            from: ClaimId::new(from),
            to: ClaimId::new(to),
            kind,
            confidence: 1.0,
        }
    }

    #[test]
    fn a_claim_nobody_contests_has_zero_mass() {
        let standing = BTreeMap::new();
        assert_eq!(contested_mass(&ClaimId::new("lonely"), &standing, &[]), 0.0);
    }

    #[test]
    fn mass_is_the_mean_standing_of_every_attacker_and_defender() {
        let mut standing = BTreeMap::new();
        standing.insert(ClaimId::new("attacker"), 0.8);
        standing.insert(ClaimId::new("defender"), 0.4);
        let relations = vec![
            rel("attacker", "c", RelationKind::Contradicts),
            rel("defender", "c", RelationKind::Supports),
        ];
        let mass = contested_mass(&ClaimId::new("c"), &standing, &relations);
        assert!((mass - 0.6).abs() < 1e-9, "mean of 0.8 and 0.4 is 0.6");
    }

    /// Relations pointing at a *different* claim, or of a kind that isn't
    /// attack/support (`Qualifies`, `Unrelated`, `Uncertain`), must not count.
    #[test]
    fn only_contradicts_and_supports_edges_into_this_claim_count() {
        let mut standing = BTreeMap::new();
        standing.insert(ClaimId::new("x"), 1.0);
        let relations = vec![
            rel("x", "other_claim", RelationKind::Contradicts),
            rel("x", "c", RelationKind::Qualifies),
            rel("x", "c", RelationKind::Unrelated),
        ];
        assert_eq!(
            contested_mass(&ClaimId::new("c"), &standing, &relations),
            0.0
        );
    }

    #[test]
    fn mass_never_exceeds_one_even_with_many_contesting_claims() {
        let mut standing = BTreeMap::new();
        let mut relations = Vec::new();
        for i in 0..10 {
            let id = format!("a{i}");
            standing.insert(ClaimId::new(&id), 1.0);
            relations.push(rel(&id, "c", RelationKind::Contradicts));
        }
        assert_eq!(
            contested_mass(&ClaimId::new("c"), &standing, &relations),
            1.0
        );
    }

    #[test]
    fn evidence_gap_is_one_minus_evidence() {
        assert!((evidence_gap(0.7) - 0.3).abs() < 1e-9);
        assert_eq!(evidence_gap(0.0), 1.0);
        assert_eq!(evidence_gap(1.0), 0.0);
    }

    /// INTERFACES §21's own worked description: contested_mass and evidence_gap
    /// both pull priority up, resolution_cost pulls it down, at the default
    /// weights (0.35 / 0.35 / 0.20 / 0.10).
    #[test]
    fn priority_combines_the_four_terms_at_the_default_weights() {
        let w = DisputeWeights::default();
        let p = dispute_priority(0.8, 0.5, 0.6, 0.2, &w);
        let expected = 0.35 * 0.8 + 0.35 * 0.5 + 0.20 * 0.6 - 0.10 * 0.2;
        assert!((p - expected).abs() < 1e-9);
    }

    #[test]
    fn a_cheaper_dispute_outranks_an_equally_useful_expensive_one() {
        let w = DisputeWeights::default();
        let cheap = dispute_priority(0.5, 0.5, 0.5, 0.1, &w);
        let expensive = dispute_priority(0.5, 0.5, 0.5, 0.9, &w);
        assert!(cheap > expensive);
    }
}
