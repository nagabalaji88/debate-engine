//! F2 — claims.normalize batch partitioning (owned by G2's `claims.normalize`
//! implementation), ARCHITECTURE §18's CI suite.

use arbiter_core::claim::{ClaimLifecycle, ClaimMember, EvidenceKind, Grounding};
use arbiter_core::{CanonicalClaim, ClaimId, ModelId, PositionId, ProviderId};
use arbiter_fixtures::harness::{RecordingSink, template};
use arbiter_kernel::budget::BudgetLedger;
use arbiter_kernel::cache::ResponseCache;
use arbiter_kernel::event::EventType;
use arbiter_kernel::stage::{
    CancellationToken, DeterministicRng, ProviderRegistry, Stage, StageContext,
};
use arbiter_kernel::stages::claims_extract::ExtractedClaims;
use arbiter_kernel::stages::claims_normalize::ClaimsNormalize;
use arbiter_kernel::store::Cost;
use arbiter_providers::mock::MockProvider;
use std::time::{Duration, Instant};

/// A claim whose text shares zero trigrams with any other claim from this
/// function (a distinct repeated Unicode scalar per index), so `top_k_pairs`
/// (cosine over character trigrams) returns no candidate pair at all and
/// `partition_into_batches` falls back to its singleton-component path,
/// packing claims into contiguous batches of exactly `max_claims_per_batch`
/// (60) in index order — the deterministic shape this fixture needs to
/// script exact batch/stitch responses against.
fn distinct_claim(i: usize) -> CanonicalClaim {
    let ch = char::from_u32(0x1000 + i as u32).expect("valid scalar in this fixture's range");
    let text: String = std::iter::repeat_n(ch, 3).collect();
    let id = ClaimId::new(format!("claim_{i:03}"));
    CanonicalClaim {
        id: id.clone(),
        text: text.clone(),
        kind: EvidenceKind::Fact,
        lifecycle: ClaimLifecycle::Proposed,
        members: vec![ClaimMember::new(
            id,
            ModelId::new("model-a"),
            ProviderId::new("mock"),
            PositionId::new("pos-a"),
            text,
            Grounding::Unsupported,
        )],
    }
}

fn merge_all(n: usize) -> serde_json::Value {
    let members: Vec<String> = (1..=n).map(|i| format!("#{i}")).collect();
    serde_json::json!([{"members": members, "confidence": 0.9}])
}

/// `t3_batch_partition`: "180 claims → partitioned batches + stitch pass, no
/// claim lost." 180 mutually-dissimilar claims exceed the default
/// `t3_max_claims_per_batch` (60) three times over, forcing genuine
/// partitioning into three batches. Each batch is scripted to merge fully
/// into a single representative (bringing the post-batch root count to 3,
/// at or under the cap), which is exactly what triggers INTERFACES §3's
/// stitch pass; the stitch call then merges two of those three
/// representatives. Every one of the 180 original claims must still be
/// reachable as a member of the final output — merging changes grouping,
/// never survivorship.
#[tokio::test]
async fn t3_batch_partition() {
    let claims: Vec<CanonicalClaim> = (0..180).map(distinct_claim).collect();

    let mock = MockProvider::new(
        ProviderId::new("mock"),
        arbiter_kernel::provider::ProviderCapabilities {
            structured_output: false,
            streaming: false,
            idempotency: None,
        },
    );
    // Three batch calls, each merging its own 60 items into one group.
    mock.script_text(merge_all(60).to_string());
    mock.script_text(merge_all(60).to_string());
    mock.script_text(merge_all(60).to_string());
    // Stitch call over the 3 surviving representatives: merge the first two.
    mock.script_text(serde_json::json!([{"members": ["#1", "#2"], "confidence": 0.9}]).to_string());

    let mut providers = ProviderRegistry::new();
    providers.register(Box::new(mock));
    let budget = BudgetLedger::unbounded();
    let cache = ResponseCache::new();
    let events = RecordingSink::new();
    let c = StageContext {
        providers: &providers,
        budget: &budget,
        events: &events,
        cache: &cache,
        deadline: Instant::now() + Duration::from_secs(30),
        cancel: CancellationToken::new(),
        round: 1,
        rng: DeterministicRng::seeded(1),
    };

    let stage = ClaimsNormalize::new(
        template("claims.group", "{{claims}}", &["claims"]),
        (ModelId::new("model-a"), ProviderId::new("mock")),
        Cost(0.01),
    );

    let out = stage.run(ExtractedClaims(claims), &c).await.unwrap();

    assert_eq!(
        events.count(EventType::CallStarted),
        4,
        "3 batch calls + 1 stitch call: partitioning must actually happen, not collapse to one call"
    );
    assert_eq!(
        out.0.len(),
        2,
        "3 batch representatives, 2 of which the stitch pass merges together"
    );
    let total_members: usize = out.0.iter().map(|c| c.members.len()).sum();
    assert_eq!(
        total_members, 180,
        "every original claim must still be present as a member somewhere: none lost"
    );
}
