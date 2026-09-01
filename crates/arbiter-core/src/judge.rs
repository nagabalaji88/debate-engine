//! The 9-metric debate rubric. The judge scores arguments, never model identity,
//! and its verdict is one term of confidence — not the decision.

use crate::ids::ModelId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scorecard {
    pub model: ModelId,
    pub factual_correctness: f64,
    pub logical_reasoning: f64,
    pub evidence_quality: f64,
    pub problem_relevance: f64,
    pub assumption_quality: f64,
    pub counterargument_handling: f64,
    pub risk_awareness: f64,
    pub practicality: f64,
    pub clarity: f64,
}

impl Scorecard {
    /// Rubric weights, fixed by the spec: 15/15/10/10/10/15/10/10/5.
    pub fn weighted(&self) -> f64 {
        let s = 0.15 * self.factual_correctness
            + 0.15 * self.logical_reasoning
            + 0.10 * self.evidence_quality
            + 0.10 * self.problem_relevance
            + 0.10 * self.assumption_quality
            + 0.15 * self.counterargument_handling
            + 0.10 * self.risk_awareness
            + 0.10 * self.practicality
            + 0.05 * self.clarity;
        s.clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(v: f64) -> Scorecard {
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

    #[test]
    fn rubric_weights_sum_to_one() {
        assert!((flat(1.0).weighted() - 1.0).abs() < 1e-12);
        assert_eq!(flat(0.0).weighted(), 0.0);
    }
}
