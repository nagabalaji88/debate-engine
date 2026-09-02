//! The `(claim × option)` attachment matrix and its deterministic propagation.
//! INTERFACES §20 Steps 2–3.
//!
//! Only Steps 3 (propagate) and scoring (§6.5) belong here. Steps 1–2 —
//! clustering recommendations and the batched classifier call that produces the
//! *direct* `Authored`/`Classified` cells — call an LLM and belong to the
//! kernel's `options.cluster` stage; this module only ever receives their output
//! and extends it deterministically. "No LLM" is Step 3's own description of
//! itself, and it is why this can live in a crate that forbids IO.

use crate::config::AttachmentParams;
use crate::ids::{ClaimId, OptionId};
use crate::option::{DecisionOption, OptionScore};
use crate::relation::{Relation, RelationKind};
use std::collections::BTreeMap;

/// How a claim bears on an option.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Polarity {
    Supports,
    Opposes,
    Neutral,
}

/// Where a cell came from — load-bearing, not decoration (PLAN_DEVIATIONS.md D9):
/// it is what makes Step 3's "the classifier only has to see direct attachment;
/// the graph does the rest" claim checkable at all.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AttachSource {
    /// The claim's own position recommended this option.
    Authored,
    /// The batched classifier call (Step 2) placed it here.
    Classified,
    /// Inferred deterministically from the relation graph (Step 3, this module).
    Propagated,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Attachment {
    pub polarity: Polarity,
    pub confidence: f64,
    pub source: AttachSource,
}

/// Recorded as its own artifact (INTERFACES §20). Keyed `(claim, option)` — a
/// `BTreeMap` so replay iterates cells in one deterministic order.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AttachmentMatrix {
    pub cells: BTreeMap<(ClaimId, OptionId), Attachment>,
}

impl AttachmentMatrix {
    pub fn get(&self, claim: &ClaimId, option: &OptionId) -> Option<&Attachment> {
        self.cells.get(&(claim.clone(), option.clone()))
    }
}

/// INTERFACES §20 Step 3, exactly the three stated rules — base case `s supports
/// O` only. PLAN_DEVIATIONS.md D11: no rule is given for propagating from an
/// `Opposes` cell, and none is invented here; extending it is a one-line addition
/// to this match once the spec states it.
///
/// `qualify_gain` is the same γ the fixpoint uses (§6.3) — the spec's "at γ weight"
/// reuses the constant by name, not a second one that happens to equal it.
///
/// Propagation never overwrites an existing cell (direct or already-propagated):
/// direct attachment from Steps 1–2 always wins, and a claim attached at an
/// earlier depth is not re-attached with a different confidence at a later one.
pub fn propagate(
    direct: &AttachmentMatrix,
    relations: &[Relation],
    params: &AttachmentParams,
    qualify_gain: f64,
) -> AttachmentMatrix {
    let mut result = direct.clone();

    for _depth in 0..params.propagation_depth {
        // Snapshot the frontier before this round so newly-added cells within the
        // same round don't chain within one depth step (chaining across rounds is
        // exactly what the depth cap governs).
        let frontier = result.cells.clone();
        let mut proposals: BTreeMap<(ClaimId, OptionId), Attachment> = BTreeMap::new();

        for r in relations {
            let (new_polarity, weight) = match r.kind {
                RelationKind::Contradicts => (Polarity::Opposes, 1.0),
                RelationKind::Supports => (Polarity::Supports, 1.0),
                RelationKind::Qualifies => (Polarity::Opposes, qualify_gain),
                RelationKind::Unrelated | RelationKind::Uncertain => continue,
            };
            // r: `r.from` acts on `r.to` (contradicts/supports/qualifies) — see
            // decision::fixpoint's doc comment for the same convention. The rule's
            // "s" is the already-attached claim (r.to); "c" is the candidate (r.from).
            for ((claim, option), cell) in &frontier {
                if claim != &r.to || cell.polarity != Polarity::Supports {
                    continue; // D11: only the `s supports O` base case is defined
                }
                let key = (r.from.clone(), option.clone());
                if result.cells.contains_key(&key) {
                    continue; // never overwrite an existing cell
                }
                let confidence = cell.confidence * r.confidence * weight;
                let candidate = Attachment {
                    polarity: new_polarity,
                    confidence,
                    source: AttachSource::Propagated,
                };
                proposals
                    .entry(key)
                    .and_modify(|existing| {
                        if candidate.confidence > existing.confidence {
                            *existing = candidate;
                        }
                    })
                    .or_insert(candidate);
            }
        }

        if proposals.is_empty() {
            break; // nothing new to propagate; further depth would be wasted work
        }
        result.cells.extend(proposals);
    }

    result
}

