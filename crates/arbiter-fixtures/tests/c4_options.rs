//! F2 — option identity, versioning and attachment (C4), ARCHITECTURE §18's
//! CI suite.

use arbiter_core::decision::attachment::{AttachSource, Attachment, AttachmentMatrix, Polarity};
use arbiter_core::{ClaimId, DecisionOption, OptionId};
use std::collections::BTreeMap;

/// `option_supersede`: "rebuttal refines a recommendation → lineage head
/// moves, cells follow." Two distinct refinements, proven distinctly:
/// a *reword* keeps the option's own `id` (attachment cells, keyed on that
/// id, need no migration at all -- they "follow" simply because the key
/// never changed), while a *material* supersession mints a new `id` whose
/// own `supersedes` pointer is how a reader walks the lineage back to find
/// what it replaced -- the "head" of the lineage moves to the new id, and
/// nothing in the attachment matrix does that walk automatically.
#[test]
fn option_supersede() {
    let claim = ClaimId::new("claim_1");
    let original = DecisionOption::new(OptionId::new("opt_monolith"), "Adopt a modular monolith");

    let mut matrix = AttachmentMatrix::default();
    matrix.cells.insert(
        (claim.clone(), original.id.clone()),
        Attachment {
            polarity: Polarity::Supports,
            confidence: 0.9,
            source: AttachSource::Authored,
        },
    );

    // A reword: same id, new version -- the existing cell is keyed on the id,
    // so it is found under the reworded option without moving anything.
    let reworded = original.reworded("Adopt a modular monolith, split by bounded context");
    assert_eq!(
        reworded.id, original.id,
        "a reword must keep the same option id"
    );
    assert_ne!(
        reworded.version, original.version,
        "a reword must mint a new version"
    );
    assert!(
        matrix.get(&claim, &reworded.id).is_some(),
        "the existing cell must still be found under the reworded option's id -- it never moved"
    );

    // A rebuttal proposes a materially different course of action: this
    // mints a genuinely new id, superseding the reworded option.
    let superseding = reworded.superseding(
        OptionId::new("opt_microservices"),
        "Adopt full microservices",
    );
    assert_ne!(
        superseding.id, reworded.id,
        "a material change must mint a new id"
    );
    assert_eq!(
        superseding.supersedes,
        Some((reworded.id.clone(), reworded.version.clone())),
        "the new option's lineage pointer must name exactly the version it replaced -- \
         this is how a reader walks the lineage head back to what it superseded"
    );
    assert!(
        matrix.get(&claim, &superseding.id).is_none(),
        "a superseding option's own id is genuinely new -- attachment does not \
         auto-migrate a cell across a material change, only a reword"
    );

    // The lineage as a whole: three options, only the last of which is the
    // current head (the first two would be retired by whichever caller
    // decided the supersession -- DecisionOption itself only records the
    // pointer, per its own doc comment on `retired`).
    let lineage: BTreeMap<OptionId, &DecisionOption> = [
        (original.id.clone(), &original),
        (reworded.id.clone(), &reworded),
        (superseding.id.clone(), &superseding),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        lineage.len(),
        2,
        "reword shares an id with the original, so the map collapses to two entries"
    );
    assert!(
        lineage.contains_key(&superseding.id),
        "the current head is reachable by its own id"
    );
}
