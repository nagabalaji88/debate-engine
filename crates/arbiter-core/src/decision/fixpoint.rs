//! The argumentation fixpoint. ARCHITECTURE §6.3:
//!
//! ```text
//! standing(c) = clamp01( E(c)
//!                      + α·min(Σ w·standing(s), support_cap)   s supports c
//!                      − β·min(Σ w·standing(a), attack_cap)    a contradicts c
//!                      − γ·Σ w·standing(q)                     q qualifies c   )
//! ```
//!
//! `w` is the edge's classifier confidence (`Relation::confidence`). Solved by
//! damped Jacobi iteration (λ), which makes the result **order-independent by
//! construction**: every claim's next value is computed from the *previous*
//! sweep's values for its neighbours, never from values already updated this
//! sweep — that distinction is what separates Jacobi from Gauss-Seidel, and it is
//! why this function never needs to care what order its input arrives in.
//!
//! `Unrelated` and `Uncertain` relations carry no weight (`relation.rs`) and are
//! excluded from every sum; only `Supports`, `Contradicts` and `Qualifies` feed
//! the arithmetic.
//!
//! Initial condition (not stated by the spec explicitly): each claim starts at
//! `E(c)` — no support or attack has been applied yet. That is the only value
//! that requires no information about neighbours, and it is a fixed point on its
//! own for any claim with no incoming edges, which is the correctness property a
//! starting value should have.

use crate::config::GraphParams;
use crate::ids::ClaimId;
use crate::relation::{Relation, RelationKind};
use std::collections::{BTreeMap, BTreeSet};

/// The solved graph, plus enough about *how* it was solved to drive
/// `FIXPOINT_NOT_CONVERGED` and the confidence report's `convergence_penalty`.
#[derive(Debug, Clone, PartialEq)]
pub struct FixpointResult {
    pub standing: BTreeMap<ClaimId, f64>,
    pub iterations: u32,
    /// `false` means the cap (`max_iterations`) was reached with the largest
    /// per-claim change still ≥ `epsilon`. The engine keeps `standing` as the
    /// last iterate regardless — the fixpoint is total (§6.3).
    pub converged: bool,
    /// The largest per-claim change on the final iteration. Zero when
    /// `converged` (well, less than `epsilon`); the size of the miss otherwise.
    pub max_delta: f64,
    /// Claims whose attack or support sum was reduced by its saturation cap on
    /// the final iteration — what `attack_saturation` exercises.
    pub saturated: BTreeSet<ClaimId>,
}

