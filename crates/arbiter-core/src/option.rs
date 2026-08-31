//! Candidate recommendations. Scored from claim standing, never from vote share.

use crate::ids::{ClaimId, OptionId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionOption {
    pub id: OptionId,
    pub label: String,
    pub supported_by: Vec<ClaimId>,
    pub opposed_by: Vec<ClaimId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionScore {
    pub id: OptionId,
    pub label: String,
    /// Support mass minus discounted opposition mass.
    pub raw: f64,
    /// `raw` normalised across options, in [0,1].
    pub share: f64,
}
