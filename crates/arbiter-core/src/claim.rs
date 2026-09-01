//! Claims: the unit of analysis. Model votes decide nothing; these do.

use crate::ids::{ClaimId, GroupId, ModelId, PositionId, ProviderId};
use serde::{Deserialize, Serialize};

/// How a claim is evidenced. Closed set: adding a variant breaks every match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Fact,
    Inference,
    Assumption,
    Opinion,
    Unverified,
}

/// A verbatim region of the position text a claim was extracted from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextSpan {
    pub start: usize,
    pub end: usize,
    pub quote: String,
}

/// How a claim connects back to what the model actually wrote.
///
/// `Unsupported` is admitted rather than rejected: unevidenced-but-real risk is
/// exactly what dissent is made of. It enters at `EvidenceKind::Unverified` weight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Grounding {
    DirectQuote { span: TextSpan },
    Derived { premises: Vec<ClaimId> },
    Unsupported,
}

/// What has happened to a claim. Orthogonal to [`ClaimStanding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ClaimLifecycle {
    Proposed,
    Verified,
    Challenged,
    Defended,
    Modified { version: u32 },
    Withdrawn,
    Rejected,
}

/// Where a claim stands in the argument graph. Computed, never authored by a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStanding {
    Agreed,
    Disputed,
    Unresolved,
    Defeated,
}

/// One model's original wording of a canonical claim. Never discarded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimMember {
    pub claim_id: ClaimId,
    pub model: ModelId,
    pub provider: ProviderId,
    /// Which correlated-prior group this member belongs to, for the `independence`
    /// term (ARCHITECTURE §6.2, INTERFACES §15). Defaults to `provider` — construct
    /// with [`ClaimMember::new`] to get that default, or set it explicitly when a
    /// correlation table says two providers share underlying weights.
    pub correlation_group: GroupId,
    pub position: PositionId,
    pub original_text: String,
    pub grounding: Grounding,
}

impl ClaimMember {
    /// `correlation_group` defaults to `provider`, which is the spec's stated default
    /// (INTERFACES §15) when no correlation table overrides it.
    pub fn new(
        claim_id: ClaimId,
        model: ModelId,
        provider: ProviderId,
        position: PositionId,
        original_text: impl Into<String>,
        grounding: Grounding,
    ) -> Self {
        let correlation_group = GroupId::new(provider.as_str());
        Self {
            claim_id,
            model,
            provider,
            correlation_group,
            position,
            original_text: original_text.into(),
            grounding,
        }
    }
}

/// A cluster of equivalent claims across models, with every original preserved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalClaim {
    pub id: ClaimId,
    pub text: String,
    pub kind: EvidenceKind,
    pub lifecycle: ClaimLifecycle,
    pub members: Vec<ClaimMember>,
}

impl CanonicalClaim {
    /// Distinct vendors behind this claim. Used by `corroboration` (§6.2 defines that
    /// term over providers specifically, unlike `independence` below).
    pub fn distinct_providers(&self) -> usize {
        let mut v: Vec<&ProviderId> = self.members.iter().map(|m| &m.provider).collect();
        v.sort();
        v.dedup();
        v.len()
    }

    /// Distinct correlation groups behind this claim — what `independence` (§6.2,
    /// INTERFACES §15) actually partitions on. Not the same count as
    /// `distinct_providers` once a correlation table groups two providers together.
    pub fn correlation_groups(&self) -> usize {
        let mut v: Vec<&GroupId> = self.members.iter().map(|m| &m.correlation_group).collect();
        v.sort();
        v.dedup();
        v.len()
    }

    pub fn asserted_by(&self) -> Vec<ModelId> {
        let mut v: Vec<ModelId> = self.members.iter().map(|m| m.model.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    pub fn is_live(&self) -> bool {
        !matches!(
            self.lifecycle,
            ClaimLifecycle::Withdrawn | ClaimLifecycle::Rejected
        )
    }
}