/// ARCHITECTURE §6.5. `raw = Σ standing(supporting) − 0.5 · Σ standing(opposing)`,
/// clamped at 0 and normalised (PLAN_DEVIATIONS.md D10). Retired options and
/// non-lineage-heads are the caller's concern — pass only the options that should
/// be scored; "scoring always runs over lineage heads" (INTERFACES §20).
///
/// Preserves `options`' input order; imposes no sort of its own.
pub fn score_options(
    options: &[DecisionOption],
    matrix: &AttachmentMatrix,
    standings: &BTreeMap<ClaimId, f64>,
) -> Vec<OptionScore> {
    let mut raws: Vec<(&DecisionOption, f64)> = options
        .iter()
        .map(|o| {
            let mut raw = 0.0;
            for ((claim, option), cell) in &matrix.cells {
                if option != &o.id {
                    continue;
                }
                let s = standings.get(claim).copied().unwrap_or(0.0);
                match cell.polarity {
                    Polarity::Supports => raw += s,
                    Polarity::Opposes => raw -= 0.5 * s,
                    Polarity::Neutral => {}
                }
            }
            (o, raw)
        })
        .collect();

    let clamped: Vec<f64> = raws.iter().map(|(_, raw)| raw.max(0.0)).collect();
    let total: f64 = clamped.iter().sum();

    raws.drain(..)
        .zip(clamped)
        .map(|((o, raw), clamped_raw)| {
            let share = if total > 0.0 {
                clamped_raw / total
            } else {
                0.0
            };
            OptionScore {
                id: o.id.clone(),
                label: o.label.clone(),
                raw,
                share,
            }
        })
        .collect()
}

