//! Standing classification. ARCHITECTURE §6.4, evaluated in the listed order —
//! first match wins, exactly like §6.6's outcome classification is explicitly
//! ordered:
//!
//! ```text
//! Defeated    standing < 0.15, or lifecycle Withdrawn/Rejected
//! Disputed    has ≥1 live attacker (attacker standing ≥ 0.30)
//! Unresolved  Unverified/Unsupported and never resolved by challenge
//! Agreed      standing ≥ 0.50 with no live attacker
//! ```
//!
//! Two things this section does not spell out further, resolved in
//! `PLAN_DEVIATIONS.md` D8: what "resolved by challenge" means precisely, and what
//! a claim with no live attacker, non-`Unverified` kind and standing in
//! `[defeated, agreed)` classifies as — the four rules as literally written are not
//! jointly exhaustive over that band, though `ClaimStanding` is a closed four-variant
//! enum that must cover every claim.

use crate::claim::{CanonicalClaim, ClaimLifecycle, ClaimStanding, EvidenceKind};
use crate::config::Thresholds;
use crate::decision::evidence::effective_kind;
use crate::ids::ClaimId;
use crate::relation::{Relation, RelationKind};
use std::collections::BTreeMap;

/// D8, gap 1: a challenge is "resolved" only when it concluded with an outcome —
/// `Defended` (survived unchanged) or `Modified` (survived revised). `Proposed`,
/// `Verified` and `Challenged` are all still open; `Withdrawn`/`Rejected` never
/// reach this check because `classify` returns `Defeated` before calling it.
fn resolved_by_challenge(lifecycle: ClaimLifecycle) -> bool {
    matches!(
        lifecycle,
        ClaimLifecycle::Defended | ClaimLifecycle::Modified { .. }
    )
}

/// `claim_id` has a live attacker when some claim `a` with `a contradicts
/// claim_id` has its own standing at or above `thresholds.live_attacker` (§6.4).
/// Takes the full standing map (not just one claim's value) because it must look
/// up the *attacker's* standing, not the claim being classified.
pub fn has_live_attacker(
    claim_id: &ClaimId,
    standings: &BTreeMap<ClaimId, f64>,
    relations: &[Relation],
    t: &Thresholds,
) -> bool {
    relations.iter().any(|r| {
        r.kind == RelationKind::Contradicts
            && &r.to == claim_id
            && standings.get(&r.from).copied().unwrap_or(0.0) >= t.live_attacker
    })
}

/// The four §6.4 rules, in order, plus the D8 residual-band fallback. `standing`
/// is this claim's own fixpoint output; `has_live_attacker` should come from the
/// function above (kept separate so callers who already have that fact — e.g.
/// batch classification with a precomputed attacker index — don't redo the scan).
pub fn classify(
    claim: &CanonicalClaim,
    standing: f64,
    has_live_attacker: bool,
    t: &Thresholds,
) -> ClaimStanding {
    if standing < t.defeated
        || matches!(
            claim.lifecycle,
            ClaimLifecycle::Withdrawn | ClaimLifecycle::Rejected
        )
    {
        return ClaimStanding::Defeated;
    }
    if has_live_attacker {
        return ClaimStanding::Disputed;
    }
    let is_unverified = effective_kind(claim) == EvidenceKind::Unverified;
    if is_unverified && !resolved_by_challenge(claim.lifecycle) {
        return ClaimStanding::Unresolved;
    }
    if standing >= t.agreed {
        return ClaimStanding::Agreed;
    }
    // D8, gap 2: standing in [defeated, agreed), no live attacker, not Unverified.
    // None of the four literal rules match. Unresolved rather than Agreed (would
    // overstate settledness) or Disputed (implies an attacker that does not exist).
    ClaimStanding::Unresolved
}

