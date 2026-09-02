//! The decision engine: pure functions over recorded artifacts.
//!
//! No IO, no async, no LLM. Every value here is reproducible from the same inputs,
//! which is what makes golden-fixture testing possible without spending a token.

pub mod attachment;
pub mod confidence;
pub mod evidence;
pub mod fixpoint;
pub mod outcome;
pub mod standing;