/// Convenience for building a direct cell from a claim standing lookup, used by
/// callers assembling the Step 1–2 output before calling `propagate`. Not used
/// internally — `propagate`/`score_options` only ever read a matrix, never build
/// one from scratch, since Steps 1–2 (LLM calls) own that.
pub fn direct_cell(polarity: Polarity, confidence: f64, source: AttachSource) -> Attachment {
    Attachment {
        polarity,
        confidence,
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cell(p: Polarity, conf: f64, src: AttachSource) -> Attachment {
        Attachment {
            polarity: p,
            confidence: conf,
            source: src,
        }
    }

    fn rel(from: &str, to: &str, kind: RelationKind, confidence: f64) -> Relation {
        Relation {
            from: ClaimId::new(from),
            to: ClaimId::new(to),
            kind,
            confidence,
        }
    }

    fn params(depth: u32) -> AttachmentParams {
        AttachmentParams {
            propagation_depth: depth,
        }
    }

    #[test]
    fn contradicting_a_supporter_propagates_to_opposes() {
        let mut m = AttachmentMatrix::default();
        m.cells.insert(
            (ClaimId::new("s"), OptionId::new("O")),
            cell(Polarity::Supports, 0.8, AttachSource::Authored),
        );
        let relations = vec![rel("c", "s", RelationKind::Contradicts, 0.9)];
        let result = propagate(&m, &relations, &params(1), 0.15);
        let got = result.get(&ClaimId::new("c"), &OptionId::new("O")).unwrap();
        assert_eq!(got.polarity, Polarity::Opposes);
        assert!((got.confidence - 0.8 * 0.9).abs() < 1e-9);
        assert_eq!(got.source, AttachSource::Propagated);
    }

    #[test]
    fn supporting_a_supporter_propagates_to_supports() {
        let mut m = AttachmentMatrix::default();
        m.cells.insert(
            (ClaimId::new("s"), OptionId::new("O")),
            cell(Polarity::Supports, 0.8, AttachSource::Authored),
        );
        let relations = vec![rel("c", "s", RelationKind::Supports, 0.5)];
        let result = propagate(&m, &relations, &params(1), 0.15);
        let got = result.get(&ClaimId::new("c"), &OptionId::new("O")).unwrap();
        assert_eq!(got.polarity, Polarity::Supports);
        assert!((got.confidence - 0.8 * 0.5).abs() < 1e-9);
    }

    #[test]
    fn qualifying_a_supporter_propagates_to_opposes_at_gamma_weight() {
        let mut m = AttachmentMatrix::default();
        m.cells.insert(
            (ClaimId::new("s"), OptionId::new("O")),
            cell(Polarity::Supports, 1.0, AttachSource::Authored),
        );
        let relations = vec![rel("c", "s", RelationKind::Qualifies, 1.0)];
        let result = propagate(&m, &relations, &params(1), 0.15);
        let got = result.get(&ClaimId::new("c"), &OptionId::new("O")).unwrap();
        assert_eq!(got.polarity, Polarity::Opposes);
        assert!(
            (got.confidence - 0.15).abs() < 1e-9,
            "1.0 * 1.0 * gamma(0.15) = 0.15"
        );
    }

    /// D11: the spec gives no rule for `s opposes O`. Nothing must propagate from it.
    #[test]
    fn propagating_from_an_opposes_cell_produces_nothing() {
        let mut m = AttachmentMatrix::default();
        m.cells.insert(
            (ClaimId::new("s"), OptionId::new("O")),
            cell(Polarity::Opposes, 0.8, AttachSource::Authored),
        );
        let relations = vec![
            rel("c1", "s", RelationKind::Contradicts, 0.9),
            rel("c2", "s", RelationKind::Supports, 0.9),
            rel("c3", "s", RelationKind::Qualifies, 0.9),
        ];
        let result = propagate(&m, &relations, &params(2), 0.15);
        assert!(
            result
                .get(&ClaimId::new("c1"), &OptionId::new("O"))
                .is_none()
        );
        assert!(
            result
                .get(&ClaimId::new("c2"), &OptionId::new("O"))
                .is_none()
        );
        assert!(
            result
                .get(&ClaimId::new("c3"), &OptionId::new("O"))
                .is_none()
        );
    }

    #[test]
    fn propagation_respects_the_depth_cap() {
        // c1 supports c2 supports c3, c3 directly attached to O. Depth 1 reaches
        // c2 only; depth 2 reaches c1 as well.
        let mut m = AttachmentMatrix::default();
        m.cells.insert(
            (ClaimId::new("c3"), OptionId::new("O")),
            cell(Polarity::Supports, 1.0, AttachSource::Authored),
        );
        let relations = vec![
            rel("c2", "c3", RelationKind::Supports, 1.0),
            rel("c1", "c2", RelationKind::Supports, 1.0),
        ];

        let depth1 = propagate(&m, &relations, &params(1), 0.15);
        assert!(
            depth1
                .get(&ClaimId::new("c2"), &OptionId::new("O"))
                .is_some()
        );
        assert!(
            depth1
                .get(&ClaimId::new("c1"), &OptionId::new("O"))
                .is_none(),
            "depth 1 must not reach c1"
        );

        let depth2 = propagate(&m, &relations, &params(2), 0.15);
        assert!(
            depth2
                .get(&ClaimId::new("c1"), &OptionId::new("O"))
                .is_some(),
            "depth 2 must reach c1 via c2"
        );
    }

    #[test]
    fn propagation_never_overwrites_an_existing_cell() {
        let mut m = AttachmentMatrix::default();
        m.cells.insert(
            (ClaimId::new("s"), OptionId::new("O")),
            cell(Polarity::Supports, 1.0, AttachSource::Authored),
        );
        // c already has an Authored cell saying it Opposes O, contradicting what
        // propagation from s (which supports O, and c contradicts s) would infer.
        m.cells.insert(
            (ClaimId::new("c"), OptionId::new("O")),
            cell(Polarity::Supports, 0.3, AttachSource::Authored),
        );
        let relations = vec![rel("c", "s", RelationKind::Contradicts, 1.0)];
        let result = propagate(&m, &relations, &params(1), 0.15);
        let got = result.get(&ClaimId::new("c"), &OptionId::new("O")).unwrap();
        assert_eq!(
            got.source,
            AttachSource::Authored,
            "the original Authored cell must survive untouched"
        );
        assert_eq!(got.polarity, Polarity::Supports);
        assert_eq!(got.confidence, 0.3);
    }

    #[test]
    fn unrelated_and_uncertain_relations_do_not_propagate() {
        let mut m = AttachmentMatrix::default();
        m.cells.insert(
            (ClaimId::new("s"), OptionId::new("O")),
            cell(Polarity::Supports, 1.0, AttachSource::Authored),
        );
        let relations = vec![
            rel("c1", "s", RelationKind::Unrelated, 1.0),
            rel("c2", "s", RelationKind::Uncertain, 1.0),
        ];
        let result = propagate(&m, &relations, &params(2), 0.15);
        assert!(
            result
                .get(&ClaimId::new("c1"), &OptionId::new("O"))
                .is_none()
        );
        assert!(
            result
                .get(&ClaimId::new("c2"), &OptionId::new("O"))
                .is_none()
        );
    }

    fn opt(id: &str, label: &str) -> DecisionOption {
        DecisionOption::new(OptionId::new(id), label)
    }

    #[test]
    fn rewording_does_not_change_the_score() {
        let a = opt("opt_a", "Modular monolith");
        let mut m = AttachmentMatrix::default();
        m.cells.insert(
            (ClaimId::new("c1"), a.id.clone()),
            cell(Polarity::Supports, 0.9, AttachSource::Authored),
        );
        let mut standings = BTreeMap::new();
        standings.insert(ClaimId::new("c1"), 0.9);

        let before = score_options(std::slice::from_ref(&a), &m, &standings);
        let b = a.reworded("Modular monolith, with enforced boundaries");
        assert_eq!(
            a.id, b.id,
            "cells are keyed by OptionId alone, so a reword cannot orphan them"
        );
        let after = score_options(&[b], &m, &standings);
        assert_eq!(before[0].raw, after[0].raw);
        assert_eq!(before[0].share, after[0].share);
    }

    #[test]
    fn shares_sum_to_one_in_the_non_degenerate_case() {
        let a = opt("a", "A");
        let b = opt("b", "B");
        let mut m = AttachmentMatrix::default();
        m.cells.insert(
            (ClaimId::new("c1"), a.id.clone()),
            cell(Polarity::Supports, 0.8, AttachSource::Authored),
        );
        m.cells.insert(
            (ClaimId::new("c2"), b.id.clone()),
            cell(Polarity::Supports, 0.2, AttachSource::Authored),
        );
        let mut standings = BTreeMap::new();
        standings.insert(ClaimId::new("c1"), 0.8);
        standings.insert(ClaimId::new("c2"), 0.2);
        let scores = score_options(&[a, b], &m, &standings);
        let sum: f64 = scores.iter().map(|s| s.share).sum();
        assert!((sum - 1.0).abs() < 1e-9);
    }

    /// D10: every option net non-positive -> every share 0, not NaN.
    #[test]
    fn all_non_positive_raw_gives_all_zero_shares_not_nan() {
        let a = opt("a", "A");
        let b = opt("b", "B");
        let mut m = AttachmentMatrix::default();
        m.cells.insert(
            (ClaimId::new("c1"), a.id.clone()),
            cell(Polarity::Opposes, 0.9, AttachSource::Authored),
        );
        m.cells.insert(
            (ClaimId::new("c2"), b.id.clone()),
            cell(Polarity::Opposes, 0.9, AttachSource::Authored),
        );
        let mut standings = BTreeMap::new();
        standings.insert(ClaimId::new("c1"), 0.9);
        standings.insert(ClaimId::new("c2"), 0.9);
        let scores = score_options(&[a, b], &m, &standings);
        for s in &scores {
            assert_eq!(s.raw, -0.45);
            assert_eq!(s.share, 0.0, "must be exactly 0.0, never NaN");
            assert!(!s.share.is_nan());
        }
    }

    /// The whole point of §6.5: model vote count is not an input. Four models'
    /// worth of weakly-evidenced claims back A; one model's single
    /// strongly-evidenced claim backs B. B must score higher.
    #[test]
    fn model_vote_count_does_not_decide_the_score() {
        let a = opt("a", "Popular but weak");
        let b = opt("b", "Unpopular but strong");
        let mut m = AttachmentMatrix::default();
        let mut standings = BTreeMap::new();
        for i in 0..4 {
            let id = ClaimId::new(format!("weak{i}"));
            m.cells.insert(
                (id.clone(), a.id.clone()),
                cell(Polarity::Supports, 0.15, AttachSource::Authored),
            );
            standings.insert(id, 0.15); // low standing: poorly evidenced
        }
        m.cells.insert(
            (ClaimId::new("strong"), b.id.clone()),
            cell(Polarity::Supports, 0.95, AttachSource::Authored),
        );
        standings.insert(ClaimId::new("strong"), 0.95);

        let scores = score_options(&[a, b], &m, &standings);
        let score_a = scores.iter().find(|s| s.id == OptionId::new("a")).unwrap();
        let score_b = scores.iter().find(|s| s.id == OptionId::new("b")).unwrap();
        assert!(
            score_b.raw > score_a.raw,
            "one strong claim (0.95) must outweigh four weak ones (4*0.15=0.60)"
        );
        assert!(score_b.share > score_a.share);
    }

    #[test]
    fn retired_options_are_the_callers_concern_not_filtered_here() {
        // score_options scores exactly what it is given; filtering retired
        // options happens before calling it (documented on the function).
        let mut retired = opt("a", "Abandoned");
        retired.retired = true;
        let m = AttachmentMatrix::default();
        let scores = score_options(&[retired.clone()], &m, &BTreeMap::new());
        assert_eq!(
            scores.len(),
            1,
            "the function itself does not filter -- this documents that contract"
        );
    }
}
