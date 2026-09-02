//! F2 — the argumentation fixpoint (C2), ARCHITECTURE §18's CI suite.

use arbiter_core::decision::fixpoint::solve;
use arbiter_core::relation::{Relation, RelationKind};
use arbiter_core::{ClaimId, config::GraphParams};
use arbiter_fixtures::harness::policy;
use std::collections::BTreeMap;

fn rel(from: &str, to: &str, kind: RelationKind, confidence: f64) -> Relation {
    Relation {
        from: ClaimId::new(from),
        to: ClaimId::new(to),
        kind,
        confidence,
    }
}

/// `fixpoint_nonconvergence`: "oscillating graph hits the cap → deterministic
/// record." A starved iteration budget must report `converged: false` and
/// keep the last iterate rather than panicking or claiming a false
/// convergence — and running the same graph twice must produce the exact
/// same standing, since §6.3's own Jacobi solve has no source of
/// nondeterminism.
#[test]
fn fixpoint_nonconvergence() {
    let mut p = policy().config.graph;
    p.max_iterations = 1;
    p.epsilon = 1e-15; // unreachably tight -- one sweep cannot satisfy it

    let ids: Vec<ClaimId> = ["fact", "attacker"].into_iter().map(ClaimId::new).collect();
    let mut evidence = BTreeMap::new();
    evidence.insert(ClaimId::new("fact"), 1.0);
    evidence.insert(ClaimId::new("attacker"), 1.0);
    let relations = vec![rel("attacker", "fact", RelationKind::Contradicts, 1.0)];

    let first = solve(&ids, &evidence, &relations, &p);
    let second = solve(&ids, &evidence, &relations, &p);

    assert!(
        !first.converged,
        "a starved budget must report unconverged, not silently pass"
    );
    assert_eq!(first.iterations, 1);
    assert!(
        first.standing.contains_key(&ClaimId::new("fact")),
        "the fixpoint is total: no claim is dropped even when the solve did not converge"
    );
    assert_eq!(
        first.standing, second.standing,
        "the record must be deterministic -- identical inputs, identical output, every time"
    );
}

/// `attack_saturation`: "ten weak attackers cannot defeat one strong fact."
/// Ten individually weak attackers (standing 0.1 each) sum to 1.0, under the
/// 1.5 attack cap -- nowhere near enough to push a fully-evidenced fact
/// below `Thresholds::default().defeated` (0.15), unlike a handful of
/// *strong* attackers would.
#[test]
fn attack_saturation() {
    let p = GraphParams::default();
    let defeated_threshold = policy().config.thresholds.defeated;

    let mut ids = vec![ClaimId::new("fact")];
    let mut evidence = BTreeMap::new();
    evidence.insert(ClaimId::new("fact"), 1.0);
    let mut relations = Vec::new();
    for i in 0..10 {
        let attacker = format!("weak{i}");
        ids.push(ClaimId::new(&attacker));
        evidence.insert(ClaimId::new(&attacker), 0.1);
        relations.push(rel(&attacker, "fact", RelationKind::Contradicts, 1.0));
    }

    let result = solve(&ids, &evidence, &relations, &p);
    let fact_standing = result.standing[&ClaimId::new("fact")];

    assert!(
        fact_standing > defeated_threshold,
        "ten weak attackers must not defeat the fact: standing {fact_standing} should exceed \
         the defeated threshold {defeated_threshold}"
    );
    assert!(
        !result.saturated.contains(&ClaimId::new("fact")),
        "ten weak attackers summing to 1.0 must not even reach the 1.5 attack cap"
    );
}