/// Classifies every live claim in one pass, building the live-attacker index once
/// rather than re-scanning `relations` per claim (`has_live_attacker` is O(edges)
/// per call; this is O(edges) total).
pub fn classify_all(
    claims: &[CanonicalClaim],
    standings: &BTreeMap<ClaimId, f64>,
    relations: &[Relation],
    t: &Thresholds,
) -> BTreeMap<ClaimId, ClaimStanding> {
    let mut attacked: BTreeMap<&ClaimId, bool> = BTreeMap::new();
    for r in relations {
        if r.kind != RelationKind::Contradicts {
            continue;
        }
        let attacker_standing = standings.get(&r.from).copied().unwrap_or(0.0);
        if attacker_standing >= t.live_attacker {
            attacked.insert(&r.to, true);
        }
    }
    claims
        .iter()
        .map(|c| {
            let s = standings.get(&c.id).copied().unwrap_or(0.0);
            let live = attacked.get(&c.id).copied().unwrap_or(false);
            (c.id.clone(), classify(c, s, live, t))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::{ClaimMember, Grounding, TextSpan};
    use crate::ids::{ModelId, PositionId, ProviderId};

    fn quoted() -> Grounding {
        Grounding::DirectQuote {
            span: TextSpan {
                start: 0,
                end: 4,
                quote: "text".into(),
            },
        }
    }

    fn member() -> ClaimMember {
        ClaimMember::new(
            ClaimId::new("raw"),
            ModelId::new("m1"),
            ProviderId::new("p1"),
            PositionId::new("pos1"),
            "text",
            quoted(),
        )
    }

    fn claim(kind: EvidenceKind, lifecycle: ClaimLifecycle) -> CanonicalClaim {
        CanonicalClaim {
            id: ClaimId::new("C-1"),
            text: "t".into(),
            kind,
            lifecycle,
            members: vec![member()],
        }
    }

    #[test]
    fn low_standing_is_defeated_regardless_of_everything_else() {
        let t = Thresholds::default();
        let c = claim(EvidenceKind::Fact, ClaimLifecycle::Defended);
        assert_eq!(classify(&c, 0.10, false, &t), ClaimStanding::Defeated);
        // even with a live attacker and otherwise-agreeable standing, low wins first
        assert_eq!(
            classify(&c, t.defeated - 1e-9, true, &t),
            ClaimStanding::Defeated
        );
    }

    #[test]
    fn withdrawn_or_rejected_is_defeated_even_at_high_standing() {
        let t = Thresholds::default();
        let withdrawn = claim(EvidenceKind::Fact, ClaimLifecycle::Withdrawn);
        let rejected = claim(EvidenceKind::Fact, ClaimLifecycle::Rejected);
        assert_eq!(
            classify(&withdrawn, 0.95, false, &t),
            ClaimStanding::Defeated
        );
        assert_eq!(
            classify(&rejected, 0.95, false, &t),
            ClaimStanding::Defeated
        );
    }

    #[test]
    fn a_live_attacker_makes_it_disputed_even_above_the_agreed_bar() {
        let t = Thresholds::default();
        let c = claim(EvidenceKind::Fact, ClaimLifecycle::Proposed);
        assert_eq!(classify(&c, 0.90, true, &t), ClaimStanding::Disputed);
    }

    #[test]
    fn unverified_never_challenged_is_unresolved() {
        let t = Thresholds::default();
        let c = claim(EvidenceKind::Unverified, ClaimLifecycle::Proposed);
        assert_eq!(classify(&c, 0.40, false, &t), ClaimStanding::Unresolved);
    }

    #[test]
    fn high_standing_no_attacker_is_agreed() {
        let t = Thresholds::default();
        let c = claim(EvidenceKind::Fact, ClaimLifecycle::Defended);
        assert_eq!(classify(&c, 0.75, false, &t), ClaimStanding::Agreed);
    }

    /// D8 gap 1: Defended/Modified count as resolved; Proposed/Verified/Challenged
    /// do not, even for the same Unverified kind and the same standing.
    #[test]
    fn resolved_by_challenge_requires_defended_or_modified() {
        let t = Thresholds::default();
        let still_open = [
            ClaimLifecycle::Proposed,
            ClaimLifecycle::Verified,
            ClaimLifecycle::Challenged,
        ];
        for lc in still_open {
            let c = claim(EvidenceKind::Unverified, lc);
            assert_eq!(
                classify(&c, 0.40, false, &t),
                ClaimStanding::Unresolved,
                "{lc:?} must still read Unresolved"
            );
        }
        let resolved = [
            ClaimLifecycle::Defended,
            ClaimLifecycle::Modified { version: 2 },
        ];
        for lc in resolved {
            let c = claim(EvidenceKind::Unverified, lc);
            // Standing 0.40 is below Agreed (0.50) and not below Defeated (0.15):
            // once no longer "unresolved", it falls to the D8 gap-2 residual band.
            assert_eq!(
                classify(&c, 0.40, false, &t),
                ClaimStanding::Unresolved,
                "{lc:?} exits the Unverified rule, \
                but still lands in the gap-2 residual band, not Agreed"
            );
        }
    }

    /// D8 gap 2: the residual band — no live attacker, not Unverified, standing
    /// short of Agreed — classifies as Unresolved rather than silently Agreed.
    #[test]
    fn the_residual_band_falls_to_unresolved_not_agreed_or_disputed() {
        let t = Thresholds::default();
        let c = claim(EvidenceKind::Assumption, ClaimLifecycle::Proposed);
        assert!(
            t.defeated <= 0.35 && 0.35 < t.agreed,
            "test standing must sit in the gap band"
        );
        assert_eq!(classify(&c, 0.35, false, &t), ClaimStanding::Unresolved);
    }

    #[test]
    fn has_live_attacker_checks_the_attackers_own_standing_not_the_edges_confidence() {
        let t = Thresholds::default();
        let mut standings = BTreeMap::new();
        standings.insert(ClaimId::new("weak-attacker"), 0.10); // below live_attacker (0.30)
        standings.insert(ClaimId::new("strong-attacker"), 0.50);
        let relations = vec![Relation {
            from: ClaimId::new("weak-attacker"),
            to: ClaimId::new("C-1"),
            kind: RelationKind::Contradicts,
            confidence: 1.0,
        }];
        assert!(
            !has_live_attacker(&ClaimId::new("C-1"), &standings, &relations, &t),
            "a weak attacker's edge confidence being 1.0 must not matter — its own standing is what counts"
        );

        let relations2 = vec![Relation {
            from: ClaimId::new("strong-attacker"),
            to: ClaimId::new("C-1"),
            kind: RelationKind::Contradicts,
            confidence: 1.0,
        }];
        assert!(has_live_attacker(
            &ClaimId::new("C-1"),
            &standings,
            &relations2,
            &t
        ));
    }

    #[test]
    fn classify_all_matches_per_claim_classify() {
        let t = Thresholds::default();
        let c1 = CanonicalClaim {
            id: ClaimId::new("C-1"),
            ..claim(EvidenceKind::Fact, ClaimLifecycle::Defended)
        };
        let c2 = CanonicalClaim {
            id: ClaimId::new("C-2"),
            ..claim(EvidenceKind::Fact, ClaimLifecycle::Proposed)
        };
        let claims = vec![c1, c2];
        let mut standings = BTreeMap::new();
        standings.insert(ClaimId::new("C-1"), 0.80);
        standings.insert(ClaimId::new("C-2"), 0.60);
        let relations = vec![Relation {
            from: ClaimId::new("C-1"),
            to: ClaimId::new("C-2"),
            kind: RelationKind::Contradicts,
            confidence: 1.0,
        }];
        let result = classify_all(&claims, &standings, &relations, &t);
        assert_eq!(result[&ClaimId::new("C-1")], ClaimStanding::Agreed);
        assert_eq!(result[&ClaimId::new("C-2")], ClaimStanding::Disputed);
    }
}
