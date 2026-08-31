//! Every threshold and weight the decision core uses. Nothing is hard-coded in the
//! algorithms; a policy plugin swaps this struct wholesale.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DecisionConfig {
    pub weights: Weights,
    pub graph: GraphParams,
    pub thresholds: Thresholds,
    pub confidence: ConfidenceWeights,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphParams {
    pub support_gain: f64,
    pub attack_gain: f64,
    pub qualify_gain: f64,
    pub damping: f64,
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
    /// Below this margin between the top two options, the decision is split.
    pub min_margin: f64,
    /// Surviving contradiction at or above this means dissent stands.
    pub dissent: f64,
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

impl Default for DecisionConfig {
    fn default() -> Self {
        Self {
            weights: Weights::default(),
            graph: GraphParams::default(),
            thresholds: Thresholds::default(),
            confidence: ConfidenceWeights::default(),
        }
    }
}
