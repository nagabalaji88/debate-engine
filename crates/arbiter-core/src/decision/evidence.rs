//! Evidence strength E(c) ∈ [0,1]. Pure arithmetic over recorded facts.
//!
//! ```text
//! E(c) = kind_weight × survival × independence × corroboration × judge_factor
//! ```

use crate::claim::{CanonicalClaim, ClaimLifecycle, EvidenceKind, Grounding};
use crate::config::Weights;
use crate::ids::{ClaimId, ModelId};
use crate::judge::Scorecard;
use std::collections::BTreeMap;

pub fn kind_weight(kind: EvidenceKind, w: &Weights) -> f64 {
    match kind {
        EvidenceKind::Fact => w.kind_fact,
        EvidenceKind::Inference => w.kind_inference,
        EvidenceKind::Assumption => w.kind_assumption,
        EvidenceKind::Opinion => w.kind_opinion,
        EvidenceKind::Unverified => w.kind_unverified,
    }
}

pub fn survival_weight(lifecycle: ClaimLifecycle, w: &Weights) -> f64 {
    match lifecycle {
        ClaimLifecycle::Defended => w.survival_defended,
        ClaimLifecycle::Proposed | ClaimLifecycle::Verified => w.survival_unchallenged,
        ClaimLifecycle::Challenged => w.survival_pending,
        ClaimLifecycle::Modified { .. } => w.survival_modified,
        ClaimLifecycle::Withdrawn | ClaimLifecycle::Rejected => 0.0,
    }
}

/// Members from one vendor are correlated, not independent. Four Anthropic models
/// agreeing is closer to one observation than to four.
pub fn independence(claim: &CanonicalClaim, w: &Weights) -> f64 {
    let members = claim.members.len();
    if members == 0 {
        return 0.0;
    }
    let distinct = claim.distinct_providers();
    let correlated = members - distinct;
    ((distinct as f64 + w.correlated_member * correlated as f64) / members as f64).clamp(0.0, 1.0)
}

/// Independent corroboration raises strength, with diminishing returns.
pub fn corroboration(claim: &CanonicalClaim) -> f64 {
    match claim.distinct_providers() {
        0 => 0.0,
        1 => 0.85,
        _ => 1.0,
    }
}

/// Claims whose authors argued poorly on evidence are discounted — bounded below so
/// a harsh judge cannot erase a claim outright.
pub fn judge_factor(claim: &CanonicalClaim, scores: &BTreeMap<ModelId, Scorecard>, w: &Weights) -> f64 {
    let mut acc = 0.0;
    let mut n = 0.0;
    for m in &claim.members {
        if let Some(sc) = scores.get(&m.model) {
            acc += sc.evidence_quality.clamp(0.0, 1.0);
            n += 1.0;
        }
    }
    if n == 0.0 {
        return 1.0;
    }
    let mean = acc / n;
    w.judge_floor + (1.0 - w.judge_floor) * mean
}

/// An ungrounded claim is admitted at Unverified weight rather than dropped.
pub fn effective_kind(claim: &CanonicalClaim) -> EvidenceKind {
    let ungrounded = !claim.members.is_empty()
        && claim.members.iter().all(|m| matches!(m.grounding, Grounding::Unsupported));
    if ungrounded { EvidenceKind::Unverified } else { claim.kind }
}

pub fn evidence(claim: &CanonicalClaim, scores: &BTreeMap<ModelId, Scorecard>, w: &Weights) -> f64 {
    if !claim.is_live() {
        return 0.0;
    }
    let e = kind_weight(effective_kind(claim), w)
        * survival_weight(claim.lifecycle, w)
        * independence(claim, w)
        * corroboration(claim)
        * judge_factor(claim, scores, w);
    e.clamp(0.0, 1.0)
}

