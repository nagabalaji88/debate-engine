//! F2 — outcome classification (C5), ARCHITECTURE §18's CI suite.

use arbiter_core::config::Thresholds;
use arbiter_core::decision::outcome::{OutcomeInputs, classify};
use arbiter_core::{OptionId, OptionScore, Outcome};

fn score(id: &str, share: f64) -> OptionScore {
    OptionScore {
        id: OptionId::new(id),
        label: id.to_string(),
        raw: share,
        share,
    }
}

fn inputs(evidence_mass: f64) -> OutcomeInputs {
    OutcomeInputs {
        evidence_mass,
        unresolved_critical_ratio: 0.0,
        live_dissent_against_top1: false,
        truncated: false,
    }
}

/// `split_decision`: "margin below τ, both options above floor." Two
/// well-evidenced options close enough together (margin 0.10 < τ_gap 0.15)
/// that neither wins outright, both individually well above the
/// `option_floor` (0.20).
#[test]
fn split_decision() {
    let scores = [score("a", 0.55), score("b", 0.45)];
    let outcome = classify(&scores, &inputs(0.90), &Thresholds::default());
    assert_eq!(outcome, Outcome::SplitDecision);
}

/// `insufficient_evidence`: "evidence floor triggers before classification."
/// Shares alone would read as a clean, wide-margin win for one option, but
/// `evidence_mass` sits below `min_evidence_mass` (0.35) -- Rule 1 fires
/// before the margin/consensus rules ever get a chance to look at the
/// shares at all.
#[test]
fn insufficient_evidence() {
    let scores = [score("a", 0.70), score("b", 0.30)];
    let outcome = classify(&scores, &inputs(0.20), &Thresholds::default());
    assert_eq!(outcome, Outcome::InsufficientEvidence);
}

/// `option_floor`: "two weak options, small margin → INSUFFICIENT, not
/// SPLIT." Both shares sit below `option_floor` (0.20) even though evidence
/// mass is otherwise healthy -- neither option earned enough of the debate
/// to be a real contender, so this must never read as a genuine split
/// between them (ARCHITECTURE §6.6's own named latent bug).
#[test]
fn option_floor() {
    let scores = [score("a", 0.11), score("b", 0.08)];
    let outcome = classify(&scores, &inputs(0.90), &Thresholds::default());
    assert_eq!(outcome, Outcome::InsufficientEvidence);
}
