//! Pure helpers `decision.synthesize` (G9) needs that no earlier task's own
//! module already provides: which claims are "decisive"/"decision-critical"
//! for an option or a set of options, and the ratios ARCHITECTURE §6.6's
//! `OutcomeInputs` and §6.7's `PenaltyInputs` are defined over but never
//! give a derivation for (PLAN_DEVIATIONS.md D41).

use crate::claim::{CanonicalClaim, ClaimStanding, EvidenceKind};
use crate::decision::attachment::{AttachmentMatrix, Polarity};
use crate::decision::evidence::effective_kind;
use crate::ids::{ClaimId, OptionId};
use crate::option::OptionScore;
use std::collections::{BTreeMap, BTreeSet};

/// `scores`, ranked `share` descending / `OptionId` ascending — the exact
/// tie-break every other "top1/top2" concept in this crate uses
/// ([`crate::decision::rank_by_share`], `pub(crate)` and therefore not
/// reachable from outside this crate; this is the public door to it that
/// `decision.synthesize` needs before it can call `outcome::classify` or
/// `confidence()`, both of which want `top1`/`top2` identified up front).
pub fn ranked(scores: &[OptionScore]) -> Vec<&OptionScore> {
    super::rank_by_share(scores)
}

/// Every claim with a `Supports` or `Opposes` cell on `option_id` — exactly
/// the set [`crate::decision::attachment::score_options`] itself sums over
/// for that option, which is what "decisive for" an option means: these are
/// the claims whose standing actually moved its score.
pub fn decisive_claims(option_id: &OptionId, matrix: &AttachmentMatrix) -> BTreeSet<ClaimId> {
    matrix
        .cells
        .iter()
        .filter(|((_, opt), cell)| {
            opt == option_id && matches!(cell.polarity, Polarity::Supports | Polarity::Opposes)
        })
        .map(|((claim_id, _), _)| claim_id.clone())
        .collect()
}

/// The union of [`decisive_claims`] across every option given — "decision-
/// critical" (§6.7's `unresolved_critical_ratio`/`assumption_dependency_ratio`)
/// reads naturally as claims that matter to the decision as a whole, not
/// only to whichever option happens to be winning, unlike §6.6's
/// `evidence_mass` (explicitly "decisive for the *winning* option").
pub fn decision_critical_claims<'a>(
    option_ids: impl Iterator<Item = &'a OptionId>,
    matrix: &AttachmentMatrix,
) -> BTreeSet<ClaimId> {
    let mut all = BTreeSet::new();
    for id in option_ids {
        all.extend(decisive_claims(id, matrix));
    }
    all
}

/// Mean standing over a claim-id set, `0.0` for an empty set (never `NaN`
/// from a zero-length division).
pub fn mean_standing(claim_ids: &BTreeSet<ClaimId>, standing: &BTreeMap<ClaimId, f64>) -> f64 {
    if claim_ids.is_empty() {
        return 0.0;
    }
    let sum: f64 = claim_ids
        .iter()
        .map(|id| standing.get(id).copied().unwrap_or(0.0))
        .sum();
    sum / claim_ids.len() as f64
}

/// Share of `claim_ids` currently `Unresolved` or `Disputed`.
pub fn unresolved_or_disputed_ratio(
    claim_ids: &BTreeSet<ClaimId>,
    classified: &BTreeMap<ClaimId, ClaimStanding>,
) -> f64 {
    if claim_ids.is_empty() {
        return 0.0;
    }
    let n = claim_ids
        .iter()
        .filter(|id| {
            matches!(
                classified.get(*id),
                Some(ClaimStanding::Unresolved) | Some(ClaimStanding::Disputed)
            )
        })
        .count();
    n as f64 / claim_ids.len() as f64
}

