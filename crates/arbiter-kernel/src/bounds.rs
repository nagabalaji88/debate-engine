//! Hard bounds the controller can never exceed, and the per-round budget sizing
//! derived from them. ARCHITECTURE §5.1, §5.5.

use crate::store::Cost;

/// No `--depth` setting, however deep, can push rounds past this.
pub const HARD_ROUND_CEILING: u32 = 6;

pub const DEFAULT_MAX_COST: f64 = 2.00;
pub const DEFAULT_MAX_WALL_TIME_SECS: u64 = 300;
/// Reserved, not spendable — every planner sizes itself against `max_cost × (1 −
/// budget_headroom)`, released for use in the final round only (§5.5).
pub const DEFAULT_BUDGET_HEADROOM: f64 = 0.05;
/// Per round, not per run (§5.1 / IMPLEMENTATION_PLAN.md §0.6's own correction of
/// an earlier "per run" misreading).
pub const DEFAULT_REPAIR_BUDGET_FRACTION: f64 = 0.15;

/// `controller.decide`'s own three constants (§5.5, IMPLEMENTATION_PLAN.md
/// §0.6 D5: explicitly "kernel controller", not `arbiter-core`'s to own).
pub const DEFAULT_CONVERGED_MARGIN_FACTOR: f64 = 1.5;
pub const DEFAULT_MIN_NEW_CLAIMS: usize = 2;
pub const DEFAULT_MIN_STANDING_DELTA: f64 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    Standard,
    Deep,
}

impl Depth {
    /// §5.5: "1 (`--depth standard`) · 3 (`--depth deep`) · hard ceiling 6".
    pub fn max_rounds(self) -> u32 {
        match self {
            Depth::Standard => 1,
            Depth::Deep => 3,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Bounds {
    pub max_rounds: u32,
    pub max_cost: Cost,
    pub max_tokens: Option<u64>,
    pub max_wall_time_secs: u64,
    pub budget_headroom: f64,
}

impl Bounds {
    pub fn for_depth(depth: Depth) -> Self {
        Self {
            max_rounds: depth.max_rounds(),
            max_cost: Cost(DEFAULT_MAX_COST),
            max_tokens: None,
            max_wall_time_secs: DEFAULT_MAX_WALL_TIME_SECS,
            budget_headroom: DEFAULT_BUDGET_HEADROOM,
        }
    }

    /// The cap every planner sizes itself against for `round` of `self.max_rounds`
    /// — `max_cost` minus headroom, except in the final round, when headroom is
    /// released because there is no later round left to starve.
    pub fn usable_cap(&self, round: u32) -> Cost {
        if round >= self.max_rounds {
            self.max_cost
        } else {
            Cost(self.max_cost.0 * (1.0 - self.budget_headroom))
        }
    }
}

/// "Each round takes `remaining_budget ÷ remaining_rounds`" (§5.5) — the whole
/// reason the challenge budget is derived from money, not panel size: a
/// per-model call count scales with the panel and can silently blow the cap
/// (§5.5's own worked example: 7 models × 3 rounds × 2 challenges prices out
/// above a $2.00 cap).
pub fn round_budget(remaining_budget: Cost, remaining_rounds: u32) -> Cost {
    if remaining_rounds == 0 {
        return Cost(0.0);
    }
    Cost(remaining_budget.0 / remaining_rounds as f64)
}

/// "Reserves the judge's share first, and spends what is left on the
/// highest-priority disputes" (§5.5). Floored at zero: a round budget smaller
/// than the judge's reservation leaves nothing for challenges, not a negative
/// spend.
pub fn challenge_budget(round_budget: Cost, judge_share: Cost) -> Cost {
    Cost((round_budget.0 - judge_share.0).max(0.0))
}

/// The repair pass's own slice of a round's budget (§5.1).
pub fn repair_budget(round_budget: Cost, fraction: f64) -> Cost {
    Cost(round_budget.0 * fraction)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_cost_eq(actual: Cost, expected: f64) {
        assert!(
            (actual.0 - expected).abs() < 1e-9,
            "expected {expected}, got {}",
            actual.0
        );
    }

    #[test]
    fn controller_constants_match_the_spec() {
        assert_eq!(DEFAULT_CONVERGED_MARGIN_FACTOR, 1.5);
        assert_eq!(DEFAULT_MIN_NEW_CLAIMS, 2);
        assert_eq!(DEFAULT_MIN_STANDING_DELTA, 0.05);
    }

    #[test]
    fn depth_round_limits_match_the_spec() {
        assert_eq!(Depth::Standard.max_rounds(), 1);
        assert_eq!(Depth::Deep.max_rounds(), 3);
        assert!(Depth::Deep.max_rounds() < HARD_ROUND_CEILING);
    }

    #[test]
    fn headroom_is_withheld_except_in_the_final_round() {
        let bounds = Bounds::for_depth(Depth::Deep); // max_rounds = 3
        assert_cost_eq(bounds.usable_cap(1), 2.00 * 0.95);
        assert_cost_eq(bounds.usable_cap(2), 2.00 * 0.95);
        assert_cost_eq(bounds.usable_cap(3), 2.00); // final round: headroom released
    }

    #[test]
    fn round_budget_is_remaining_over_remaining_rounds() {
        assert_cost_eq(round_budget(Cost(1.00), 4), 0.25);
        assert_cost_eq(round_budget(Cost(0.0), 3), 0.0);
        assert_cost_eq(round_budget(Cost(1.00), 0), 0.0); // no rounds left: no division by zero
    }

    #[test]
    fn challenge_budget_reserves_the_judge_share_first() {
        assert_cost_eq(challenge_budget(Cost(0.50), Cost(0.10)), 0.40);
        // A round budget too small for the judge share leaves zero, not negative.
        assert_cost_eq(challenge_budget(Cost(0.05), Cost(0.10)), 0.0);
    }

    #[test]
    fn the_seven_model_deep_worked_example_would_have_blown_a_per_model_cap() {
        // ARCHITECTURE §5.5's own illustration: 7 models x 3 rounds x 2
        // challenges prices out at ~$2.03 against a $2.00 cap under a per-model
        // count -- this is why sizing is money-derived instead. Not a literal
        // dollar-cost test (no per-call price model exists yet), just confirming
        // round_budget/challenge_budget never themselves exceed what's available.
        let bounds = Bounds::for_depth(Depth::Deep);
        let mut remaining = bounds.usable_cap(1);
        for round in 1..=3 {
            let remaining_rounds = bounds.max_rounds - round + 1;
            let this_round = round_budget(remaining, remaining_rounds);
            assert!(this_round.0 <= remaining.0 + 1e-9);
            remaining = Cost(remaining.0 - this_round.0);
        }
        assert_cost_eq(remaining, 0.0);
    }
}
