//! Arbiter core — domain model and deterministic decision engine.
//!
//! Nothing in this crate performs IO, awaits, or calls a model. The kernel feeds it
//! recorded artifacts; it returns a decision that is a pure function of them.
//!
//! `arbiter-core`'s v2.9 scope (IMPLEMENTATION_PLAN.md tasks X1–C8) is complete:
//! everything from claim ingestion through the `explain --json` payload is a pure
//! function of recorded artifacts. `arbiter-store`, `arbiter-kernel` and the rest
//! of the workspace consume this crate; it consumes nothing internal.
#![forbid(unsafe_code)]

pub mod acceptance;
pub mod claim;
pub mod config;
pub mod decision;
pub mod ids;
pub mod judge;
pub mod option;
pub mod policy;
pub mod relation;

pub use acceptance::{DecisionAcceptance, DecisionOverride};
pub use claim::{
    CanonicalClaim, ClaimLifecycle, ClaimMember, ClaimStanding, EvidenceKind, Grounding, TextSpan,
};
pub use config::DecisionConfig;
pub use decision::attachment::{AttachSource, Attachment, AttachmentMatrix, Polarity};
pub use decision::confidence::{ConfidenceBreakdown, PenaltyInputs};
pub use decision::explain::{DefeatChain, DefeatStep, defeat_chain_for};
pub use decision::fixpoint::FixpointResult;
pub use decision::outcome::{Outcome, OutcomeInputs};
pub use decision::record::{
    ChangeTriggerEntry, ClaimCounts, ConfidenceExplain, DecisionRecord, DimensionEntry,
    PenaltyEntry, Recommendation, SCHEMA_VERSION, explain_confidence,
};
pub use decision::triggers::{CounterfactualFlip, FlipDirection};
pub use ids::{
    ClaimId, GroupId, ModelId, OptionId, OptionVersion, OverrideId, PolicyVersion, PositionId,
    ProviderId, RunId,
};
pub use judge::Scorecard;
pub use option::{DecisionOption, OptionScore};
pub use policy::Policy;
pub use relation::{Relation, RelationKind};
