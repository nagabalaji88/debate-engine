//! The active policy: which version's constants a decision was computed under, and
//! whether they are still provisional.
//!
//! ARCHITECTURE §6.3: "Constants ship marked provisional. `argument-v1` … carr[ies]
//! `provisional = true` in config until the gate below has run; `arbiter doctor`
//! reports which constants are still provisional, and the 1.0 release checklist
//! requires none to be." The gate itself — the tuning sweep plus a red-team
//! session — is a release-process fact, not something this crate can observe; this
//! type only records the one bit code needs: has this constant set been pinned yet.

use crate::config::DecisionConfig;
use crate::ids::PolicyVersion;
use serde::{Deserialize, Serialize};

/// The constants a `DecisionRecord` was computed under. Two `DecisionRecord`s are
/// only comparable when their `version` matches (§6.9: "decisions compare only
/// within a version") — swapping the constants mints a new version rather than
/// silently changing what past decisions meant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub version: PolicyVersion,
    /// True until the tuning sweep and the red-team session have both run against
    /// this exact constant set (§6.3). Ships `true`; a release process flips it,
    /// this crate never flips it itself.
    pub provisional: bool,
    pub config: DecisionConfig,
}

impl Policy {
    /// The policy this codebase currently implements. Provisional until the gate in
    /// §6.3 has run — that has not happened yet, so this always ships `true`.
    pub fn argument_v1() -> Self {
        Self {
            version: PolicyVersion::new("argument-v1"),
            provisional: true,
            config: DecisionConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_v1_ships_provisional() {
        let p = Policy::argument_v1();
        assert_eq!(p.version.as_str(), "argument-v1");
        assert!(
            p.provisional,
            "the tuning sweep and red-team session have not run; \
            this must not silently claim to be pinned"
        );
    }
}