pub fn evidence_map(
    claims: &[CanonicalClaim],
    scores: &BTreeMap<ModelId, Scorecard>,
    w: &Weights,
) -> BTreeMap<ClaimId, f64> {
    claims.iter().map(|c| (c.id.clone(), evidence(c, scores, w))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::{ClaimMember, TextSpan};
    use crate::ids::{PositionId, ProviderId};

    fn member(model: &str, provider: &str, grounding: Grounding) -> ClaimMember {
        ClaimMember {
            claim_id: ClaimId::new(format!("{model}-raw")),
            model: ModelId::new(model),
            provider: ProviderId::new(provider),
            position: PositionId::new(format!("pos-{model}")),
            original_text: "on-call load rises with service count".into(),
            grounding,
        }
    }

    fn quoted() -> Grounding {
        Grounding::DirectQuote {
            span: TextSpan { start: 0, end: 36, quote: "on-call load rises with service count".into() },
        }
    }

    fn claim(kind: EvidenceKind, lifecycle: ClaimLifecycle, members: Vec<ClaimMember>) -> CanonicalClaim {
        CanonicalClaim { id: ClaimId::new("C-001"), text: "t".into(), kind, lifecycle, members }
    }

    #[test]
    fn withdrawn_claims_carry_no_evidence() {
        let c = claim(EvidenceKind::Fact, ClaimLifecycle::Withdrawn, vec![member("m1", "p1", quoted())]);
        assert_eq!(evidence(&c, &BTreeMap::new(), &Weights::default()), 0.0);
    }

    #[test]
    fn same_vendor_members_do_not_manufacture_independence() {
        let w = Weights::default();
        let correlated = claim(
            EvidenceKind::Fact,
            ClaimLifecycle::Defended,
            vec![member("a1", "vendor", quoted()), member("a2", "vendor", quoted()), member("a3", "vendor", quoted())],
        );
        let independent = claim(
            EvidenceKind::Fact,
            ClaimLifecycle::Defended,
            vec![member("a1", "v1", quoted()), member("b1", "v2", quoted()), member("c1", "v3", quoted())],
        );
        assert!(independence(&correlated, &w) < independence(&independent, &w));
        assert!(evidence(&correlated, &BTreeMap::new(), &w) < evidence(&independent, &BTreeMap::new(), &w));
    }

    #[test]
    fn ungrounded_claims_are_admitted_at_unverified_weight_not_dropped() {
        let c = claim(
            EvidenceKind::Opinion,
            ClaimLifecycle::Defended,
            vec![member("m1", "p1", Grounding::Unsupported)],
        );
        assert_eq!(effective_kind(&c), EvidenceKind::Unverified);
        let e = evidence(&c, &BTreeMap::new(), &Weights::default());
        assert!(e > 0.0, "unevidenced risk must survive to the decision, at low weight");
        assert!(e < 0.2);
    }

    #[test]
    fn a_harsh_judge_discounts_but_cannot_erase() {
        let w = Weights::default();
        let c = claim(EvidenceKind::Fact, ClaimLifecycle::Defended, vec![member("m1", "p1", quoted())]);
        let mut scores = BTreeMap::new();
        scores.insert(ModelId::new("m1"), crate::judge::Scorecard {
            model: ModelId::new("m1"),
            factual_correctness: 0.0, logical_reasoning: 0.0, evidence_quality: 0.0,
            problem_relevance: 0.0, assumption_quality: 0.0, counterargument_handling: 0.0,
            risk_awareness: 0.0, practicality: 0.0, clarity: 0.0,
        });
        assert_eq!(judge_factor(&c, &scores, &w), w.judge_floor);
        assert!(evidence(&c, &scores, &w) > 0.0);
    }

    #[test]
    fn survival_ranks_defended_above_modified_above_withdrawn() {
        let w = Weights::default();
        assert!(survival_weight(ClaimLifecycle::Defended, &w) > survival_weight(ClaimLifecycle::Modified { version: 2 }, &w));
        assert!(survival_weight(ClaimLifecycle::Modified { version: 2 }, &w) > survival_weight(ClaimLifecycle::Withdrawn, &w));
    }
}
