//! `controller.decide`'s own stop predicates, ARCHITECTURE §5.5:
//!
//! ```text
//! NoNewInformation   new_canonical_claims < min_new_claims (2)
//!                    AND max |Δ standing| across all claims < min_standing_delta (0.05)
//!
//! Converged          no live attacker ≥ τ_dissent against the top option
//!                    AND margin(top1, top2) ≥ τ_gap × converged_margin_factor (1.5)
//!                    AND no unresolved claim is a change trigger
//! ```
//!
//! `converged_margin_factor`, `min_new_claims` and `min_standing_delta` are
//! not this crate's constants to own — IMPLEMENTATION_PLAN.md §0.6 assigns
//! them to "kernel controller" explicitly (D5) — so every function here
//! takes its thresholds as plain parameters rather than reading them off a
//! core-owned config struct. The kernel stage is what stores the actual
//! numbers and calls these.

use crate::decision::attachment::{AttachmentMatrix, Polarity};
use crate::decision::rank_by_share;
use crate::decision::triggers::CounterfactualFlip;
use crate::ids::{ClaimId, OptionId};
use crate::option::OptionScore;
use std::collections::BTreeMap;

/// `new_canonical_claims < min_new_claims AND max |Δ standing| < min_standing_delta`
/// — literal, both halves required.
pub fn no_new_information(
    new_claim_count: usize,
    max_standing_delta: f64,
    min_new_claims: usize,
    min_standing_delta: f64,
) -> bool {
    new_claim_count < min_new_claims && max_standing_delta < min_standing_delta
}

/// "no live attacker ≥ τ_dissent against the top option." Read over the
/// *propagated* matrix (Step 3, `attachment::propagate`), not the direct
/// one: propagation is exactly what already turns "a claim contradicting a
/// supporter of O" into an `Opposes` cell on O (ARCHITECTURE §5.3), so an
/// `Opposes` cell on the propagated matrix already *is* "a live attacker
/// against this option" — re-deriving it from the raw relation graph a
/// second time would just recompute what Step 3 already materialised.
pub fn has_live_dissent_against(
    option_id: &OptionId,
    matrix: &AttachmentMatrix,
    standing: &BTreeMap<ClaimId, f64>,
    dissent_threshold: f64,
) -> bool {
    matrix.cells.iter().any(|((claim_id, opt), cell)| {
        opt == option_id
            && cell.polarity == Polarity::Opposes
            && standing.get(claim_id).copied().unwrap_or(0.0) >= dissent_threshold
    })
}

/// `share(top1) - share(top2)`, `0.0` if fewer than two scored options —
/// the same definition `decision::triggers`' own `margin` uses internally
/// for `decision_leverage`, exposed here so the controller's own `Converged`
/// predicate reads it identically rather than a second, possibly-drifting
/// copy.
pub fn margin(scores: &[OptionScore]) -> f64 {
    let ranked = rank_by_share(scores);
    let top1 = ranked.first().map(|o| o.share).unwrap_or(0.0);
    let top2 = ranked.get(1).map(|o| o.share).unwrap_or(0.0);
    top1 - top2
}

/// All three `Converged` sub-predicates combined. `top_option` is `None`
/// when nothing has been scored at all — there is no "top option" for
/// dissent or margin to be evaluated against, so this is never converged.
#[allow(clippy::too_many_arguments)]
pub fn converged(
    scores: &[OptionScore],
    propagated_matrix: &AttachmentMatrix,
    standing: &BTreeMap<ClaimId, f64>,
    unresolved_claims: &[ClaimId],
    flips: &[CounterfactualFlip],
    dissent_threshold: f64,
    gap_threshold: f64,
    converged_margin_factor: f64,
) -> bool {
    let ranked = rank_by_share(scores);
    let Some(top) = ranked.first() else {
        return false;
    };

    let dissent_clear =
        !has_live_dissent_against(&top.id, propagated_matrix, standing, dissent_threshold);
    let margin_clear = margin(scores) >= gap_threshold * converged_margin_factor;
    let no_unresolved_trigger = !flips
        .iter()
        .any(|f| unresolved_claims.contains(&f.claim_id) && f.is_trigger);

    dissent_clear && margin_clear && no_unresolved_trigger
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::attachment::{AttachSource, Attachment};
    use crate::decision::triggers::FlipDirection;

    fn score(id: &str, share: f64) -> OptionScore {
        OptionScore {
            id: OptionId::new(id),
            label: id.to_string(),
            raw: share,
            share,
        }
    }

    #[test]
    fn no_new_information_requires_both_halves() {
        assert!(no_new_information(0, 0.01, 2, 0.05));
        assert!(
            !no_new_information(2, 0.01, 2, 0.05),
            "claim count not below the minimum"
        );
        assert!(
            !no_new_information(0, 0.05, 2, 0.05),
            "delta not below the minimum (not strictly less)"
        );
        assert!(!no_new_information(3, 0.10, 2, 0.05), "neither half holds");
    }

    #[test]
    fn dissent_is_read_from_opposes_cells_on_the_propagated_matrix() {
        let mut matrix = AttachmentMatrix::default();
        matrix.cells.insert(
            (ClaimId::new("attacker"), OptionId::new("opt_a")),
            Attachment {
                polarity: Polarity::Opposes,
                confidence: 1.0,
                source: AttachSource::Propagated,
            },
        );
        let mut standing = BTreeMap::new();
        standing.insert(ClaimId::new("attacker"), 0.5);

        assert!(has_live_dissent_against(
            &OptionId::new("opt_a"),
            &matrix,
            &standing,
            0.30
        ));
        assert!(!has_live_dissent_against(
            &OptionId::new("opt_a"),
            &matrix,
            &standing,
            0.60
        ));
        assert!(!has_live_dissent_against(
            &OptionId::new("opt_b"),
            &matrix,
            &standing,
            0.30
        ));
    }

    #[test]
    fn margin_is_zero_with_fewer_than_two_options() {
        assert_eq!(margin(&[]), 0.0);
        assert_eq!(margin(&[score("a", 1.0)]), 1.0);
    }

    #[test]
    fn converged_requires_all_three_subpredicates() {
        let scores = vec![score("winner", 0.8), score("loser", 0.2)];
        let matrix = AttachmentMatrix::default(); // no Opposes cells at all
        let standing = BTreeMap::new();
        let unresolved = vec![ClaimId::new("u1")];
        let flips = vec![CounterfactualFlip {
            claim_id: ClaimId::new("u1"),
            direction: FlipDirection::IfTrue,
            new_winner: None,
            margin_before: 0.6,
            margin_after: 0.6,
            is_trigger: false,
        }];

        assert!(converged(
            &scores,
            &matrix,
            &standing,
            &unresolved,
            &flips,
            0.30,
            0.15,
            1.5
        ));

        // Now make the unresolved claim a genuine trigger -- must flip to false.
        let mut triggering = flips.clone();
        triggering[0].is_trigger = true;
        assert!(!converged(
            &scores,
            &matrix,
            &standing,
            &unresolved,
            &triggering,
            0.30,
            0.15,
            1.5
        ));
    }

    #[test]
    fn converged_is_false_with_no_scored_options() {
        assert!(!converged(
            &[],
            &AttachmentMatrix::default(),
            &BTreeMap::new(),
            &[],
            &[],
            0.3,
            0.15,
            1.5
        ));
    }
}
