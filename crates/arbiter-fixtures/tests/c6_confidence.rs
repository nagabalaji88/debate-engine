//! F2 — confidence: 3 dimensions minus 5 penalties (C6), ARCHITECTURE §18's
//! CI suite.

use arbiter_core::config::ConfidenceWeights;
use arbiter_core::decision::confidence::{PenaltyInputs, confidence};
use arbiter_core::{ModelId, OptionId, OptionScore, Scorecard};

fn flat_scorecard(model: &str, v: f64) -> Scorecard {
    Scorecard {
        model: ModelId::new(model),
        factual_correctness: v,
        logical_reasoning: v,
        evidence_quality: v,
        problem_relevance: v,
        assumption_quality: v,
        counterargument_handling: v,
        risk_awareness: v,
        practicality: v,
        clarity: v,
    }
}

/// `confidence_arithmetic`: "every term independently hand-computed; pins
/// the formula." Every one of §6.7's terms computed by hand below and
/// checked against what `confidence()` actually returns -- this is the
/// formula pin, not just a plausibility check.
#[test]
fn confidence_arithmetic() {
    let scores = [
        OptionScore {
            id: OptionId::new("a"),
            label: "a".into(),
            raw: 0.905,
            share: 0.905,
        },
        OptionScore {
            id: OptionId::new("b"),
            label: "b".into(),
            raw: 0.095,
            share: 0.095,
        },
    ];
    let judges = [flat_scorecard("m", 0.91)];
    let penalties = PenaltyInputs {
        unresolved_critical_ratio: 0.08,
        assumption_dependency_ratio: 0.07,
        truncated: false,
        fixpoint_converged: true,
    };
    let weights = ConfidenceWeights::default();

    let b = confidence(0.88, &scores, &judges, &penalties, &weights);

    // decision_margin = top1.share - top2.share = 0.905 - 0.095 = 0.81
    assert!((b.decision_margin - 0.81).abs() < 1e-9);
    // judge_score: one judge, weighted() of a flat 0.91 scorecard = 0.91
    // (the rubric's own weights sum to exactly 1.0, so a flat scorecard's
    // weighted average equals the flat value).
    assert!((b.judge_score - 0.91).abs() < 1e-9);
    // base = 0.35*0.88 + 0.30*0.81 + 0.35*0.91 = 0.308 + 0.243 + 0.3185
    assert!((b.base - 0.8695).abs() < 1e-9);
    // unresolved_penalty = 0.25 * 0.08 = 0.0200
    assert!((b.unresolved_penalty - 0.0200).abs() < 1e-9);
    // assumption_penalty = 0.15 * 0.07 = 0.0105
    assert!((b.assumption_penalty - 0.0105).abs() < 1e-9);
    assert_eq!(b.truncation_penalty, 0.0, "not truncated");
    assert_eq!(b.convergence_penalty, 0.0, "the fixpoint converged");
    assert_eq!(
        b.dispersion_penalty, 0.0,
        "one judge: no dispersion to penalize"
    );
    assert!(
        b.judge_dispersion.is_none(),
        "dispersion needs at least two judges"
    );
    // total = base - unresolved - assumption = 0.8695 - 0.0200 - 0.0105 = 0.8390
    assert!((b.total - 0.8390).abs() < 1e-9);
}

/// `judge_dispersion`: "two judges disagree → dispersion penalty applied
/// and reported." Judges at 0.9 and 0.5 have a population stdev of 0.2,
/// above `dispersion_threshold` (0.15), so the penalty is
/// `dispersion_weight * (0.2 - 0.15) = 0.20 * 0.05 = 0.01`, and the
/// dispersion value itself is reported (not silently absorbed).
#[test]
fn judge_dispersion() {
    let scores = [OptionScore {
        id: OptionId::new("a"),
        label: "a".into(),
        raw: 1.0,
        share: 1.0,
    }];
    let judges = [flat_scorecard("m1", 0.9), flat_scorecard("m2", 0.5)];
    let penalties = PenaltyInputs {
        unresolved_critical_ratio: 0.0,
        assumption_dependency_ratio: 0.0,
        truncated: false,
        fixpoint_converged: true,
    };
    let weights = ConfidenceWeights::default();

    let b = confidence(1.0, &scores, &judges, &penalties, &weights);

    assert!(
        b.judge_dispersion.is_some(),
        "two disagreeing judges must report a dispersion value"
    );
    let dispersion = b.judge_dispersion.unwrap();
    assert!(
        (dispersion - 0.2).abs() < 1e-9,
        "population stdev of [0.9, 0.5] is 0.2"
    );
    assert!(
        (b.dispersion_penalty - 0.01).abs() < 1e-9,
        "0.20 * (0.2 - 0.15) = 0.01, applied because dispersion exceeds the threshold"
    );
    assert!(
        b.dispersion_penalty > 0.0,
        "the penalty must actually be applied, not just computed"
    );
}
