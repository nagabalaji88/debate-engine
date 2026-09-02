//! F2 — standing classification (C3), ARCHITECTURE §18's CI suite.

use arbiter_core::claim::{ClaimLifecycle, ClaimMember, EvidenceKind, Grounding, TextSpan};
use arbiter_core::config::Thresholds;
use arbiter_core::decision::standing::classify_all;
use arbiter_core::relation::{Relation, RelationKind};
use arbiter_core::{CanonicalClaim, ClaimId, ClaimStanding, ModelId, PositionId, ProviderId};
use std::collections::BTreeMap;

fn claim(id: &str) -> CanonicalClaim {
    CanonicalClaim {
        id: ClaimId::new(id),
        text: format!("claim {id}"),
        kind: EvidenceKind::Fact,
        lifecycle: ClaimLifecycle::Proposed,
        members: vec![ClaimMember::new(
            ClaimId::new(id),
            ModelId::new("model-a"),
            ProviderId::new("anthropic"),
            PositionId::new("pos-a"),
            format!("claim {id}"),
            Grounding::DirectQuote {
                span: TextSpan {
                    start: 0,
                    end: 8,
                    quote: format!("claim {id}"),
                },
            },
        )],
    }
}

/// `strong_dissent`: "surviving contradiction retained in the record." A
/// claim under attack from a *live* attacker (standing at or above
/// `Thresholds::live_attacker`, 0.30) must classify `Disputed`, not silently
/// collapse to `Agreed` just because its own standing is otherwise healthy
/// — the contradiction survives into the classification, not just the raw
/// standing number.
#[test]
fn strong_dissent() {
    let claims = [claim("fact"), claim("dissenter")];
    let mut standings = BTreeMap::new();
    standings.insert(ClaimId::new("fact"), 0.70); // would read Agreed (>= 0.50) alone
    standings.insert(ClaimId::new("dissenter"), 0.60); // well above the 0.30 live-attacker bar
    let relations = [Relation {
        from: ClaimId::new("dissenter"),
        to: ClaimId::new("fact"),
        kind: RelationKind::Contradicts,
        confidence: 0.9,
    }];

    let result = classify_all(&claims, &standings, &relations, &Thresholds::default());
    assert_eq!(
        result[&ClaimId::new("fact")],
        ClaimStanding::Disputed,
        "a live, surviving contradiction must keep the claim Disputed, not Agreed"
    );
}
