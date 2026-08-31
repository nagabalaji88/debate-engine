//! Relationships between claims. The argument graph is built from these.

use crate::ids::ClaimId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Supports,
    Contradicts,
    /// Adds a condition — a partial attack on unconditional standing.
    Qualifies,
    Unrelated,
    /// Recorded, but carries no weight: the classifier declined to commit.
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    pub from: ClaimId,
    pub to: ClaimId,
    pub kind: RelationKind,
    /// Classifier confidence in [0,1].
    pub confidence: f64,
}
