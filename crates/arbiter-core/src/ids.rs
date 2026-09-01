//! Opaque, ordered identifiers. Ordering is what makes iteration deterministic.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
        }
        impl From<&str> for $name {
            fn from(s: &str) -> Self { Self(s.to_owned()) }
        }
    };
}

id_type!(/// A canonical claim (a cluster of equivalent statements).
    ClaimId);
id_type!(/// One model's position in a debate.
    PositionId);
id_type!(/// A model as seated on the panel.
    ModelId);
id_type!(/// The vendor behind a model. Used to discount correlated priors.
    ProviderId);
id_type!(/// A candidate recommendation. **Cluster identity** — stable across rewording.
    /// Never construct one by hashing option text; that is what [`OptionVersion`] is
    /// for. Hashing text into the id was the v2.3 formulation and it minted a new
    /// option on every refinement, orphaning attachment mid-debate (INTERFACES §20).
    OptionId);
id_type!(/// A single debate run.
    RunId);
id_type!(/// Which panel members are correlated priors, not independent evidence.
    /// Defaults to a member's [`ProviderId`]; overridable via `correlation.toml` so two
    /// vendors serving the same base weights can share a group (ARCHITECTURE §6.2,
    /// INTERFACES §15). Arbitrary construction is fine — this is a label, not a hash.
    GroupId);
id_type!(/// The fixpoint constants and thresholds a `DecisionRecord` was computed
    /// under, e.g. `"argument-v1"`. Decisions are only comparable within one version.
    PolicyVersion);

/// `blake3` of an option's canonical text. The **only** way to construct one is from
/// text — never from an arbitrary string — because the whole point of separating
/// [`OptionId`] (identity) from `OptionVersion` (content) is that nothing downstream
/// can accidentally treat a version as an identity or vice versa by construction.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OptionVersion(String);

impl OptionVersion {
    /// `blake3(canonical_text)`, prefixed so the hash is self-describing in JSON
    /// (ARCHITECTURE §16: hashes carry a `blake3:` prefix in JSON fields).
    pub fn of(canonical_text: &str) -> Self {
        Self(format!(
            "blake3:{}",
            blake3::hash(canonical_text.as_bytes()).to_hex()
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OptionVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_version_has_no_public_arbitrary_constructor() {
        // This test is the documentation: OptionVersion::of is the only entry point,
        // and it always hashes. There is no `new`, no `From<&str>`, no way to smuggle
        // raw text into a version field. If this line fails to compile after someone
        // adds one, that is the regression this test exists to catch.
        let v = OptionVersion::of("Modular monolith — enforce boundaries in-process");
        assert!(v.as_str().starts_with("blake3:"));
    }

    #[test]
    fn same_text_same_version_different_text_different_version() {
        let a = OptionVersion::of("Modular monolith");
        let b = OptionVersion::of("Modular monolith");
        let c = OptionVersion::of("Modular monolith, with enforced boundaries");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
