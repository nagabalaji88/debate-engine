//! Every threshold and weight the decision core uses. Nothing is hard-coded in the
//! algorithms; a policy plugin swaps this struct wholesale.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DecisionConfig {
    pub weights: Weights,
    pub graph: GraphParams,
    pub thresholds: Thresholds,
    pub confidence: ConfidenceWeights,
    pub attachment: AttachmentParams,
}

/// INTERFACES §20 Step 3 — deterministic attachment propagation. Kept separate
/// from `GraphParams`, which is scoped to the §6.3 fixpoint specifically: this
/// walks the same relation edges but for a different purpose (which option a
/// claim supports), not claim standing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AttachmentParams {
    /// How many relation-hops propagation walks outward from a directly-attached
    /// claim before stopping.
    pub propagation_depth: u32,
}

impl Default for AttachmentParams {
    fn default() -> Self {
        Self {
            propagation_depth: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Weights {
    pub kind_fact: f64,
    pub kind_inference: f64,
    pub kind_assumption: f64,
    pub kind_opinion: f64,
    pub kind_unverified: f64,
    /// Survival through cross-examination.
    pub survival_defended: f64,
    pub survival_unchallenged: f64,
    pub survival_pending: f64,
    pub survival_modified: f64,
    /// Members from the same vendor beyond the first count this much.
    pub correlated_member: f64,
    /// Floor of the judge factor when a scorecard exists.
    pub judge_floor: f64,
}

/// Fixpoint constants, ARCHITECTURE §6.3. Field names are English; the doc comments
/// carry the Greek letters the spec tables use, so the two stay cross-referenceable
/// without forcing every reader to memorise a symbol-to-identifier table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphParams {
    /// α — support weight.
    pub support_gain: f64,
    /// β — attack weight.
    pub attack_gain: f64,
    /// γ — qualify weight.
    pub qualify_gain: f64,
    /// λ — damping factor between iterations.
    pub damping: f64,
    /// Ten weak attackers must not outweigh one strong refutation: total attack
    /// pressure on a claim saturates here before it is applied.
    pub attack_cap: f64,
    /// The support-side saturation cap, so a claim cannot be inflated past
    /// usefulness by piling on corroboration either.
    pub support_cap: f64,
    pub max_iterations: u32,
    pub epsilon: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Thresholds {
    /// Below this, a claim is Defeated.
    pub defeated: f64,
    /// An attacker at or above this is a live attacker.
    pub live_attacker: f64,
    /// At or above this, with no live attacker, a claim is Agreed.
    pub agreed: f64,
    /// Below this evidence mass the debate cannot conclude.
    pub min_evidence_mass: f64,
    /// Above this share of unresolved-critical claims the debate cannot conclude.
    pub max_unresolved_ratio: f64,
    /// τ_gap. Below this margin between the top two options, the decision is split
    /// (§6.6). Also multiplied by `converged_margin_factor` — a kernel-owned
    /// constant (IMPLEMENTATION_PLAN.md §0.6) — for the controller's Converged
    /// stop predicate (§5.5); this crate does not compute that predicate itself.
    pub min_margin: f64,
    /// τ_dissent. Surviving contradiction at or above this means dissent stands,
    /// which is what keeps an outcome from being CONSENSUS (§6.6).
    pub dissent: f64,
    /// Required in outcome-classification rules 1–3 (§6.6). Without it,
    /// `score(A)=0.11, score(B)=0.08` reads as a split between two options neither
    /// of which is evidenced — that is INSUFFICIENT_EVIDENCE, not SPLIT_DECISION.
    pub option_floor: f64,
    /// Multiplies `min_evidence_mass` in outcome-classification rule 1 only, when
    /// the run is `Completeness::Truncated` (§6.6, INTERFACES §9) — a half-finished
    /// debate must clear a higher evidence bar before it is trusted to conclude.
    /// Does **not** apply to rule 3's evidence check, which reads `min_evidence_mass`
    /// alone per §6.6's literal text (PLAN_DEVIATIONS.md D12).
    pub truncation_factor: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfidenceWeights {
    pub evidence_mass: f64,
    pub margin: f64,
    pub judge: f64,
    pub unresolved_penalty: f64,
    pub assumption_penalty: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            kind_fact: 1.00,
            kind_inference: 0.75,
            kind_assumption: 0.50,
            kind_opinion: 0.35,
            kind_unverified: 0.15,
            survival_defended: 1.00,
            survival_unchallenged: 1.00,
            survival_pending: 0.90,
            survival_modified: 0.70,
            correlated_member: 0.25,
            judge_floor: 0.60,
        }
    }
}

impl Default for GraphParams {
    fn default() -> Self {
        Self {
            support_gain: 0.25,
            // Attacks bite harder than support: a refuted claim should fall faster
            // than an corroborated one rises.
            attack_gain: 0.60,
            qualify_gain: 0.15,
            damping: 0.50,
            attack_cap: 1.5,
            support_cap: 2.0,
            max_iterations: 64,
            epsilon: 1e-9,
        }
    }
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            defeated: 0.15,
            live_attacker: 0.30,
            agreed: 0.50,
            min_evidence_mass: 0.35,
            max_unresolved_ratio: 0.40,
            min_margin: 0.15,
            dissent: 0.30,
            option_floor: 0.20,
            truncation_factor: 1.2,
        }
    }
}

impl Default for ConfidenceWeights {
    fn default() -> Self {
        Self {
            evidence_mass: 0.35,
            margin: 0.30,
            judge: 0.35,
            unresolved_penalty: 0.25,
            assumption_penalty: 0.15,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturation_caps_match_the_spec() {
        let g = GraphParams::default();
        assert_eq!(g.attack_cap, 1.5);
        assert_eq!(g.support_cap, 2.0);
    }

    #[test]
    fn option_floor_matches_the_spec() {
        assert_eq!(Thresholds::default().option_floor, 0.20);
    }

    #[test]
    fn truncation_factor_matches_the_spec() {
        assert_eq!(Thresholds::default().truncation_factor, 1.2);
    }

    #[test]
    fn confidence_dimension_weights_sum_to_one_within_tolerance() {
        // Not `== 1.0`: 0.35 + 0.30 + 0.35 is 0.9999999999999999 in f64.
        let c = ConfidenceWeights::default();
        let sum = c.evidence_mass + c.margin + c.judge;
        assert!(
            (sum - 1.0).abs() < 1e-9,
            "sum was {sum}, not within 1e-9 of 1.0"
        );
    }
}
