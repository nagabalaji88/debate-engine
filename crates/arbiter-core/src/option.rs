//! Candidate recommendations. Scored from claim standing via the [`AttachmentMatrix`]
//! (`decision::attachment`), never from vote share.

use crate::ids::{OptionId, OptionVersion};
use serde::{Deserialize, Serialize};

/// A recommendation cluster. `id` is stable across rewording (INTERFACES §20 Step
/// 3b) — hashing text into the id was the v2.3 formulation, and it minted a new
/// option on every refinement, orphaning attachment mid-debate.
///
/// Does **not** carry `supported_by`/`opposed_by` — that was the v2.0 shape, and it
/// duplicated what the [`AttachmentMatrix`](crate::decision::attachment::AttachmentMatrix)
/// already records more richly (polarity, confidence, and *why* a cell exists).
/// Scoring reads the matrix directly; this struct is identity and lineage only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionOption {
    pub id: OptionId,
    pub label: String,
    /// `blake3` of the current canonical text. Changes on every reword; `id` does not.
    pub version: OptionVersion,
    /// Set when this option replaced an earlier one after a *material* change in
    /// course of action (not mere rewording, which keeps the same `id`).
    pub supersedes: Option<(OptionId, OptionVersion)>,
    /// Superseded versions and abandoned options are kept, never deleted — but
    /// excluded from scoring. `score_options` skips every retired option.
    pub retired: bool,
}

impl DecisionOption {
    /// A fresh option: not superseding anything, not retired.
    pub fn new(id: OptionId, label: impl Into<String>) -> Self {
        let label = label.into();
        let version = OptionVersion::of(&label);
        Self {
            id,
            label,
            version,
            supersedes: None,
            retired: false,
        }
    }

    /// The same cluster, reworded. Same `id`, new `version` — attachment cells
    /// carry over untouched (INTERFACES §20's round-2 event table).
    pub fn reworded(&self, new_label: impl Into<String>) -> Self {
        let label = new_label.into();
        let version = OptionVersion::of(&label);
        Self {
            id: self.id.clone(),
            label,
            version,
            supersedes: self.supersedes.clone(),
            retired: false,
        }
    }

    /// A materially different course of action superseding this one: new `id`,
    /// `supersedes` set to this option's (id, version).
    pub fn superseding(&self, new_id: OptionId, new_label: impl Into<String>) -> Self {
        let label = new_label.into();
        let version = OptionVersion::of(&label);
        Self {
            id: new_id,
            label,
            version,
            supersedes: Some((self.id.clone(), self.version.clone())),
            retired: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionScore {
    pub id: OptionId,
    pub label: String,
    /// `Σ standing(supporting) − 0.5 · Σ standing(opposing)`, clamped at 0 before
    /// normalisation (PLAN_DEVIATIONS.md D10) — a net-opposed option contributing
    /// negative mass to the others' shares has no dialectical meaning.
    pub raw: f64,
    /// `raw` normalised across options. Sums to 1.0 across all scored options
    /// **only when at least one has positive `raw`**; when every option's clamped
    /// `raw` is 0, every share is 0 — not an even split, which would manufacture
    /// confidence that does not exist, and not `NaN` from dividing by zero.
    pub share: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reword_keeps_id_and_mints_a_new_version() {
        let a = DecisionOption::new(OptionId::new("opt_modular"), "Modular monolith");
        let b = a.reworded("Modular monolith, with enforced boundaries");
        assert_eq!(a.id, b.id);
        assert_ne!(a.version, b.version);
        assert!(!b.retired);
        assert_eq!(b.supersedes, None);
    }

    #[test]
    fn material_change_mints_a_new_id_and_sets_supersedes() {
        let a = DecisionOption::new(OptionId::new("opt_modular"), "Modular monolith");
        let b = a.superseding(OptionId::new("opt_micro"), "Full microservices");
        assert_ne!(a.id, b.id);
        assert_eq!(b.supersedes, Some((a.id.clone(), a.version.clone())));
    }
}
