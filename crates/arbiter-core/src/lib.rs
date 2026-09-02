//! Arbiter core — domain model and deterministic decision engine.
//!
//! Nothing in this crate performs IO, awaits, or calls a model. The kernel feeds it
//! recorded artifacts; it returns a decision that is a pure function of them.
//!
//! Work in progress: `DecisionRecord` and the `explain --json` payload land next, on
//! top of these types (IMPLEMENTATION_PLAN.md task C8).
#![forbid(unsafe_code)]

pub mod claim;
pub mod config;
pub mod decision;
pub mod ids;
pub mod judge;
pub mod option;
pub mod policy;
pub mod relation;

pub use claim::{
    CanonicalClaim, ClaimLifecycle, ClaimMember, ClaimStanding, EvidenceKind, Grounding, TextSpan,
};
pub use config::DecisionConfig;
pub use decision::attachment::{AttachSource, Attachment, AttachmentMatrix, Polarity};
pub use decision::confidence::{ConfidenceBreakdown, PenaltyInputs};
pub use decision::fixpoint::FixpointResult;
pub use decision::outcome::{Outcome, OutcomeInputs};
pub use decision::triggers::{CounterfactualFlip, FlipDirection};
pub use ids::{
    ClaimId, GroupId, ModelId, OptionId, OptionVersion, PolicyVersion, PositionId, ProviderId,
    RunId,
};
pub use judge::Scorecard;
pub use option::{DecisionOption, OptionScore};
pub use policy::Policy;
pub use relation::{Relation, RelationKind};
