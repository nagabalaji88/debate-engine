//! Outcome classification, ARCHITECTURE §6.6. Four rules, evaluated **in order** —
//! `INSUFFICIENT_EVIDENCE` first, so an unevidenced pair of options never reads as a
//! genuine split, and `CONSENSUS` reads claim standing only, never model alignment
//! (Principle 1 — see the §6.6 commentary on why "every model aligned or silent" was
//! withdrawn).

use crate::config::Thresholds;
use crate::option::OptionScore;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    InsufficientEvidence,
    SplitDecision,
    Consensus,
    MajorityWithDissent,
}

/// Everything §6.6's rules read that is not already carried on an [`OptionScore`].
///
/// Deliberately **not** the full `Completeness{reason: StopReason, missing_stages}`
/// enum INTERFACES §9 describes (PLAN_DEVIATIONS.md D12): `StopReason`/`StageName`
/// are pipeline/kernel concepts this pure decision core has no need of yet — `C8`
/// introduces `Completeness` itself, where `DecisionRecord` actually serializes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutcomeInputs {
    /// Mean standing of the claims decisive for the winning option.
    pub evidence_mass: f64,
    /// Share of decision-critical claims still `Unresolved` or `Disputed`.
    pub unresolved_critical_ratio: f64,
    /// True when some still-live claim attacks top1 at standing ≥ `dissent` (§6.4's
    /// "live attacker" test, applied to top1 specifically).
    pub live_dissent_against_top1: bool,
    /// True when this run is `Completeness::Truncated` — raises rule 1's evidence
    /// floor by `truncation_factor` (D12). Rule 3 is unaffected (D12).
    pub truncated: bool,
}

