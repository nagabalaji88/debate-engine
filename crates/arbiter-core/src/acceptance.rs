//! `DecisionAcceptance`/`DecisionOverride`, INTERFACES §17: "a debate
//! concludes; a human decides whether to act on it, and may act on a
//! modified version." Recorded, not computed — unlike the rest of this
//! crate, nothing here derives from anything; it is what `arbiter accept`
//! (L4) hands back to be persisted.
//!
//! PLAN_DEVIATIONS.md D45: `DecisionOverride::from` is `Value::Null` in
//! every override this codebase can produce today. INTERFACES §17 frames a
//! override as changing one field of a *generated Build Studio spec*
//! (`path: "technical.cloud_provider"` is a Build Studio document path, not
//! a `DecisionRecord` field) — Build Studio does not exist yet (ARCHITECTURE
//! §13, explicitly out of scope), so there is no baseline document to read
//! the prior value from. `to`/`reason`/`path` all still carry their real,
//! user-supplied meaning.

use crate::ids::OverrideId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionOverride {
    pub id: OverrideId,
    /// A dotted field path into whatever document this override applies to,
    /// e.g. `"technical.cloud_provider"` (INTERFACES §17's own example) —
    /// no concrete `FieldPath` type exists anywhere in this workspace, so
    /// this holds its canonical dotted-string form, the same precedent
    /// `CacheKey::params` already set for an unspecified concrete type.
    pub path: String,
    pub from: serde_json::Value,
    pub to: serde_json::Value,
    /// Required — INTERFACES §17: "an unexplained override is rejected."
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionAcceptance {
    pub accepted_by: String,
    pub accepted_at: String,
    pub overrides: Vec<DecisionOverride>,
}
