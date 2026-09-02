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
pub mod triggers;

use crate::option::OptionScore;

/// Ranks options by `share` descending, `OptionId` ascending as a deterministic
/// tie-break. Shared by every place a "top1"/"top2" concept is needed — outcome
/// classification, confidence's `decision_margin`, and change-trigger detection —
/// so the three can never disagree about who is winning.
pub(crate) fn rank_by_share(scores: &[OptionScore]) -> Vec<&OptionScore> {
    let mut ranked: Vec<&OptionScore> = scores.iter().collect();
    ranked.sort_by(|a, b| {
        b.share
            .partial_cmp(&a.share)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    ranked
}
