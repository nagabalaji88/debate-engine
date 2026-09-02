//! Confidence, ARCHITECTURE §6.7 / INTERFACES §14: three evidence dimensions minus
//! five penalties — not "five terms", which stopped being true the moment truncation
//! and convergence were added. The implementation evaluates this formula; it does
//! not invent it, and every component is stored so `arbiter explain` always answers
//! "why?"

use crate::config::ConfidenceWeights;
use crate::judge::Scorecard;
use crate::option::OptionScore;
use serde::{Deserialize, Serialize};

/// Everything §6.7's formula reads that is not a judge scorecard or an
/// [`OptionScore`]. Plain scalars/bools, matching C5's `OutcomeInputs` convention
/// (PLAN_DEVIATIONS.md D12): the full `Completeness` enum is C8's concern.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PenaltyInputs {
    /// Share of decision-critical claims still `Unresolved` or `Disputed`.
    pub unresolved_critical_ratio: f64,
    /// Share of decisive claims whose evidence is an unverified assumption.
    pub assumption_dependency_ratio: f64,
    /// True when this run is `Completeness::Truncated`.
    pub truncated: bool,
    /// [`crate::decision::fixpoint::FixpointResult::converged`] — `false` applies
    /// `convergence_penalty`.
    pub fixpoint_converged: bool,
}

/// INTERFACES §14's `ConfidenceBreakdown`, plus `judge_dispersion` itself (`None`
/// when `judge_count <= 1`) — the spec's own struct omits it, but `explain --json`'s
/// penalties array (§22) prints it as the `dispersion` entry's `input`, so it must
/// survive past this function rather than being folded into `dispersion_penalty`
/// and discarded.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceBreakdown {
    pub evidence_mass: f64,
    pub decision_margin: f64,
    pub judge_score: f64,
    pub base: f64,

    pub unresolved_penalty: f64,
    pub assumption_penalty: f64,
    pub truncation_penalty: f64,
    pub convergence_penalty: f64,
    pub dispersion_penalty: f64,
    pub judge_dispersion: Option<f64>,

    pub total: f64,
}

/// Population standard deviation (÷n, not ÷n−1) — INTERFACES §14 is explicit that
/// this is population, not sample: "For two judges that is exactly half their gap."
fn population_stdev(values: &[f64]) -> f64 {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    variance.sqrt()
}