/// Applies ARCHITECTURE §6.6's four rules in order. `scores` should already exclude
/// retired options — [`crate::decision::attachment::score_options`] does this.
///
/// Reads [`OptionScore::share`] for every comparison against `option_floor` and
/// `min_margin`, not `raw` (PLAN_DEVIATIONS.md D13).
pub fn classify(
    scores: &[OptionScore],
    inputs: &OutcomeInputs,
    thresholds: &Thresholds,
) -> Outcome {
    let mut ranked: Vec<&OptionScore> = scores.iter().collect();
    ranked.sort_by(|a, b| {
        b.share
            .partial_cmp(&a.share)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });

    let top1_share = ranked.first().map(|o| o.share).unwrap_or(0.0);

    let evidence_floor = thresholds.min_evidence_mass
        * if inputs.truncated {
            thresholds.truncation_factor
        } else {
            1.0
        };

    // Rule 1 — INSUFFICIENT_EVIDENCE.
    if inputs.evidence_mass < evidence_floor
        || inputs.unresolved_critical_ratio > thresholds.max_unresolved_ratio
        || top1_share < thresholds.option_floor
    {
        return Outcome::InsufficientEvidence;
    }

    // Rule 2 — SPLIT_DECISION.
    if let Some(top2) = ranked.get(1) {
        let margin = top1_share - top2.share;
        if margin < thresholds.min_margin
            && top1_share >= thresholds.option_floor
            && top2.share >= thresholds.option_floor
        {
            return Outcome::SplitDecision;
        }
    }

    // Rule 3 — CONSENSUS.
    let every_other_below_floor = ranked[1..]
        .iter()
        .all(|o| o.share < thresholds.option_floor);
    if !inputs.live_dissent_against_top1
        && every_other_below_floor
        && inputs.evidence_mass >= thresholds.min_evidence_mass
    {
        return Outcome::Consensus;
    }

    // Rule 4 — otherwise.
    Outcome::MajorityWithDissent
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::OptionId;

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

    #[test]
    fn under_evidenced_pair_is_insufficient_evidence_never_split_decision() {
        // The exact latent bug ARCHITECTURE §6.6 names: two low-score options with a
        // small margin between them must not read as a genuine split.
        let scores = [score("a", 0.11), score("b", 0.08)];
        let outcome = classify(&scores, &inputs(0.90), &Thresholds::default());
        assert_eq!(outcome, Outcome::InsufficientEvidence);
    }

    #[test]
    fn evidence_mass_below_floor_is_insufficient_evidence() {
        let scores = [score("a", 0.70), score("b", 0.30)];
        let mut i = inputs(0.10);
        i.evidence_mass = 0.10; // below default min_evidence_mass 0.35
        let outcome = classify(&scores, &i, &Thresholds::default());
        assert_eq!(outcome, Outcome::InsufficientEvidence);
    }

    #[test]
    fn unresolved_critical_ratio_over_threshold_is_insufficient_evidence() {
        let scores = [score("a", 0.70), score("b", 0.30)];
        let mut i = inputs(0.90);
        i.unresolved_critical_ratio = 0.50; // above default max_unresolved_ratio 0.40
        let outcome = classify(&scores, &i, &Thresholds::default());
        assert_eq!(outcome, Outcome::InsufficientEvidence);
    }

    #[test]
    fn truncation_raises_the_rule_one_evidence_floor_only() {
        let thresholds = Thresholds::default();
        let scores = [score("a", 0.70), score("b", 0.30)];
        // 0.40 clears the plain floor (0.35) but not the truncated floor (0.35*1.2=0.42).
        let mut i = inputs(0.40);
        i.truncated = false;
        assert_ne!(
            classify(&scores, &i, &thresholds),
            Outcome::InsufficientEvidence
        );
        i.truncated = true;
        assert_eq!(
            classify(&scores, &i, &thresholds),
            Outcome::InsufficientEvidence
        );
    }

    #[test]
    fn close_well_evidenced_options_are_split_decision() {
        let scores = [score("a", 0.55), score("b", 0.45)]; // margin 0.10 < tau_gap 0.15
        let outcome = classify(&scores, &inputs(0.90), &Thresholds::default());
        assert_eq!(outcome, Outcome::SplitDecision);
    }

    #[test]
    fn wide_margin_with_no_dissent_and_no_rival_above_floor_is_consensus() {
        let scores = [score("a", 0.90), score("b", 0.10)]; // b below option_floor 0.20
        let outcome = classify(&scores, &inputs(0.90), &Thresholds::default());
        assert_eq!(outcome, Outcome::Consensus);
    }

    #[test]
    fn a_single_option_with_no_dissent_is_consensus() {
        let scores = [score("a", 1.0)];
        let outcome = classify(&scores, &inputs(0.90), &Thresholds::default());
        assert_eq!(outcome, Outcome::Consensus);
    }

    #[test]
    fn live_dissent_against_top1_blocks_consensus_even_with_no_rival_option() {
        let scores = [score("a", 0.90), score("b", 0.10)];
        let mut i = inputs(0.90);
        i.live_dissent_against_top1 = true;
        let outcome = classify(&scores, &i, &Thresholds::default());
        assert_eq!(outcome, Outcome::MajorityWithDissent);
    }

    #[test]
    fn a_rival_at_or_above_the_floor_with_a_wide_margin_is_majority_with_dissent() {
        // margin 0.90-0.25=0.65 clears tau_gap, so not SPLIT_DECISION; but b is at
        // the floor, so rule 3's "every other option < option_floor" fails.
        let scores = [score("a", 0.75), score("b", 0.25)];
        let outcome = classify(&scores, &inputs(0.90), &Thresholds::default());
        assert_eq!(outcome, Outcome::MajorityWithDissent);
    }

    #[test]
    fn empty_scores_is_insufficient_evidence() {
        let outcome = classify(&[], &inputs(0.90), &Thresholds::default());
        assert_eq!(outcome, Outcome::InsufficientEvidence);
    }

    #[test]
    fn rule_order_is_strict_insufficient_evidence_beats_a_would_be_split() {
        // Margin here (0.05) is also < tau_gap, so without rule ordering this could
        // misclassify as SPLIT_DECISION; option_floor must win first.
        let scores = [score("a", 0.15), score("b", 0.10)];
        let outcome = classify(&scores, &inputs(0.90), &Thresholds::default());
        assert_eq!(outcome, Outcome::InsufficientEvidence);
    }

    #[test]
    fn ranking_ties_break_deterministically_by_option_id() {
        let scores = [score("z", 0.50), score("a", 0.50)];
        // Same ranking (by share desc, id asc) regardless of input order.
        let scores_reordered = [score("a", 0.50), score("z", 0.50)];
        assert_eq!(
            classify(&scores, &inputs(0.90), &Thresholds::default()),
            classify(&scores_reordered, &inputs(0.90), &Thresholds::default())
        );
    }
}
