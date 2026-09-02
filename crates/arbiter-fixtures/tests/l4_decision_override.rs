//! F2 — `decision_override` (L4), ARCHITECTURE §18's CI suite: "accepted
//! with an override → provenance carries UserOverride."
//!
//! Two real gaps stand between this fixture and the literal proves-line, both
//! already logged (D45, and this session's own D47 for the second):
//! `Provenance::UserOverride` is a *generated Build Studio spec's* pointer
//! back to the override that produced one of its fields (INTERFACES §17) —
//! Build Studio itself does not exist in this workspace (ARCHITECTURE §13,
//! explicitly out of scope), so there is no `Provenance` type anywhere to
//! carry that variant. And `arbiter accept`'s own validation ("an
//! unexplained override is rejected") lives in `arbiter-cli::accept_command`,
//! which `arbiter-fixtures` cannot depend on (the dependency rule, X2's own
//! test: "nothing depends on cli").
//!
//! What *is* real and in this crate's reach: `DecisionAcceptance`/
//! `DecisionOverride` (`arbiter-core::acceptance`, D45's own scope) are the
//! actual persisted record of who overrode what, from what, to what, and
//! why — the closest true statement to "provenance carries the override"
//! available without either missing piece. This fixture proves that record
//! carries its full audit trail intact, including through serialization
//! (`arbiter accept`'s own persistence path, INTERFACES §17).

use arbiter_core::ids::OverrideId;
use arbiter_core::{DecisionAcceptance, DecisionOverride};

/// `decision_override`: an accepted decision with a human override records
/// exactly what changed and why — the id, the field path, the reason
/// (required by INTERFACES §17, even though this crate cannot reach the
/// command that enforces that requirement), and both the prior and new
/// value — and every one of those fields survives a full serialize/
/// deserialize round trip unchanged, since that is genuinely how `accept`
/// persists it.
#[test]
fn decision_override() {
    let acceptance = DecisionAcceptance {
        accepted_by: "operator@example.com".to_string(),
        accepted_at: "2026-09-02T00:00:00Z".to_string(),
        overrides: vec![DecisionOverride {
            id: OverrideId::new("override_1"),
            path: "recommendation.option_id".to_string(),
            from: serde_json::json!("opt_microservices"),
            to: serde_json::json!("opt_modular_monolith"),
            reason: "team lacks operational experience running microservices in production"
                .to_string(),
        }],
    };

    assert_eq!(acceptance.overrides.len(), 1);
    let override_record = &acceptance.overrides[0];
    assert!(
        !override_record.reason.is_empty(),
        "INTERFACES §17: an unexplained override has no place in the record"
    );
    assert_ne!(
        override_record.from, override_record.to,
        "an override that changes nothing is not a real override -- both fields must genuinely differ"
    );

    let json = serde_json::to_string(&acceptance).expect("DecisionAcceptance must serialize");
    let round_tripped: DecisionAcceptance =
        serde_json::from_str(&json).expect("DecisionAcceptance must deserialize");

    assert_eq!(
        round_tripped, acceptance,
        "the full override provenance -- id, path, from, to, reason -- must survive persistence intact"
    );
}
