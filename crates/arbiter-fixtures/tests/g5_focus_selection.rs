//! F2 — `focus_selection` (G5), ARCHITECTURE §18's CI suite: "dispute
//! ranking picks leverage-bearing disputes, not the loudest."

use arbiter_core::config::DisputeWeights;
use arbiter_core::decision::dispute::dispute_priority;

/// Two claims, same evidence gap and resolution cost, differing only in
/// what they're made of: `loud` is heavily contested (many attackers and
/// defenders churning around it) but flipping it would not move the
/// decision at all (zero leverage) -- the noisiest claim in the debate,
/// and the least consequential. `quiet` draws barely any contest but is
/// exactly the claim standing between the top two options (leverage 1.0).
/// With INTERFACES §21's default weights (`w_contested` and `w_leverage`
/// both 0.35), `quiet`'s leverage term alone outweighs `loud`'s entire
/// contested-mass advantage -- ranking must surface the leverage-bearing
/// claim first, not the one generating the most noise.
#[test]
fn focus_selection() {
    let weights = DisputeWeights::default();
    let evidence_gap = 0.5;
    let resolution_cost = 0.1;

    let loud_priority = dispute_priority(
        /* contested_mass */ 1.0,
        /* decision_leverage */ 0.0,
        evidence_gap,
        resolution_cost,
        &weights,
    );
    let quiet_priority = dispute_priority(
        /* contested_mass */ 0.1,
        /* decision_leverage */ 1.0,
        evidence_gap,
        resolution_cost,
        &weights,
    );

    assert!(
        quiet_priority > loud_priority,
        "the leverage-bearing claim ({quiet_priority}) must outrank the merely loud one ({loud_priority})"
    );

    let mut disputes = [("loud", loud_priority), ("quiet", quiet_priority)];
    disputes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    assert_eq!(
        disputes[0].0, "quiet",
        "sorted by priority, the leverage-bearing dispute must come first"
    );
}