/// Share of `claim_ids` whose effective evidence kind is `Assumption` — read
/// literally as that one `EvidenceKind` variant, not `Unverified` too:
/// "unverified assumption" (§6.7) names a stated-but-unverified assumption,
/// a different failure mode from `Unverified`'s "grounding never
/// established at all" (PLAN_DEVIATIONS.md D41).
pub fn assumption_dependency_ratio(
    claim_ids: &BTreeSet<ClaimId>,
    claims_by_id: &BTreeMap<ClaimId, &CanonicalClaim>,
) -> f64 {
    if claim_ids.is_empty() {
        return 0.0;
    }
    let n = claim_ids
        .iter()
        .filter(|id| {
            claims_by_id
                .get(*id)
                .map(|c| effective_kind(c) == EvidenceKind::Assumption)
                .unwrap_or(false)
        })
        .count();
    n as f64 / claim_ids.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::{ClaimLifecycle, ClaimMember, Grounding, TextSpan};
    use crate::decision::attachment::{AttachSource, Attachment};
    use crate::ids::{ModelId, PositionId, ProviderId};

    fn matrix_with(cells: Vec<((&str, &str), Polarity)>) -> AttachmentMatrix {
        let mut matrix = AttachmentMatrix::default();
        for ((claim, option), polarity) in cells {
            matrix.cells.insert(
                (ClaimId::new(claim), OptionId::new(option)),
                Attachment {
                    polarity,
                    confidence: 1.0,
                    source: AttachSource::Authored,
                },
            );
        }
        matrix
    }

    #[test]
    fn decisive_claims_includes_supports_and_opposes_only() {
        let matrix = matrix_with(vec![
            (("c1", "a"), Polarity::Supports),
            (("c2", "a"), Polarity::Opposes),
            (("c3", "a"), Polarity::Neutral),
            (("c4", "b"), Polarity::Supports),
        ]);
        let decisive = decisive_claims(&OptionId::new("a"), &matrix);
        assert_eq!(decisive.len(), 2);
        assert!(decisive.contains(&ClaimId::new("c1")));
        assert!(decisive.contains(&ClaimId::new("c2")));
    }

    #[test]
    fn decision_critical_claims_unions_across_every_option_given() {
        let matrix = matrix_with(vec![
            (("c1", "a"), Polarity::Supports),
            (("c2", "b"), Polarity::Supports),
        ]);
        let ids = [OptionId::new("a"), OptionId::new("b")];
        let critical = decision_critical_claims(ids.iter(), &matrix);
        assert_eq!(critical.len(), 2);
    }

    #[test]
    fn mean_standing_is_zero_for_an_empty_set() {
        assert_eq!(mean_standing(&BTreeSet::new(), &BTreeMap::new()), 0.0);
    }

    #[test]
    fn mean_standing_averages_over_the_given_claims_only() {
        let mut standing = BTreeMap::new();
        standing.insert(ClaimId::new("c1"), 0.8);
        standing.insert(ClaimId::new("c2"), 0.4);
        standing.insert(ClaimId::new("unrelated"), 0.0);
        let ids: BTreeSet<ClaimId> = [ClaimId::new("c1"), ClaimId::new("c2")].into();
        assert!((mean_standing(&ids, &standing) - 0.6).abs() < 1e-9);
    }

    #[test]
    fn unresolved_or_disputed_ratio_counts_both_standings() {
        let mut classified = BTreeMap::new();
        classified.insert(ClaimId::new("c1"), ClaimStanding::Agreed);
        classified.insert(ClaimId::new("c2"), ClaimStanding::Disputed);
        classified.insert(ClaimId::new("c3"), ClaimStanding::Unresolved);
        classified.insert(ClaimId::new("c4"), ClaimStanding::Defeated);
        let ids: BTreeSet<ClaimId> = classified.keys().cloned().collect();
        assert!((unresolved_or_disputed_ratio(&ids, &classified) - 0.5).abs() < 1e-9);
    }

    fn assumption_claim(id: &str, kind: EvidenceKind) -> CanonicalClaim {
        let member = ClaimMember::new(
            ClaimId::new(id),
            ModelId::new("m"),
            ProviderId::new("p"),
            PositionId::new("pos"),
            "text",
            Grounding::DirectQuote {
                span: TextSpan {
                    start: 0,
                    end: 4,
                    quote: "text".into(),
                },
            },
        );
        CanonicalClaim {
            id: ClaimId::new(id),
            text: "text".into(),
            kind,
            lifecycle: ClaimLifecycle::Proposed,
            members: vec![member],
        }
    }

    #[test]
    fn assumption_ratio_counts_assumption_kind_only_not_unverified() {
        let c1 = assumption_claim("c1", EvidenceKind::Assumption);
        let c2 = assumption_claim("c2", EvidenceKind::Fact);
        let claims_by_id: BTreeMap<ClaimId, &CanonicalClaim> =
            [(c1.id.clone(), &c1), (c2.id.clone(), &c2)].into();
        let ids: BTreeSet<ClaimId> = claims_by_id.keys().cloned().collect();
        assert!((assumption_dependency_ratio(&ids, &claims_by_id) - 0.5).abs() < 1e-9);
    }
}
