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
id_type!(/// A candidate recommendation.
    OptionId);
id_type!(/// A single debate run.
    RunId);