/// `scores` ranks options the same way [`crate::decision::outcome::classify`] does
/// (share descending, `OptionId` ascending as a deterministic tie-break) so
/// `decision_margin` can never disagree with the outcome the confidence describes.
pub fn confidence(
    evidence_mass: f64,
    scores: &[OptionScore],
    judges: &[Scorecard],
    penalties: &PenaltyInputs,
    weights: &ConfidenceWeights,
) -> ConfidenceBreakdown {
    let mut ranked: Vec<&OptionScore> = scores.iter().collect();
    ranked.sort_by(|a, b| {
        b.share
            .partial_cmp(&a.share)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    let top1_share = ranked.first().map(|o| o.share).unwrap_or(0.0);
    let top2_share = ranked.get(1).map(|o| o.share).unwrap_or(0.0);
    let decision_margin = (top1_share - top2_share).clamp(0.0, 1.0);

    let weighted_judge_scores: Vec<f64> = judges.iter().map(Scorecard::weighted).collect();
    let judge_score = if weighted_judge_scores.is_empty() {
        0.0
    } else {
        weighted_judge_scores.iter().sum::<f64>() / weighted_judge_scores.len() as f64
    };
    let judge_dispersion = if weighted_judge_scores.len() >= 2 {
        Some(population_stdev(&weighted_judge_scores))
    } else {
        None
    };

    let evidence_mass = evidence_mass.clamp(0.0, 1.0);
    let judge_score = judge_score.clamp(0.0, 1.0);

    let base = weights.evidence_mass * evidence_mass
        + weights.margin * decision_margin
        + weights.judge * judge_score;

    let unresolved_penalty = weights.unresolved_penalty * penalties.unresolved_critical_ratio;
    let assumption_penalty = weights.assumption_penalty * penalties.assumption_dependency_ratio;
    let truncation_penalty = if penalties.truncated {
        weights.truncation_penalty
    } else {
        0.0
    };
    let convergence_penalty = if !penalties.fixpoint_converged {
        weights.convergence_penalty
    } else {
        0.0
    };
    let dispersion_penalty = weights.dispersion_weight
        * (judge_dispersion.unwrap_or(0.0) - weights.dispersion_threshold).max(0.0);

    let total = (base
        - unresolved_penalty
        - assumption_penalty
        - truncation_penalty
        - convergence_penalty
        - dispersion_penalty)
        .clamp(0.0, 1.0);

    ConfidenceBreakdown {
        evidence_mass,
        decision_margin,
        judge_score,
        base,
        unresolved_penalty,
        assumption_penalty,
        truncation_penalty,
        convergence_penalty,
        dispersion_penalty,
        judge_dispersion,
        total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ModelId, OptionId};

    fn score(id: &str, share: f64) -> OptionScore {
        OptionScore {
            id: OptionId::new(id),
            label: id.to_string(),
            raw: share,
            share,
        }
    }

    fn flat_scorecard(v: f64) -> Scorecard {
        Scorecard {
            model: ModelId::new("m"),
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

    fn no_penalties() -> PenaltyInputs {
        PenaltyInputs {
            unresolved_critical_ratio: 0.0,
            assumption_dependency_ratio: 0.0,
            truncated: false,
            fixpoint_converged: true,
        }
    }

    #[test]
    fn worked_example_matches_the_spec_exactly() {
        // INTERFACES §14 / ARCHITECTURE §6.7's pinned worked example.
        let scores = [score("a", 0.905), score("b", 0.095)]; // margin = 0.81
        let judges = [flat_scorecard(0.91)]; // single judge, dispersion inactive
        let penalties = PenaltyInputs {
            unresolved_critical_ratio: 0.08,
            assumption_dependency_ratio: 0.07,
            truncated: false,
            fixpoint_converged: true,
        };
        let b = confidence(
            0.88,
            &scores,
            &judges,
            &penalties,
            &ConfidenceWeights::default(),
        );

        assert!((b.base - 0.8695).abs() < 1e-9, "base was {}", b.base);
        assert!(
            (b.unresolved_penalty - 0.0200).abs() < 1e-9,
            "unresolved_penalty was {}",
            b.unresolved_penalty
        );
        assert!(
            (b.assumption_penalty - 0.0105).abs() < 1e-9,
            "assumption_penalty was {}",
            b.assumption_penalty
        );
        assert_eq!(b.truncation_penalty, 0.0);
        assert_eq!(b.convergence_penalty, 0.0);
        assert_eq!(b.dispersion_penalty, 0.0);
        assert_eq!(b.judge_dispersion, None);
        assert!((b.total - 0.8390).abs() < 1e-9, "total was {}", b.total);
    }

    #[test]
    fn dimensions_and_penalties_reconstruct_total_exactly() {
        let scores = [score("a", 0.905), score("b", 0.095)];
        let judges = [flat_scorecard(0.91)];
        let penalties = PenaltyInputs {
            unresolved_critical_ratio: 0.08,
            assumption_dependency_ratio: 0.07,
            truncated: false,
            fixpoint_converged: true,
        };
        let b = confidence(
            0.88,
            &scores,
            &judges,
            &penalties,
            &ConfidenceWeights::default(),
        );
        let recomputed = b.base
            - b.unresolved_penalty
            - b.assumption_penalty
            - b.truncation_penalty
            - b.convergence_penalty
            - b.dispersion_penalty;
        assert!(
            (b.total - recomputed.clamp(0.0, 1.0)).abs() < 1e-9,
            "total must be recomputed from its own stored components, never stored independently"
        );
    }

    #[test]
    fn dispersion_table_matches_the_spec_for_every_row() {
        let w = ConfidenceWeights::default();
        let cases = [
            (0.85, 0.75, 0.0),
            (0.80, 0.50, 0.0), // exactly at the threshold, not past it
            (0.90, 0.50, 0.010),
            (1.00, 0.00, 0.070), // the two-judge maximum
        ];
        for (a, b_score, expected_penalty) in cases {
            let scores = [score("x", 1.0)];
            let judges = [flat_scorecard(a), flat_scorecard(b_score)];
            let out = confidence(0.5, &scores, &judges, &no_penalties(), &w);
            assert!(
                (out.dispersion_penalty - expected_penalty).abs() < 1e-9,
                "judges {a}/{b_score}: expected penalty {expected_penalty}, got {}",
                out.dispersion_penalty
            );
        }
    }

    #[test]
    fn a_single_judge_never_pays_a_dispersion_penalty() {
        let scores = [score("x", 1.0)];
        let judges = [flat_scorecard(1.0)]; // maximal possible spread if it were paired
        let out = confidence(
            0.5,
            &scores,
            &judges,
            &no_penalties(),
            &ConfidenceWeights::default(),
        );
        assert_eq!(out.dispersion_penalty, 0.0);
        assert_eq!(out.judge_dispersion, None);
    }

    #[test]
    fn truncation_and_convergence_penalties_are_flat_not_scaled() {
        let scores = [score("x", 1.0)];
        let judges = [flat_scorecard(0.9)];
        let w = ConfidenceWeights::default();

        let mut p = no_penalties();
        p.truncated = true;
        let truncated = confidence(0.5, &scores, &judges, &p, &w);
        assert_eq!(truncated.truncation_penalty, w.truncation_penalty);

        let mut p = no_penalties();
        p.fixpoint_converged = false;
        let unconverged = confidence(0.5, &scores, &judges, &p, &w);
        assert_eq!(unconverged.convergence_penalty, w.convergence_penalty);
    }

    #[test]
    fn total_is_clamped_at_zero_never_negative() {
        let scores = [score("x", 1.0)];
        let judges = [flat_scorecard(0.0)];
        let penalties = PenaltyInputs {
            unresolved_critical_ratio: 1.0,
            assumption_dependency_ratio: 1.0,
            truncated: true,
            fixpoint_converged: false,
        };
        let out = confidence(
            0.0,
            &scores,
            &judges,
            &penalties,
            &ConfidenceWeights::default(),
        );
        assert_eq!(out.total, 0.0);
    }

    #[test]
    fn decision_margin_agrees_with_outcome_classifications_ranking() {
        // Order the input slice differently from share order; the ranking inside
        // `confidence` must not depend on input order, matching `outcome::classify`.
        let scores = [score("b", 0.30), score("a", 0.70)];
        let judges = [flat_scorecard(0.9)];
        let out = confidence(
            0.9,
            &scores,
            &judges,
            &no_penalties(),
            &ConfidenceWeights::default(),
        );
        assert!((out.decision_margin - 0.40).abs() < 1e-9);
    }
}
