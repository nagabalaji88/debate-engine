//! `DecisionRecord`, ARCHITECTURE §6.9 / INTERFACES §22. The canonical output: API
//! response, Build Studio input, stored record, and `arbiter explain --json`
//! payload — one structure, never a separate rendering code path.
//!
//! PLAN_DEVIATIONS.md D18: §6.9's full JSON also names `model_agreement`, `dissent`
//! (with per-claim `risk_awareness`), `assumptions` (with a `decision_impact`
//! classification), `acceptance` and `completeness` (`Completeness`/`StopReason`/
//! `StageName`, already deferred at D12). None of these has a rule this crate has
//! been given — `model_agreement` needs the raw per-model vote tally, `dissent`
//! needs a per-claim judge dossier join, `assumptions` needs a `decision_impact`
//! classifier nowhere specified. Inventing shapes for untested guesses is exactly
//! what §0.2 rule 1 forbids; this task ships the fields the spec gives a concrete
//! formula or type for, and the rest wait for whichever kernel stage first has
//! their inputs (`G9 decision.synthesize`, which is why it depends on C8).

use crate::claim::ClaimStanding;
use crate::config::ConfidenceWeights;
use crate::decision::outcome::Outcome;
use crate::decision::triggers::{CounterfactualFlip, FlipDirection};
use crate::decision::{confidence::ConfidenceBreakdown, rank_by_share};
use crate::ids::{ClaimId, OptionId, PolicyVersion, RunId};
use crate::option::OptionScore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// INTERFACES §22 is explicit that this is versioned so a future incompatible
/// change bumps it rather than breaking silently.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DimensionEntry {
    pub name: String,
    pub value: f64,
    pub weight: f64,
    pub contribution: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PenaltyEntry {
    pub name: String,
    /// The raw input the penalty was computed from (a ratio, a 0/1 flag, or the
    /// judge-dispersion value) — recovered from `contribution / rate` for every
    /// penalty except `dispersion`, whose formula subtracts a threshold before
    /// scaling and so is not a pure multiply.
    pub input: Option<f64>,
    pub rate: f64,
    /// Negative — this is a subtraction from `base`, and the sign is part of the
    /// schema (INTERFACES §22's own worked example: `"contribution": -0.0200`).
    pub contribution: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceExplain {
    pub total: f64,
    pub base: f64,
    pub dimensions: Vec<DimensionEntry>,
    pub penalties: Vec<PenaltyEntry>,
}

/// Renders a [`ConfidenceBreakdown`] into INTERFACES §22's `confidence` object.
/// Every dimension/penalty `contribution` here is exactly the term `total` was
/// computed from — nothing is recomputed with different arithmetic, so the two can
/// never drift (INTERFACES §14's own invariant: "total == clamp01(base − Σ
/// penalties), recomputed, never stored independently").
pub fn explain_confidence(b: &ConfidenceBreakdown, w: &ConfidenceWeights) -> ConfidenceExplain {
    let dimensions = vec![
        DimensionEntry {
            name: "evidence_mass".to_string(),
            value: b.evidence_mass,
            weight: w.evidence_mass,
            contribution: w.evidence_mass * b.evidence_mass,
        },
        DimensionEntry {
            name: "decision_margin".to_string(),
            value: b.decision_margin,
            weight: w.margin,
            contribution: w.margin * b.decision_margin,
        },
        DimensionEntry {
            name: "judge_score".to_string(),
            value: b.judge_score,
            weight: w.judge,
            contribution: w.judge * b.judge_score,
        },
    ];

    // `magnitude` is the always-positive penalty value stored on the breakdown;
    // dividing back by `rate` recovers the ratio/flag it was scaled from, since
    // every one of these four is a pure `rate * input` multiply.
    let ratio_penalty = |name: &str, magnitude: f64, rate: f64| PenaltyEntry {
        name: name.to_string(),
        input: if rate > 0.0 {
            Some(magnitude / rate)
        } else {
            Some(0.0)
        },
        rate,
        contribution: -magnitude,
        note: None,
    };

    let penalties = vec![
        ratio_penalty("unresolved", b.unresolved_penalty, w.unresolved_penalty),
        ratio_penalty("assumption", b.assumption_penalty, w.assumption_penalty),
        ratio_penalty("truncation", b.truncation_penalty, w.truncation_penalty),
        ratio_penalty("convergence", b.convergence_penalty, w.convergence_penalty),
        PenaltyEntry {
            name: "dispersion".to_string(),
            input: b.judge_dispersion,
            rate: w.dispersion_weight,
            contribution: -b.dispersion_penalty,
            note: b
                .judge_dispersion
                .is_none()
                .then(|| "inactive — judge_count == 1".to_string()),
        },
    ];

    ConfidenceExplain {
        total: b.total,
        base: b.base,
        dimensions,
        penalties,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recommendation {
    pub option_id: OptionId,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ClaimCounts {
    pub agreed: u32,
    pub disputed: u32,
    pub unresolved: u32,
    pub defeated: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeTriggerEntry {
    pub claim_id: ClaimId,
    pub direction: FlipDirection,
    pub new_winner: Option<OptionId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub schema_version: u32,
    pub run_id: RunId,
    pub policy_version: PolicyVersion,
    pub question: String,
    pub outcome: Outcome,
    /// `None` only when `options` is empty — no ranked option exists to recommend.
    pub recommendation: Option<Recommendation>,
    pub confidence: ConfidenceExplain,
    pub options: Vec<OptionScore>,
    pub claims: ClaimCounts,
    /// Only the flips that actually changed the winner
    /// ([`CounterfactualFlip::is_trigger`]) — matching §6.9's own `change_triggers`
    /// example, which lists triggering claims only.
    pub change_triggers: Vec<ChangeTriggerEntry>,
    pub unresolved_claims: Vec<ClaimId>,
    /// Every claim's classification, not just the unresolved ones —
    /// `arbiter claims --state`'s only source, since no other persisted
    /// artifact carries the post-fixpoint classification at all
    /// (PLAN_DEVIATIONS.md D43).
    pub claim_standings: BTreeMap<ClaimId, ClaimStanding>,
    pub engine_version: String,
    pub inputs_hash: String,
}

/// Assembles a [`DecisionRecord`] from every already-computed piece: `options` is
/// [`crate::decision::attachment::score_options`]'s output, `confidence` is
/// [`explain_confidence`]'s, `claim_standings` is
/// [`crate::decision::standing::classify_all`]'s, and `flips` is
/// [`crate::decision::triggers::counterfactual_flips`]'s. This function does no
/// arithmetic of its own beyond ranking and counting — every number in the record
/// was computed once, upstream, by the task that owns that formula.
#[allow(clippy::too_many_arguments)]
pub fn build(
    run_id: RunId,
    policy_version: PolicyVersion,
    question: impl Into<String>,
    outcome: Outcome,
    options: Vec<OptionScore>,
    confidence: ConfidenceExplain,
    claim_standings: &BTreeMap<ClaimId, crate::claim::ClaimStanding>,
    flips: &[CounterfactualFlip],
    engine_version: impl Into<String>,
    inputs_hash: impl Into<String>,
) -> DecisionRecord {
    let ranked = rank_by_share(&options);
    let recommendation = ranked.first().map(|o| Recommendation {
        option_id: o.id.clone(),
        label: o.label.clone(),
    });

    let mut claims = ClaimCounts::default();
    let mut unresolved_claims = Vec::new();
    for (claim_id, standing) in claim_standings {
        match standing {
            crate::claim::ClaimStanding::Agreed => claims.agreed += 1,
            crate::claim::ClaimStanding::Disputed => claims.disputed += 1,
            crate::claim::ClaimStanding::Unresolved => {
                claims.unresolved += 1;
                unresolved_claims.push(claim_id.clone());
            }
            crate::claim::ClaimStanding::Defeated => claims.defeated += 1,
        }
    }

    let change_triggers = flips
        .iter()
        .filter(|f| f.is_trigger)
        .map(|f| ChangeTriggerEntry {
            claim_id: f.claim_id.clone(),
            direction: f.direction,
            new_winner: f.new_winner.clone(),
        })
        .collect();

    DecisionRecord {
        schema_version: SCHEMA_VERSION,
        run_id,
        policy_version,
        question: question.into(),
        outcome,
        recommendation,
        confidence,
        options,
        claims,
        change_triggers,
        unresolved_claims,
        claim_standings: claim_standings.clone(),
        engine_version: engine_version.into(),
        inputs_hash: inputs_hash.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::ClaimStanding;
    use crate::decision::confidence::PenaltyInputs;
    use crate::ids::ModelId;
    use crate::judge::Scorecard;

    fn flat_scorecard(v: f64) -> Scorecard {
        Scorecard {
            model: ModelId::new("m"),
            factual_correctness: v,
            logical_reasoning: v,
            evidence_quality: v,
            problem_relevance: v,
            assumption_quality: v,
            counterargument_handling: v,
            risk_awareness: v,
            practicality: v,
            clarity: v,
        }
    }

    fn worked_example_breakdown() -> ConfidenceBreakdown {
        let scores = [
            OptionScore {
                id: OptionId::new("a"),
                label: "a".into(),
                raw: 0.905,
                share: 0.905,
            },
            OptionScore {
                id: OptionId::new("b"),
                label: "b".into(),
                raw: 0.095,
                share: 0.095,
            },
        ];
        let judges = [flat_scorecard(0.91)];
        let penalties = PenaltyInputs {
            unresolved_critical_ratio: 0.08,
            assumption_dependency_ratio: 0.07,
            truncated: false,
            fixpoint_converged: true,
        };
        crate::decision::confidence::confidence(
            0.88,
            &scores,
            &judges,
            &penalties,
            &ConfidenceWeights::default(),
        )
    }

    #[test]
    fn explain_json_matches_schema_v1() {
        let b = worked_example_breakdown();
        let explain = explain_confidence(&b, &ConfidenceWeights::default());
        let json = serde_json::to_value(&explain).unwrap();

        assert!((json["total"].as_f64().unwrap() - 0.8390).abs() < 1e-9);
        assert!((json["base"].as_f64().unwrap() - 0.8695).abs() < 1e-9);
        assert_eq!(json["dimensions"].as_array().unwrap().len(), 3);
        assert_eq!(json["penalties"].as_array().unwrap().len(), 5);

        let unresolved = json["penalties"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "unresolved")
            .unwrap();
        assert!((unresolved["input"].as_f64().unwrap() - 0.08).abs() < 1e-9);
        assert!((unresolved["contribution"].as_f64().unwrap() - (-0.0200)).abs() < 1e-9);

        let dispersion = json["penalties"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "dispersion")
            .unwrap();
        assert!(dispersion["input"].is_null());
        assert_eq!(dispersion["note"], "inactive — judge_count == 1");
    }

    #[test]
    fn contributions_sum_to_total() {
        let b = worked_example_breakdown();
        let explain = explain_confidence(&b, &ConfidenceWeights::default());

        let dims_sum: f64 = explain.dimensions.iter().map(|d| d.contribution).sum();
        let penalties_sum: f64 = explain.penalties.iter().map(|p| p.contribution).sum();
        assert!(
            (dims_sum - explain.base).abs() < 1e-9,
            "dimension contributions must sum to base exactly"
        );
        assert!(
            ((dims_sum + penalties_sum) - explain.total).abs() < 1e-9,
            "base plus (already-negative) penalty contributions must equal total"
        );
    }

    #[test]
    fn penalties_array_has_five_entries() {
        let b = worked_example_breakdown();
        let explain = explain_confidence(&b, &ConfidenceWeights::default());
        assert_eq!(explain.penalties.len(), 5);
        let names: Vec<&str> = explain.penalties.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "unresolved",
                "assumption",
                "truncation",
                "convergence",
                "dispersion"
            ]
        );
    }

    #[test]
    fn build_derives_recommendation_from_the_highest_share_option() {
        let options = vec![
            OptionScore {
                id: OptionId::new("a"),
                label: "A".into(),
                raw: 0.3,
                share: 0.3,
            },
            OptionScore {
                id: OptionId::new("b"),
                label: "B".into(),
                raw: 0.7,
                share: 0.7,
            },
        ];
        let b = worked_example_breakdown();
        let explain = explain_confidence(&b, &ConfidenceWeights::default());
        let record = build(
            RunId::new("run1"),
            PolicyVersion::new("argument-v1"),
            "which option?",
            Outcome::MajorityWithDissent,
            options,
            explain,
            &BTreeMap::new(),
            &[],
            "0.1.0",
            "blake3:deadbeef",
        );
        assert_eq!(record.schema_version, SCHEMA_VERSION);
        assert_eq!(record.recommendation.unwrap().option_id, OptionId::new("b"));
    }

    #[test]
    fn build_counts_claims_and_only_lists_unresolved_ids() {
        let mut standings = BTreeMap::new();
        standings.insert(ClaimId::new("c1"), ClaimStanding::Agreed);
        standings.insert(ClaimId::new("c2"), ClaimStanding::Disputed);
        standings.insert(ClaimId::new("c3"), ClaimStanding::Unresolved);
        standings.insert(ClaimId::new("c4"), ClaimStanding::Defeated);
        standings.insert(ClaimId::new("c5"), ClaimStanding::Unresolved);

        let b = worked_example_breakdown();
        let explain = explain_confidence(&b, &ConfidenceWeights::default());
        let record = build(
            RunId::new("run1"),
            PolicyVersion::new("argument-v1"),
            "q",
            Outcome::Consensus,
            vec![],
            explain,
            &standings,
            &[],
            "0.1.0",
            "blake3:deadbeef",
        );

        assert_eq!(
            record.claims,
            ClaimCounts {
                agreed: 1,
                disputed: 1,
                unresolved: 2,
                defeated: 1,
            }
        );
        assert_eq!(
            record.unresolved_claims,
            vec![ClaimId::new("c3"), ClaimId::new("c5")]
        );
        assert!(record.recommendation.is_none(), "no options were given");
    }

    #[test]
    fn build_keeps_only_triggering_flips_in_change_triggers() {
        let trigger = CounterfactualFlip {
            claim_id: ClaimId::new("c1"),
            direction: FlipDirection::IfTrue,
            new_winner: Some(OptionId::new("b")),
            margin_before: 0.5,
            margin_after: -0.1,
            is_trigger: true,
        };
        let quiet = CounterfactualFlip {
            claim_id: ClaimId::new("c2"),
            direction: FlipDirection::IfFalse,
            new_winner: Some(OptionId::new("a")),
            margin_before: 0.5,
            margin_after: 0.45,
            is_trigger: false,
        };

        let b = worked_example_breakdown();
        let explain = explain_confidence(&b, &ConfidenceWeights::default());
        let record = build(
            RunId::new("run1"),
            PolicyVersion::new("argument-v1"),
            "q",
            Outcome::MajorityWithDissent,
            vec![],
            explain,
            &BTreeMap::new(),
            &[trigger, quiet],
            "0.1.0",
            "blake3:deadbeef",
        );

        assert_eq!(record.change_triggers.len(), 1);
        assert_eq!(record.change_triggers[0].claim_id, ClaimId::new("c1"));
    }
}
