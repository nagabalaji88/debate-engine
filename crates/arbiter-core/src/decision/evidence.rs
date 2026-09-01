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

/// `(groups + λ·(members − groups)) / members` — INTERFACES §15, exactly. Partitioned
/// on `correlation_group`, never on provider identity directly: provider is only that
/// group's *default*, and defaulting it unconditionally is what the spec calls "an
/// optimistic proxy" (§6.2) — two vendors serving the same base weights are correlated
/// and a correlation table can say so without this function changing.
pub fn independence(claim: &CanonicalClaim, w: &Weights) -> f64 {
    let members = claim.members.len();
    if members == 0 {
        return 0.0;
    }
    let groups = claim.correlation_groups();
    let correlated = members - groups;
    ((groups as f64 + w.correlated_member * correlated as f64) / members as f64).clamp(0.0, 1.0)
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
pub fn judge_factor(
    claim: &CanonicalClaim,
    scores: &BTreeMap<ModelId, Scorecard>,
    w: &Weights,
) -> f64 {
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
        && claim
            .members
            .iter()
            .all(|m| matches!(m.grounding, Grounding::Unsupported));
    if ungrounded {
        EvidenceKind::Unverified
    } else {
        claim.kind
    }
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
    claims
        .iter()
        .map(|c| (c.id.clone(), evidence(c, scores, w)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::{ClaimMember, TextSpan};
    use crate::ids::{GroupId, PositionId, ProviderId};

    fn member(model: &str, provider: &str, grounding: Grounding) -> ClaimMember {
        ClaimMember::new(
            ClaimId::new(format!("{model}-raw")),
            ModelId::new(model),
            ProviderId::new(provider),
            PositionId::new(format!("pos-{model}")),
            "on-call load rises with service count",
            grounding,
        )
    }

    fn quoted() -> Grounding {
        Grounding::DirectQuote {
            span: TextSpan {
                start: 0,
                end: 36,
                quote: "on-call load rises with service count".into(),
            },
        }
    }

    fn claim(
        kind: EvidenceKind,
        lifecycle: ClaimLifecycle,
        members: Vec<ClaimMember>,
    ) -> CanonicalClaim {
        CanonicalClaim {
            id: ClaimId::new("C-001"),
            text: "t".into(),
            kind,
            lifecycle,
            members,
        }
    }

    #[test]
    fn withdrawn_claims_carry_no_evidence() {
        let c = claim(
            EvidenceKind::Fact,
            ClaimLifecycle::Withdrawn,
            vec![member("m1", "p1", quoted())],
        );
        assert_eq!(evidence(&c, &BTreeMap::new(), &Weights::default()), 0.0);
    }

    #[test]
    fn same_vendor_members_do_not_manufacture_independence() {
        let w = Weights::default();
        let correlated = claim(
            EvidenceKind::Fact,
            ClaimLifecycle::Defended,
            vec![
                member("a1", "vendor", quoted()),
                member("a2", "vendor", quoted()),
                member("a3", "vendor", quoted()),
            ],
        );
        let independent = claim(
            EvidenceKind::Fact,
            ClaimLifecycle::Defended,
            vec![
                member("a1", "v1", quoted()),
                member("b1", "v2", quoted()),
                member("c1", "v3", quoted()),
            ],
        );
        assert!(independence(&correlated, &w) < independence(&independent, &w));
        assert!(
            evidence(&correlated, &BTreeMap::new(), &w)
                < evidence(&independent, &BTreeMap::new(), &w)
        );
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
        assert!(
            e > 0.0,
            "unevidenced risk must survive to the decision, at low weight"
        );
        assert!(e < 0.2);
    }

    #[test]
    fn a_harsh_judge_discounts_but_cannot_erase() {
        let w = Weights::default();
        let c = claim(
            EvidenceKind::Fact,
            ClaimLifecycle::Defended,
            vec![member("m1", "p1", quoted())],
        );
        let mut scores = BTreeMap::new();
        scores.insert(
            ModelId::new("m1"),
            crate::judge::Scorecard {
                model: ModelId::new("m1"),
                factual_correctness: 0.0,
                logical_reasoning: 0.0,
                evidence_quality: 0.0,
                problem_relevance: 0.0,
                assumption_quality: 0.0,
                counterargument_handling: 0.0,
                risk_awareness: 0.0,
                practicality: 0.0,
                clarity: 0.0,
            },
        );
        assert_eq!(judge_factor(&c, &scores, &w), w.judge_floor);
        assert!(evidence(&c, &scores, &w) > 0.0);
    }

    #[test]
    fn survival_ranks_defended_above_modified_above_withdrawn() {
        let w = Weights::default();
        assert!(
            survival_weight(ClaimLifecycle::Defended, &w)
                > survival_weight(ClaimLifecycle::Modified { version: 2 }, &w)
        );
        assert!(
            survival_weight(ClaimLifecycle::Modified { version: 2 }, &w)
                > survival_weight(ClaimLifecycle::Withdrawn, &w)
        );
    }

    /// INTERFACES §15's worked examples, pinned exactly.
    #[test]
    fn independence_matches_the_spec_worked_examples() {
        let w = Weights::default();
        assert_eq!(w.correlated_member, 0.25, "λ in the spec's formula");

        // members=4, providers {OpenAI×2, Anthropic×1, Google×1}, groups=3
        // -> (3 + 0.25*1) / 4 = 0.8125
        let c = claim(
            EvidenceKind::Fact,
            ClaimLifecycle::Defended,
            vec![
                member("m1", "openai", quoted()),
                member("m2", "openai", quoted()),
                member("m3", "anthropic", quoted()),
                member("m4", "google", quoted()),
            ],
        );
        assert!((independence(&c, &w) - 0.8125).abs() < 1e-9);

        // members=1 -> 1.0
        let c = claim(
            EvidenceKind::Fact,
            ClaimLifecycle::Defended,
            vec![member("m1", "openai", quoted())],
        );
        assert!((independence(&c, &w) - 1.0).abs() < 1e-9);

        // members=3, all one provider -> (1 + 0.25*2) / 3 = 0.50
        let c = claim(
            EvidenceKind::Fact,
            ClaimLifecycle::Defended,
            vec![
                member("m1", "openai", quoted()),
                member("m2", "openai", quoted()),
                member("m3", "openai", quoted()),
            ],
        );
        assert!((independence(&c, &w) - 0.50).abs() < 1e-9);
    }

    /// The reason `correlation_group` exists as its own field rather than being
    /// computed from `provider` inline: an operator's correlation table can say two
    /// providers are correlated, and `independence` must follow the table, not the
    /// providers. This is D6 in PLAN_DEVIATIONS.md.
    #[test]
    fn independence_follows_correlation_group_even_when_providers_differ() {
        let w = Weights::default();

        let mut m1 = member("m1", "vendor-a", quoted());
        let mut m2 = member("m2", "vendor-b", quoted());
        // Two distinct providers, but a correlation table says they share weights:
        // both go in one group.
        m1.correlation_group = GroupId::new("shared-base-model");
        m2.correlation_group = GroupId::new("shared-base-model");
        let grouped = claim(EvidenceKind::Fact, ClaimLifecycle::Defended, vec![m1, m2]);

        // Same two providers, no override: two distinct default groups.
        let ungrouped = claim(
            EvidenceKind::Fact,
            ClaimLifecycle::Defended,
            vec![
                member("m1", "vendor-a", quoted()),
                member("m2", "vendor-b", quoted()),
            ],
        );

        assert_eq!(
            grouped.distinct_providers(),
            ungrouped.distinct_providers(),
            "providers unchanged"
        );
        assert!(
            independence(&grouped, &w) < independence(&ungrouped, &w),
            "the correlation table must be able to lower independence even though \
             distinct_providers() alone would say nothing changed"
        );
    }
}