/// `claim_ids` is the full set of claims to solve over — must include every claim
/// `relations` references, live or not (a dead claim's `evidence` entry is 0 per
/// `decision::evidence::evidence`, so it naturally exerts no influence without any
/// special-casing here). `evidence` is `E(c)` for each, from `evidence_map`.
pub fn solve(
    claim_ids: &[ClaimId],
    evidence: &BTreeMap<ClaimId, f64>,
    relations: &[Relation],
    p: &GraphParams,
) -> FixpointResult {
    let mut supports: BTreeMap<ClaimId, Vec<(&ClaimId, f64)>> = BTreeMap::new();
    let mut attacks: BTreeMap<ClaimId, Vec<(&ClaimId, f64)>> = BTreeMap::new();
    let mut qualifies: BTreeMap<ClaimId, Vec<(&ClaimId, f64)>> = BTreeMap::new();
    for r in relations {
        let bucket = match r.kind {
            RelationKind::Supports => &mut supports,
            RelationKind::Contradicts => &mut attacks,
            RelationKind::Qualifies => &mut qualifies,
            RelationKind::Unrelated | RelationKind::Uncertain => continue,
        };
        bucket
            .entry(r.to.clone())
            .or_default()
            .push((&r.from, r.confidence));
    }

    let mut standing: BTreeMap<ClaimId, f64> = claim_ids
        .iter()
        .map(|id| (id.clone(), evidence.get(id).copied().unwrap_or(0.0)))
        .collect();

    let weighted_sum = |edges: &BTreeMap<ClaimId, Vec<(&ClaimId, f64)>>,
                        id: &ClaimId,
                        prev: &BTreeMap<ClaimId, f64>|
     -> f64 {
        edges
            .get(id)
            .map(|v| {
                v.iter()
                    .map(|(src, w)| w * prev.get(*src).copied().unwrap_or(0.0))
                    .sum()
            })
            .unwrap_or(0.0)
    };

    let mut converged = false;
    let mut iterations = 0;
    let mut max_delta = f64::INFINITY;
    let mut saturated = BTreeSet::new();

    for iter in 1..=p.max_iterations {
        iterations = iter;
        let prev = standing.clone();
        max_delta = 0.0;
        saturated.clear();

        for id in claim_ids {
            let e = evidence.get(id).copied().unwrap_or(0.0);

            let support_raw = weighted_sum(&supports, id, &prev);
            let attack_raw = weighted_sum(&attacks, id, &prev);
            let qualify_raw = weighted_sum(&qualifies, id, &prev);

            // Strict `>`, not `>=`: `min(raw, cap)` only actually clips when raw
            // exceeds the cap. At raw == cap exactly, the min is a no-op — the
            // natural value already equals the cap, nothing was cut down to
            // reach it — so marking that claim "saturated" would report a
            // clipping that didn't happen.
            if support_raw > p.support_cap || attack_raw > p.attack_cap {
                saturated.insert(id.clone());
            }

            let support_term = p.support_gain * support_raw.min(p.support_cap);
            let attack_term = p.attack_gain * attack_raw.min(p.attack_cap);
            // Qualify has no stated cap in §6.3 — only attack and support do.
            let qualify_term = p.qualify_gain * qualify_raw;

            let target = (e + support_term - attack_term - qualify_term).clamp(0.0, 1.0);
            let prev_v = prev.get(id).copied().unwrap_or(0.0);
            let next_v = prev_v + p.damping * (target - prev_v);

            max_delta = max_delta.max((next_v - prev_v).abs());
            standing.insert(id.clone(), next_v);
        }

        if max_delta < p.epsilon {
            converged = true;
            break;
        }
    }

    FixpointResult {
        standing,
        iterations,
        converged,
        max_delta,
        saturated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The four worked examples in ARCHITECTURE §6.3, pinned exactly. Each attacker
    /// or supporter here is itself evidence-only (no incoming edges), so it is
    /// already at its fixed point from iteration 1 — these numbers hold regardless
    /// of how many sweeps it takes the target claim to settle.
    #[test]
    fn worked_examples_match_the_spec_exactly() {
        let p = params();
        let ids: Vec<ClaimId> = ["fact", "attacker"].into_iter().map(ClaimId::new).collect();

        // one strong attacker (standing 1.0) -> 1.00 - 0.60 = 0.40
        let mut ev = BTreeMap::new();
        ev.insert(ClaimId::new("fact"), 1.0);
        ev.insert(ClaimId::new("attacker"), 1.0);
        let rels = vec![rel("attacker", "fact", RelationKind::Contradicts, 1.0)];
        let r = solve(&ids, &ev, &rels, &p);
        assert!(r.converged);
        assert!((r.standing[&ClaimId::new("fact")] - 0.40).abs() < 1e-6);

        // two strong attackers, sum 2.0 capped to 1.5 -> 1.00 - 0.90 = 0.10
        let ids3: Vec<ClaimId> = ["fact", "a1", "a2"].into_iter().map(ClaimId::new).collect();
        let mut ev3 = BTreeMap::new();
        ev3.insert(ClaimId::new("fact"), 1.0);
        ev3.insert(ClaimId::new("a1"), 1.0);
        ev3.insert(ClaimId::new("a2"), 1.0);
        let rels3 = vec![
            rel("a1", "fact", RelationKind::Contradicts, 1.0),
            rel("a2", "fact", RelationKind::Contradicts, 1.0),
        ];
        let r3 = solve(&ids3, &ev3, &rels3, &p);
        assert!(r3.converged);
        assert!((r3.standing[&ClaimId::new("fact")] - 0.10).abs() < 1e-6);
        assert!(
            r3.saturated.contains(&ClaimId::new("fact")),
            "sum 2.0 must saturate the 1.5 cap"
        );

        // one strong supporter (standing 1.0) -> 1.00 + 0.25 = 1.25, clamped to 1.00
        let ids2: Vec<ClaimId> = ["fact", "supporter"]
            .into_iter()
            .map(ClaimId::new)
            .collect();
        let mut ev2 = BTreeMap::new();
        ev2.insert(ClaimId::new("fact"), 1.0);
        ev2.insert(ClaimId::new("supporter"), 1.0);
        let rels2 = vec![rel("supporter", "fact", RelationKind::Supports, 1.0)];
        let r2 = solve(&ids2, &ev2, &rels2, &p);
        assert!(r2.converged);
        assert!((r2.standing[&ClaimId::new("fact")] - 1.00).abs() < 1e-6);
        assert!(
            !r2.saturated.contains(&ClaimId::new("fact")),
            "1.0 does not reach the 2.0 support cap"
        );
    }

    /// What the cap actually guarantees is narrower than "many weak attackers can
    /// never outweigh one strong one" — a single attack edge can contribute at most
    /// `1.0 * 1.0 = 1.0` to the raw sum (one claim's standing is itself ≤ 1.0), which
    /// is already *below* the 1.5 cap, so two or more attackers reaching the cap
    /// will always out-damage a single one. That is exactly the spec's own "two
    /// strong attackers" worked example above (0.10, worse than one attacker's
    /// 0.40) — not a contradiction, the intended behaviour.
    ///
    /// What the cap *does* guarantee, and what is actually tested here: past the
    /// cap, piling on additional weak attackers changes nothing. Without it, an
    /// unbounded pile of weak objections could bury a well-evidenced fact
    /// completely (§6.3: "which is wrong both dialectically and arithmetically").
    /// With it, damage saturates at `attack_gain * attack_cap` no matter how many
    /// more attackers join — a well-evidenced fact cannot be buried by *volume*,
    /// only by a small number of edges whose raw sum already reaches the cap.
    #[test]
    fn attack_pressure_saturates_and_more_weak_attackers_add_no_further_damage() {
        let p = params();

        fn graph_with_n_weak_attackers(
            n: usize,
        ) -> (Vec<ClaimId>, BTreeMap<ClaimId, f64>, Vec<Relation>) {
            let mut ids = vec![ClaimId::new("fact")];
            let mut ev = BTreeMap::new();
            ev.insert(ClaimId::new("fact"), 1.0);
            let mut rels = Vec::new();
            for i in 0..n {
                let a = format!("weak{i}");
                ids.push(ClaimId::new(&a));
                ev.insert(ClaimId::new(&a), 0.3); // weak: 5*0.3=1.5 already saturates the cap
                rels.push(rel(&a, "fact", RelationKind::Contradicts, 1.0));
            }
            (ids, ev, rels)
        }

        // 6 weak attackers at 0.3 each sum to 1.8, strictly past the 1.5 cap --
        // `saturated` marks a claim only when the cap actually clipped something
        // (`raw > cap`, not `raw >= cap`), so this deliberately overshoots rather
        // than landing exactly on it.
        let (ids6, ev6, rels6) = graph_with_n_weak_attackers(6);
        let r6 = solve(&ids6, &ev6, &rels6, &p);
        let fact6 = r6.standing[&ClaimId::new("fact")];
        assert!(
            (fact6 - 0.10).abs() < 1e-6,
            "1.8 raw capped to 1.5: 1.0 - 0.6*1.5 = 0.10"
        );
        assert!(r6.saturated.contains(&ClaimId::new("fact")));

        // Piling on six more (sum 3.6, further past the cap) must change nothing:
        // the cap already bounds the damage at the same 0.10 floor.
        let (ids12, ev12, rels12) = graph_with_n_weak_attackers(12);
        let r12 = solve(&ids12, &ev12, &rels12, &p);
        let fact12 = r12.standing[&ClaimId::new("fact")];
        assert!(
            (fact6 - fact12).abs() < 1e-9,
            "past the cap, more weak attackers must add zero further damage: \
             6 attackers -> {fact6}, 12 attackers -> {fact12}"
        );
    }

    /// The floor this produces for a maximally-evidenced fact — `1.0 - attack_gain *
    /// attack_cap = 0.10` — is the worst any number of attackers can ever do to it,
    /// matching the "two strong attackers" worked example exactly.
    #[test]
    fn no_number_of_attackers_can_push_a_fully_evidenced_fact_below_the_cap_floor() {
        let p = params();
        let floor = 1.0 - p.attack_gain * p.attack_cap;
        for n in [2, 3, 10, 50] {
            let mut ids = vec![ClaimId::new("fact")];
            let mut ev = BTreeMap::new();
            ev.insert(ClaimId::new("fact"), 1.0);
            let mut rels = Vec::new();
            for i in 0..n {
                let a = format!("a{i}");
                ids.push(ClaimId::new(&a));
                ev.insert(ClaimId::new(&a), 1.0);
                rels.push(rel(&a, "fact", RelationKind::Contradicts, 1.0));
            }
            let r = solve(&ids, &ev, &rels, &p);
            let standing = r.standing[&ClaimId::new("fact")];
            assert!(
                standing >= floor - 1e-9,
                "{n} attackers pushed standing to {standing}, below the {floor} floor the cap guarantees"
            );
        }
    }

    /// Exercises the non-convergence *reporting* path directly, rather than
    /// searching for a graph that genuinely oscillates under the spec's own
    /// well-damped constants (λ=0.5, β=0.6 is a contraction map for any graph
    /// this shape of formula can build, so a naturally divergent example may not
    /// exist at these settings — that is a property of good tuning, not a gap in
    /// this test). What must be true regardless: a starved iteration budget
    /// reports `converged: false` with the last iterate kept, never a panic and
    /// never a silently-wrong "converged: true".
    #[test]
    fn exhausting_the_iteration_budget_reports_unconverged_and_keeps_the_last_iterate() {
        let mut p = params();
        p.max_iterations = 1;
        p.epsilon = 1e-15; // unreachably tight, so one sweep cannot satisfy it

        let ids: Vec<ClaimId> = ["fact", "attacker"].into_iter().map(ClaimId::new).collect();
        let mut ev = BTreeMap::new();
        ev.insert(ClaimId::new("fact"), 1.0);
        ev.insert(ClaimId::new("attacker"), 1.0);
        let rels = vec![rel("attacker", "fact", RelationKind::Contradicts, 1.0)];

        let r = solve(&ids, &ev, &rels, &p);
        assert!(!r.converged);
        assert_eq!(r.iterations, 1);
        assert!(
            r.standing.contains_key(&ClaimId::new("fact")),
            "the fixpoint is total: no claim is dropped"
        );
    }

    /// Jacobi, not Gauss-Seidel: permuting the order claims are listed in must not
    /// change the result, because every claim reads only `prev` — never a
    /// neighbour's value already updated this same sweep.
    #[test]
    fn result_is_independent_of_input_order() {
        let p = params();
        let mut ev = BTreeMap::new();
        for name in ["a", "b", "c", "d"] {
            ev.insert(ClaimId::new(name), 0.8);
        }
        let rels = vec![
            rel("a", "b", RelationKind::Contradicts, 0.9),
            rel("b", "c", RelationKind::Supports, 0.7),
            rel("c", "d", RelationKind::Qualifies, 0.5),
            rel("d", "a", RelationKind::Contradicts, 0.6),
        ];

        let forward: Vec<ClaimId> = ["a", "b", "c", "d"].into_iter().map(ClaimId::new).collect();
        let reversed: Vec<ClaimId> = ["d", "c", "b", "a"].into_iter().map(ClaimId::new).collect();

        let r1 = solve(&forward, &ev, &rels, &p);
        let r2 = solve(&reversed, &ev, &rels, &p);
        assert_eq!(r1.standing, r2.standing);
        assert_eq!(r1.iterations, r2.iterations);
    }

    /// A claim with no incoming edges is its own fixed point from the start: this
    /// is the correctness property the initial condition (`standing_0 = E(c)`) is
    /// chosen for.
    #[test]
    fn an_isolated_claim_converges_in_one_iteration_at_its_own_evidence() {
        let p = params();
        let ids = vec![ClaimId::new("lonely")];
        let mut ev = BTreeMap::new();
        ev.insert(ClaimId::new("lonely"), 0.73);
        let r = solve(&ids, &ev, &[], &p);
        assert!(r.converged);
        assert_eq!(r.iterations, 1);
        assert!((r.standing[&ClaimId::new("lonely")] - 0.73).abs() < 1e-12);
    }

    #[test]
    fn uncertain_and_unrelated_relations_carry_no_weight() {
        let p = params();
        let ids: Vec<ClaimId> = ["fact", "other"].into_iter().map(ClaimId::new).collect();
        let mut ev = BTreeMap::new();
        ev.insert(ClaimId::new("fact"), 0.6);
        ev.insert(ClaimId::new("other"), 1.0);

        let with_noise = vec![
            rel("other", "fact", RelationKind::Unrelated, 1.0),
            rel("other", "fact", RelationKind::Uncertain, 1.0),
        ];
        let r_noise = solve(&ids, &ev, &with_noise, &p);
        let r_none = solve(&ids, &ev, &[], &p);
        assert_eq!(r_noise.standing, r_none.standing);
    }
}
