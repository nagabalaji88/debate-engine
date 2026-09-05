# Plan deviations

Per `IMPLEMENTATION_PLAN.md` §0.1: when this plan and the spec disagree, the spec wins
and the plan has a bug. Logged here, in execution order.

---

## D1 — kernel/store dependency direction (task X1)

**What the plan's own task table implied.** `IMPLEMENTATION_PLAN.md` §2 listed
`K1 · Budget ledger` as depending on `S3 · Event append, hash chain`, and K2/K3/K5
similarly depend on S-tasks. Read naively, that puts `arbiter-store` upstream of
`arbiter-kernel` in the crate graph.

**What ARCHITECTURE.md §4.1 actually says.** *"kernel depends on core; everything else
depends on kernel."* And the crate table is explicit about the direction: `arbiter-kernel`
owns "event store" (i.e. the trait); `arbiter-store` is "the SQLite **implementation** of
the Store traits." Store depends on kernel, never the reverse — kernel must not know
SQLite exists, so a second `Store` implementation (or a test double) never has to touch
`arbiter-store` to exist.

**Caught by:** attempting the X1 scaffold with `arbiter-kernel` depending on
`arbiter-store`, which would have made X2's dependency-rule test fail on first write.

**Fix applied in X1:**
- `arbiter-kernel/Cargo.toml` depends on `arbiter-core` only.
- `arbiter-store/Cargo.toml` depends on `arbiter-core` **and** `arbiter-kernel`.
- The `RunStore` / `RunWriter` / `Tx` / `RunReader` trait signatures (INTERFACES §1) are
  defined **in `arbiter-kernel`**, not in `arbiter-store`. `arbiter-store` provides the
  SQLite-backed implementation.

**Consequence for the task table:** read every `K* depends on S*` edge in §2 as backwards.
The correct direction is: kernel tasks that need a store define the trait they need
in-crate (no external dependency); store tasks then implement that trait and therefore
depend on the kernel task that defined it. When executing K1/K2/K3/K5, do not add
`arbiter-store` as a dependency to satisfy them — write the trait, or a temporary
in-memory test double, inside `arbiter-kernel` itself, and let `arbiter-store` catch up.

---

## D2 — `wasmtime` not yet a workspace dependency

`ARCHITECTURE.md` §16 names `wasmtime` for the WASM plugin host. That host is explicitly
1.5 (`IMPLEMENTATION_PLAN.md` §9, "explicitly out of this plan"). Adding the dependency
now, unused, would slow every `cargo build`/`clippy` invocation for the rest of 1.0 for no
present benefit — it violates §0.2 rule 2 ("do not widen scope"). Add it when the WASM
host task actually starts, not before.

---

## D3 — C1 over-scoped: Build Studio provenance is not a core type

**What the plan said.** C1 listed `provenance.rs` with `ProvenanceChain` and five gates
(`Unattributed`, `OrphanInference`, `ChainTooDeep`, `CitesDefeatedClaim`,
`TooMuchInvention`), citing "INTERFACES §13" as if it were core decision machinery.

**What the spec actually says.** Those five gates and the `Provenance` enum
(`UserOverride` / `ExternalSource` / `ArchitectInference`) are **Build Studio's**
mechanism for validating generated build documents (ARCHITECTURE §13, INTERFACES §7,
"Build Studio provenance, made mechanical") — they operate on `BuildDoc` and
`ArchitectInference`, concepts that don't exist anywhere in the claim/relation/option
graph. Build Studio is explicitly 1.5 and out of this plan's scope (§9).

**Fix:** dropped `provenance.rs` from C1 entirely. It belongs to a future `arbiter-build`
task, not written here.

## D4 — `supersedes` belongs on `DecisionOption`, not `ClaimMember`

**What the plan said.** C1 listed `supersedes: Option<ClaimId>` as a `claim.rs` addition.

**What the spec says** (ARCHITECTURE §5.3, INTERFACES §20 Step 3b): `supersedes:
Option<(OptionId, OptionVersion)>` is a field on `DecisionOption`. Claim versioning is
already fully handled by the existing `ClaimLifecycle::Modified { version }` — a second,
claim-level `supersedes` would be a redundant, spec-less mechanism.

**Fix:** moved to C4's `option.rs` rework, where it belongs.

## D5 — C1's constant list mixed core, kernel and store ownership

**What the plan said.** C1 told `config.rs` to grow nine fields: `option_floor`,
`tau_gap`, `tau_dissent`, `converged_margin_factor`, `min_new_claims`,
`min_standing_delta`, `budget_headroom`, `repair_budget_fraction`, `blob_threshold`.

**What's actually core.** Checked each against what C2–C8 (the pure decision functions)
consume:

| Constant | Consumed by | Owner |
|---|---|---|
| `option_floor` | C5 outcome classification | **core** — genuinely missing, added |
| τ_gap | C5 outcome classification | **already exists**, as `Thresholds::min_margin` |
| τ_dissent | C5 outcome classification | **already exists**, as `Thresholds::dissent` |
| `converged_margin_factor`, `min_new_claims`, `min_standing_delta` | the controller's stop predicates (§5.5) — a kernel stage (G7), not a `decision.synthesize`-time computation | kernel |
| `budget_headroom`, `repair_budget_fraction` | the budget ledger (§7, §11) | kernel |
| `blob_threshold` | the blob store (§8.2) | store |

**Fix:** C1 adds only `option_floor`. Two existing fields (`min_margin`, `dissent`) get a
doc comment cross-referencing the spec's Greek-letter names rather than being renamed —
renaming a correct, tested field to satisfy cosmetic symmetry would be scope creep in the
other direction. The kernel/store constants stay in `IMPLEMENTATION_PLAN.md` §0.6 as a
system-wide reference table (that table's job), but are added to code by K1/K4/S5, not C1.

## D6 — `independence()` needs correlation groups, not provider counts

Not an error in the plan, but a real upgrade the plan's C1 acceptance criteria missed
stating explicitly. `decision/evidence.rs::independence()` computes the exact formula
INTERFACES §15 specifies — `(groups + λ·(members − groups)) / members` — but currently
treats `ProviderId` as the partition, which is only the *default*. The spec requires an
explicit, config-overridable `correlation_group: GroupId` field on `ClaimMember`
(INTERFACES §15), because "two vendors serving the same base weights are correlated and
should share a group" and provider identity alone systematically overstates independence
(§6.2). Fixed in C1: `correlation_group` added to `ClaimMember`; `independence()`
repartitions on it. `corroboration()` is unaffected — the spec still defines it over
distinct providers, not correlation groups.

---

## D7 — C2's own formula sketch capped in the wrong order

**What the plan said** (§2, C2 section): `support = min(Σ α·standing(s), support_cap)`,
`attack = min(Σ β·standing(a), attack_cap)` — weighting *inside* the cap.

**What ARCHITECTURE §6.3 actually says:** the cap applies to the **raw, unweighted**
sum, and α/β are applied *after*:

```
attack_term  = β · min(Σ w·standing(a), attack_cap)
support_term = α · min(Σ w·standing(s), support_cap)
```

**Confirmed to actually diverge**, not just cosmetically different — with three
full-strength supporters (raw = 3.0, α = 0.25, `support_cap` = 2.0):

```
spec (cap raw, then weight):    0.25 * min(3.0, 2.0) = 0.5
plan's sketch (weight, then cap): min(0.25*3.0, 2.0) = 0.75
```

The plan's version needs a raw sum of `cap / gain` (6.0 for attack, 8.0 for support)
before the cap ever engages, instead of `cap` (1.5 / 2.0) — the cap would almost never
fire in practice under the wrong reading.

**Caught by:** implementing C2 against ARCHITECTURE §6.3 directly (not the plan's own
shorthand) and checking the result against the spec's four worked examples, which only
match under the cap-the-raw-sum reading.

**Fix:** `decision/fixpoint.rs` implements the correct order. `IMPLEMENTATION_PLAN.md`'s
C2 section is corrected to match. Anyone re-deriving this from the plan's own
(now-fixed) text rather than the spec directly would have gotten the right answer
either way.

Also corrected in the same pass: the plan's sketch signature (`solve(graph: &ClaimGraph,
…, p: &Policy)`) referenced a `ClaimGraph` type that doesn't exist anywhere in the spec
or codebase. The actual signature takes `claim_ids: &[ClaimId]` and `relations:
&[Relation]` directly — the natural inputs the function needs, nothing invented — and
`&GraphParams` rather than the wider `&Policy`, since the fixpoint uses only the graph
constants, not the policy's other threshold groups. `FixpointResult` also gained a
`max_delta: f64` field the sketch omitted, needed for the `FIXPOINT_NOT_CONVERGED{
max_delta, iterations }` event payload §6.3 names.

---

## D8 — C3: two gaps in §6.4's standing rules, resolved conservatively

**Gap 1 — "never resolved by challenge" is not defined further anywhere in either
spec file** (checked both; only the one line in §6.4 mentions it). Taken as: a claim's
`ClaimLifecycle` represents a challenge having concluded with an outcome only when it
is `Defended` or `Modified{_}` — both are the product of surviving cross-examination,
one unchanged, one revised. `Proposed`, `Verified` and `Challenged` are all "not yet":
never tested, or tested but no outcome recorded yet. `Withdrawn`/`Rejected` never reach
this check — `Defeated` is evaluated first and claims them.

**Gap 2 — the four listed rules are not jointly exhaustive**, though `ClaimStanding` is
a closed four-variant enum that must classify every claim. A claim with standing in
`[0.15, 0.50)`, no live attacker, and a kind other than the effective `Unverified`
(e.g. a middling `Assumption` nobody has contradicted) matches **none** of the four
literal conditions: not below the Defeated floor, no live attacker so not Disputed,
not Unverified so not Unresolved by the letter of the rule, below 0.50 so not Agreed.

Resolved conservatively, per §0.4 ("the reading that … refuses rather than proceeds"):
this residual band classifies as **Unresolved**. The alternative — silently promoting
it to `Agreed` — would overstate settledness for a claim that never cleared the 0.50
bar, which is a strictly worse failure than an honest "not yet resolved." Reusing
`Disputed` was considered and rejected: that variant specifically means an identified
opponent exists at live-attacker strength, and labelling a claim with **no** attacker
as Disputed in `explain` output would misrepresent it.

Both choices are implemented in `decision/standing.rs` and covered by dedicated tests
(`resolved_by_challenge_requires_defended_or_modified`,
`the_residual_band_falls_to_unresolved_not_agreed_or_disputed`).

---

## D9 — C4's plan sketch invented a simpler `Attachment` type than the spec's

**What the plan said:** `(OptionId, ClaimId) -> Attachment { Supports(f64) | Opposes(f64)
| None }` — an enum carrying just a polarity and a strength.

**What INTERFACES §20 actually defines:**

```rust
pub struct Attachment { pub polarity: Polarity, pub confidence: f64, pub source: AttachSource }
pub enum Polarity     { Supports, Opposes, Neutral }
pub enum AttachSource { Authored, Classified, Propagated }
```

The plan's sketch drops `source` entirely — which loses exactly the distinction Step 2/3
exist to record: whether a cell came from the position's own recommendation
(`Authored`), the batched classifier call (`Classified`), or deterministic propagation
through the relation graph (`Propagated`). An `explain` view that cannot say *why* a
claim attaches to an option the way it does is missing the point of Step 3's design
note: "the classifier only has to see direct attachment: the graph does the rest, and
it does it identically on replay" — that guarantee is only checkable if `Propagated`
cells are distinguishable from the others.

**Fix:** implemented the real three-field `Attachment` / `Polarity` / `AttachSource`
types verbatim, not the plan's simplified sketch.

## D10 — option share normalization when `raw` can be negative

`ARCHITECTURE.md §6.5` gives `raw = Σ standing(supporting) − 0.5·Σ standing(opposing)`
and says it is "normalised to `share ∈ [0,1]`" — but doesn't say how, and `raw` can be
negative (an option with more opposition than support). Neither spec file's worked
JSON example shows more than one option's `raw`/`share` pair, so there's nothing to
reverse-engineer a convention from.

**Resolved:** clamp `raw` at 0 before normalizing — a net-opposed option contributing a
*negative* probability mass to the others' shares has no dialectical meaning — then
`share_i = max(raw_i, 0) / Σ max(raw_j, 0)`. When every option's clamped `raw` is 0 (no
option has net-positive support at all), **every share is 0**, not NaN from a
divide-by-zero and not an even split — an even split would manufacture confidence that
doesn't exist. This correctly cascades into `option_floor` failing for every option,
which is what routes the outcome to `INSUFFICIENT_EVIDENCE` in C5 rather than a false
`SPLIT_DECISION`. Shares sum to 1.0 **only in the non-degenerate case**; the plan's own
C4 acceptance criterion ("shares sum to 1.0 within 1e-9") is corrected to say so.

## D11 — attachment propagation: no rule given for propagating from an `Opposes` cell

INTERFACES §20 Step 3 gives exactly three rules, and all three have the same base case
— `s supports O`:

```
c contradicts s ∧ s supports O   →  c opposes O
c supports    s ∧ s supports O   →  c supports O
c qualifies   s ∧ s supports O   →  c opposes O at γ weight
```

Nothing says what happens when `s opposes O` instead. A symmetric extension is easy to
guess (flip the inferred polarity) but the spec does not state it, and guessing a
bidirectional propagation rule the authors may have deliberately left narrower is
exactly the kind of invention §0.2 rule 1 forbids.

**Resolved conservatively:** propagation implemented for **exactly the three stated
rules**, base case `s supports O` only. A claim related to a claim that *opposes* an
option is left unattached by propagation (though it may still carry an `Authored` or
`Classified` cell from Steps 1–2, which this gap does not affect). This under-populates
the matrix relative to a symmetric reading, which is the conservative direction — an
absent cell is a claim `explain` shows no opinion on, not a wrong one.

If a future spec revision adds the `Opposes` base case, `decision/attachment.rs`'s
`propagate` function is where it goes — the three existing rules are written as a
small match table specifically so a fourth entry is a one-line addition, not a rewrite.

## D12 — `truncation_factor` missing from the plan's own load-bearing constants table

IMPLEMENTATION_PLAN.md §0.6 lists every number that must appear as a named constant,
not a literal — and ARCHITECTURE §6.6 rule 1 needs one it never listed:

```
INSUFFICIENT_EVIDENCE   evidence_mass < τ_min × truncation_factor
```

INTERFACES §9 gives the value: "the evidence-mass floor is raised by `truncation_factor`
(default ×1.2)". §0.6's table has a row for the *penalty* (`0.10 truncation`, §6.7) but
none for this multiplier, so C5 would have hard-coded `1.2` inline had this not been
caught while reading §6.6 against the plan's own constant list before coding.

**Resolved:** added `Thresholds::truncation_factor: f64 = 1.2` (config.rs), doc-commented
to INTERFACES §9, pinned by a test alongside the other `Thresholds` constants. §0.6's
table is corrected to add the missing row.

## D13 — `score(top1)` / `margin(top1, top2)` in §6.6: raw or share?

ARCHITECTURE §6.6 writes `score(top1)`, `score(top2)` and `margin(top1, top2)` without
saying which `OptionScore` field they read. Two candidates exist post-C4:
`raw` (`Σstanding(supporting) − 0.5·Σstanding(opposing)`, unbounded) and `share` (`raw`
normalised across options, sums to 1.0 in the non-degenerate case, D10).

§6.7 defines `decision_margin` — the same word, same top1/top2 pairing — explicitly as
`share(top1) − share(top2)`. Reusing "margin" for a differently-scaled quantity three
sections later, with `option_floor = 0.20` compared against it as if it were a
probability-like share, would be an inconsistent spec; reading both as `share` keeps
`option_floor` and `τ_gap` meaningful as fractions of the same normalised total both
places, and keeps one definition of "margin" in the whole document.

**Resolved:** `outcome::classify` reads `OptionScore.share` for every comparison against
`option_floor` and `τ_gap` in §6.6 rules 1–3. If a future spec revision states `raw` was
intended, this is the one function to change.

## D14 — `ConfidenceWeights` (C1) only carried two of the five penalty coefficients

C1 added `ConfidenceWeights{evidence_mass, margin, judge, unresolved_penalty,
assumption_penalty}` — the three dimension weights plus exactly the two penalties C1
happened to test against. But IMPLEMENTATION_PLAN.md's own §0.6 constants table (row
"penalties") already named all five: `0.25 unresolved · 0.15 assumption · 0.10
truncation · 0.05 convergence · 0.20 dispersion`, plus a separate row for the 0.15
dispersion threshold. `truncation_penalty`, `convergence_penalty`, `dispersion_weight`
and `dispersion_threshold` were simply never added to the struct — a gap invisible
until C6 became the first task to actually evaluate the formula that needs them.

**Resolved:** extended `ConfidenceWeights` with the four missing fields, defaults
1.2×-consistent with §0.6 (`truncation_penalty: 0.10`, `convergence_penalty: 0.05`,
`dispersion_weight: 0.20`, `dispersion_threshold: 0.15`), pinned by a new test
alongside the existing dimension-weight-sum test.

## D15 — `judge_score` and `judge_dispersion` for `judge_count > 1`: aggregation left unstated

ARCHITECTURE §6.7 and INTERFACES §14 define `judge_score` as "weighted 9-metric
rubric" (singular) and `judge_dispersion` as "the spread of weighted scores over the
same anonymised dossier" — both assume a single number per run, but neither states
how multiple judges' individual `Scorecard::weighted()` values combine into that one
`judge_score` when `judge_count > 1`. The dispersion table (§14) only shows the
*penalty*, derived from the gap between two judges, never the base term those same
two judges would produce.

**Resolved:** `judge_score` = arithmetic mean of `Scorecard::weighted()` across all
supplied judges (0.0 for zero judges, an edge case that cannot occur once `arbiter`
requires at least one judge, but the function does not panic on it). This is the only
aggregate consistent with `judge_dispersion` being defined as *spread around* a
central value from the same set. If a future spec revision states a different
aggregation (e.g. min, to be conservative about weak judges), `confidence()` is the
one function to change.

## D16 — plan's C7 sketch scoped candidates to "unresolved", spec says "unresolved or disputed"

IMPLEMENTATION_PLAN.md's C7 section read "for each **unresolved** claim, flip it,
re-run the fixpoint" — but ARCHITECTURE §6.8 is explicit: "For each **unresolved or
disputed** claim, pin its standing to the opposite extreme and recompute." Disputed
claims (a live attacker present despite possibly-high standing, per C3's §6.4
classification) are exactly the claims a counterfactual pass should also probe — a
disputed claim's standing could plausibly have gone the other way had the live
attacker been more or less credible, which is precisely the kind of assumption
`change_triggers` exists to surface.

**Resolved:** `counterfactual_flips` takes `candidates: &[ClaimId]` as a plain input
rather than hard-coding a standing-class filter (consistent with C5/C6's convention
of taking pre-classified inputs, D12) — but the plan's own C7 section is corrected to
say the caller must pass **both** `Disputed` and `Unresolved` claims from
`standing::classify_all`, not `Unresolved` alone.

## D17 — "the opposite extreme": one flip per claim, not both

ARCHITECTURE §6.8 says "pin its standing to **the opposite extreme**" — singular,
definite article — which reads as one deterministic flip per claim, not a test of
both 0.0 and 1.0. Two things independently confirm this rather than the "test both
directions" alternative:

- INTERFACES §21 sizes the reused pass at "~32 runs of a 64-iteration loop... for ~32
  candidate claims" — one fixpoint solve per claim, not two.
- The `DecisionRecord.change_triggers` example (§6.9) carries a single `"direction":
  "if_true"` per entry, not a pair of results.

**Resolved:** the extreme tested is the one *opposite the claim's current baseline
lean* — standing ≥ 0.5 is tested `IfFalse` (pinned to 0.0), standing < 0.5 is tested
`IfTrue` (pinned to 1.0). If a future spec revision wants both directions probed
per claim, `counterfactual_flips` is the one function to change — it would return two
entries per candidate instead of one.

## D18 — `DecisionRecord` (C8) omits fields this crate has no rule to compute

ARCHITECTURE §6.9's full `DecisionRecord` JSON also includes `model_agreement`,
`dissent`, `assumptions`, `acceptance` and `completeness`. None of these has a
formula or a fully-specified type this pure decision core has been given:

- `model_agreement: {aligned, total}` needs the raw per-model vote tally — a
  reporting-only field explicitly "never an input" (§6.9), so it is produced by
  whatever stage tallies model positions, not derived from claim/option state.
- `dissent` needs each entry's `risk_awareness`, a per-claim judge assessment
  joined against the dissenting claim — no rule anywhere ties a judge's rubric
  score to a specific claim; `Scorecard` scores a whole debate submission, not
  individual claims.
- `assumptions` needs a `decision_impact: "high"|"medium"|"low"` classification —
  no threshold or formula for this exists in either spec file.
- `completeness` needs `Completeness::Truncated{reason: StopReason, missing_stages:
  Vec<StageName>}` — already deferred at D12; C5/C6 took a plain `truncated: bool`
  instead, and C8 does the same rather than inventing `StopReason`/`StageName` now.
- `acceptance` is `null` until `arbiter accept` runs (§6.9's own comment) — a later
  CLI command (L4), not a field this crate ever populates.

Inventing shapes for any of these to make C8 "complete" would be exactly the kind
of guess §0.2 rule 1 forbids — none of them can be tested against a spec-given
worked example, unlike every other field `DecisionRecord` carries.

**Resolved:** `DecisionRecord` ships the fields the spec gives a concrete formula
or type for: `schema_version`, `run_id`, `policy_version`, `question`, `outcome`
(C5), `recommendation` (derived from `options`), `confidence` (C6, via
`explain_confidence`), `options` (C4), `claims` counts and `unresolved_claims`
(C3's classifications, counted), `change_triggers` (C7, triggering flips only),
plus `engine_version`/`inputs_hash` as opaque caller-supplied strings. The five
omitted fields wait for `G9 decision.synthesize` — the kernel stage that actually
has model tallies, judge-to-claim joins, and `Completeness` in hand — which is why
the task graph has `G9` depend on `C8` rather than the reverse.

## D19 — K0's supporting types have no concrete Rust definition anywhere in either spec file

INTERFACES §1's `RunStore`/`RunWriter`/`Tx`/`RunReader` block references thirteen
names — `Event`, `Sequence`, `Manifest`, `StoreError`, `Artifact`, `ArtifactId`,
`CacheKey`, `CachedResponse`, `ReservationId`, `Cost`, `CallId`, `CallState`,
`ChainStatus` — and not one of them is given its own `struct`/`enum` code block in
either ARCHITECTURE.md or INTERFACES.md. A full-file research pass (both files read
in full) confirmed: every Rust type definition in the project lives in
docs/INTERFACES.md, but these thirteen appear *only* as parameter/return types
inside that one trait block. Their shapes exist only as JSON examples, SQL
`INSERT`/`UPDATE` statements, prose, and (for `CallState`) a transition diagram —
never as a `pub struct`/`pub enum`.

K0's task file scope is "define the trait signatures only... copy `RunStore` /
`RunWriter` / `Tx` / `RunReader` from INTERFACES §1 verbatim" — but verbatim
signatures reference types that do not exist yet anywhere in the workspace. Writing
`K0` without authoring these thirteen supporting types is not possible; leaving them
unauthored is not "minimal scope", it is a crate that does not compile.

**Resolved**, one per type, each anchored to the most specific spec text available
rather than invented freely (full field-level detail in each type's own doc comment
in `arbiter-kernel/src/{ids,event,provider,store}.rs`):

- `Event` — ARCHITECTURE §9's JSON envelope example gives the complete field list
  (`schema_version, event_id, run_id, sequence, timestamp, stage, event_type,
  durable, payload, content_hash, previous_event_hash`); transcribed directly, no
  invention needed beyond choosing Rust types for each JSON field.
- `EventType` — INTERFACES §13 gives this **exactly**, copied verbatim, byte for
  byte, down to each family's grouping comment.
- `Sequence` — `seq INTEGER PRIMARY KEY` (ARCHITECTURE §8.1/§8.7) pins this to an
  integer newtype; no invention.
- `CallState` — ARCHITECTURE §8.4's transition diagram and table name the exact
  8-variant set (`RESERVED, SENT, ACKNOWLEDGED, COMPLETED, RETRYABLE, FAILED,
  ORPHANED, RECOVERED`) even though no code block exists; transcribed as an enum.
- `ProviderCapabilities` / `IdempotencyStyle` — INTERFACES §5 gives these
  **exactly**, copied verbatim (with one practicality substitution: `Header(&'static
  str)` → `Header(String)`, since a borrowed `'static` field cannot derive
  `Deserialize` in general and every adapter only ever constructs this from a string
  literal regardless).
- `ArtifactId`, `ReservationId`, `CallId`, `EventId` — no shape given at all beyond
  "an identifier"; modelled as opaque string newtypes, matching the one convention
  the rest of the workspace already has for exactly this (`arbiter-core::ids`'s
  `id_type!` macro) rather than inventing a new pattern.
- `StageName` — needed because `Event.stage` and `Stage::name`/`PromptTemplate::stage`
  (INTERFACES §6, §23) all reference it with no definition; same opaque-string
  treatment. The G-tasks that define the 15 concrete stages own the actual values.
- `ChainStatus` — no enum given; inferred minimally (`Intact` / `Broken { at:
  Sequence }`) from "a chain break... is not repairable... the event records
  detection" (ARCHITECTURE §9) and the `ChainBreakDetected` event variant — the
  smallest shape consistent with what `verify_chain` is described as detecting.
- `Manifest` — no struct given; every field is individually named in prose across
  ARCHITECTURE §7/§15 ("recorded in the manifest", "frozen by `init`") as
  `policy_version`, `config_hash`, `pack_hash`, `correlation_table_version`, and an
  `rng` seed — assembled into one struct for the first time here.
- `StoreError` — only one variant is spec-named (`AlreadyOpen`); rather than invent
  a full failure taxonomy no real implementation has exercised yet, this ships
  `AlreadyOpen` plus a single `Other(String)` escape hatch, explicitly deferring the
  rest to whichever `S`-task (S2+) first needs a specific new variant.
- `Cost` — a bare `f64` newtype; the spec never states money's representation
  beyond calling every ledger quantity a plain number.
- `CacheKey` — the four-field tuple `(provider, model, params, prompt_hash)` is
  given exactly in prose (INTERFACES §5); `params`' own type is never specified
  anywhere in the workspace, so it holds the call parameters' canonical serialized
  string rather than a structured type that does not exist yet.
- `CachedResponse` — inferred minimally from ARCHITECTURE §8.2's blob-threshold
  description (`response_hash`, `size_bytes`, `inline: Option<String>` — `None`
  exactly when the payload lives in the blob store instead).
- `Artifact` — INTERFACES §1 uses it as a concrete type (`&Artifact` in
  `Tx::put_artifact`) while INTERFACES §6 uses it as a trait bound (`type In:
  Artifact` on `Stage`) — an internal inconsistency between the two sections.
  Resolved as a trait (`&dyn Artifact`, matching §6's usage and INTERFACES §6's own
  prose: "content-addressed, `serde`-typed, and versioned"), because a `Tx` capable
  of storing heterogeneous stage outputs behind one method needs a trait object,
  not one concrete struct — and, separately, `Tx` is used as `&mut dyn Tx`
  elsewhere in the same block, so every one of its methods must be object-safe
  regardless.

## D20 — `RunWriter::transact<T>` cannot compile as written: a generic method on a trait used as `Box<dyn RunWriter>`

INTERFACES §1's `RunWriter` trait, copied verbatim:
```rust
pub trait RunWriter: Send {
    fn transact<T>(
        &mut self,
        f: &mut dyn FnMut(&mut dyn Tx) -> Result<T, StoreError>,
    ) -> Result<T, StoreError>;
}
```
is used exclusively as a trait object — every `RunStore` method that produces one
returns `Box<dyn RunWriter>` (`create`, `reopen`). This is not implementable: Rust
does not allow a trait with a generic method to be made into a trait object at all
(the vtable has no slot for an unbounded number of monomorphizations), regardless of
whether that method is ever actually called through the `dyn` reference. The spec's
own two requirements — `RunWriter` is generic over `T`, and `RunWriter` is always
handled as `Box<dyn RunWriter>` — are mutually exclusive in the language the rest of
the spec is written in.

**Resolved:** dropped the generic return. `transact` now returns `Result<(),
StoreError>`, and a caller that needs a value out of the transaction captures it
from inside the closure (a `let mut captured = None;` before the call, written
inside the closure body, read after) rather than receiving it as the method's return
value. This preserves the actual guarantee INTERFACES §1 cares about — "everything
inside the closure commits, or none of it does" — while making the trait
constructible as `Box<dyn RunWriter>` at all. A unit test in `store.rs`
(`transact_lets_a_caller_extract_a_value_via_closure_capture`) proves the pattern
works end to end against an in-memory `Tx`/`RunWriter` pair built purely from these
trait definitions.

## D21 — S1 scoped down to the 3 tables (+ history.db's `run_catalog`) the spec actually gives columns for

ARCHITECTURE §8.1's table lists ~15 more projection tables beyond `events` and
`schema_metadata`: `run`, `stages`, `provider_calls`, `budget`, `positions`,
`claims`, `claim_relations`, `disputes`, `challenges`, `rebuttals`,
`judge_evaluations`, `decision`, `decision_triggers`, `provenance`,
`cache_entries`, `artifacts`. It names every one of them — status (source of
truth / projection) and rebuild policy (never / replay / migrations) — but gives
**column definitions for none of them**. The only tables with a complete,
spec-given shape anywhere in either file are:

- `events` — ARCHITECTURE §9's JSON envelope example, field-for-field.
- `run` — INTERFACES §1's literal `INSERT`/`UPDATE` statements (D19 already
  established this same column list for the `Manifest`/lease-CAS discussion).
- `schema_metadata` — ARCHITECTURE §8.7's explicit prose column list.
- `run_catalog` (in `history.db`) — ARCHITECTURE §8.5's `CREATE TABLE`, verbatim,
  the only fully-given `CREATE TABLE` statement in either document.

Two more (`budget`, `provider_calls`) have *some* columns nameable from scattered
SQL fragments in §5/§8.3/§8.4 (`reserved`, `committed`, `state`,
`reserved_amount`, ...) but not a complete, confident list — and `cache_entries`/
`artifacts` have only the K0-authored `CacheKey`/`CachedResponse`/`Artifact`
shapes (themselves already D19-flagged as inferred, not spec-given) to go on. The
remaining ~9 (`stages`, `positions`, `claims`, `claim_relations`, `disputes`,
`challenges`, `rebuttals`, `judge_evaluations`, `decision`, `decision_triggers`,
`provenance`) have **zero** column information anywhere in either file.

Authoring ~15 table schemas from a names-only list would be invention at a scale
this project's own discipline (§0.2 rule 1: never invent, log the gap) does not
tolerate for something this load-bearing — a wrong column now means every later
task that reads/writes these tables inherits the mistake, and there is no worked
example anywhere to test a guess against.

**Resolved:** `migrations/0001_initial.sql` creates exactly `events`, `run`,
`schema_metadata`. `schema.rs` separately applies `history.db`'s `run_catalog` (+
its own `schema_metadata`) as a Rust constant rather than a second migrations
directory, since the plan's own file list for S1 names only one migration file.
`budget`, `provider_calls`, `cache_entries` and `artifacts` are deferred to K1
(budget ledger), K2 (provider-call state machine) and K5 (response cache) — the
tasks that actually read and write them, and so are best positioned to pin exact
columns against real code rather than a schema built in isolation ahead of its own
users. The 9-table debate/decision projection group is deferred to S4
("Projections + rebuild-from-events"), which the task graph already has depending
on C8 (done) and K3 (StageGraph, not yet built) — i.e. it was never meant to
happen before the Rust types being projected exist to project from.

## D22 — "content_hash = blake3(canonical payload)": whole-event content, not just the `payload` field

ARCHITECTURE §9 writes `content_hash = blake3(canonical payload)`. `Event` (K0,
D19) has a field literally named `payload: serde_json::Value` — read narrowly,
"canonical payload" could mean "hash only that one field". That reading fails the
stated purpose: ARCHITECTURE §9 also says a chain break is how "someone edited the
database directly" is detected, and a hash that only covers the `payload` column
would not notice `event_type`, `stage`, `durable`, `timestamp`, `run_id`, or
`schema_version` being altered with the payload bytes left untouched — exactly the
kind of tampering `verify_chain`/`ChainBreakDetected` exists to catch.

**Resolved:** `content_hash` covers the event's whole content — every field except
the two hash fields themselves (self-referential) and `sequence` (DB-assigned,
not known until after the row is inserted, and not part of what "this event says"
semantically). `previous_event_hash` is *not* mixed into the computation of this
row's own `content_hash` — it is simply the prior row's `content_hash`, carried
forward unchanged — so `content_hash` stays a pure function of one event and can
be computed entirely before the store round-trip assigns `seq`.

A second, purely mechanical pitfall surfaced while wiring this up and is worth
naming for anyone touching `events.rs` later: the string hashed for `event_type`
must be **exactly** the same string `Tx::append_event` writes into the column
(`serde_json::to_string(&e.event_type)`, the quoted JSON form, e.g. `"CLAIM_
EXTRACTED"` with its quotes) — an early draft hashed a trimmed/unquoted form at
write time while `verify_chain` read the quoted form back from the column, so
every freshly-appended event failed its own verification despite never having
been tampered with. Fixed by hashing the identical stored string on both sides;
`canonical_content_hash`'s doc comment now states this explicitly as the
invariant to preserve.

`verify_chain`'s own hash recomputation, and appending an event whose
`event_type` this binary does not compile a variant for, both operate on
[`arbiter-store/src/events.rs`]'s `RawEventRow` / `HashableContent` — raw strings,
never the typed `EventType`/`serde_json::Value` — precisely so a row from a newer
binary (an `event_type` this one has never heard of) is still hashable and
chain-verifiable, matching INTERFACES §13's forward-compatibility promise:
`RunReader::events()`'s typed view *skips* such a row, but `verify_chain` still
accounts for it.

## D23 — the stage idempotency key: two spec files describe different axes

INTERFACES §5 gives the idempotency key as a literal formula: `blake3(stage_name ‖
engine_version ‖ config_hash ‖ round ‖ input_artifact_hashes)`. ARCHITECTURE §7
describes the same concept differently: "idempotency key per `(run_id, stage,
input_hash)`" — and argues explicitly that `policy_version`/`pack_hash`/
`table_version`/`config_hash` do **not** need to be part of it at all, since
they're frozen by `init` and constant within a run, and `run_id` already prevents
a stage from colliding across runs. The two lists disagree on both ends:
INTERFACES's formula has no `run_id` term; ARCHITECTURE's tuple has no
`engine_version`/`config_hash` (and doesn't explicitly mention `round`, though
§6's re-entrant round-subgraph design separately requires round to be in *some*
per-round key).

Neither text is obviously wrong on its own — ARCHITECTURE's reasoning ("these are
constants within a run, so they add nothing") is sound as an argument for why they
*don't need* to be included, which is different from a claim that INTERFACES's
formula is incorrect to include them.

**Resolved:** `idempotency_key()` (`arbiter-kernel/src/stage.rs`) hashes the union
of both — `stage_name`, `run_id`, `engine_version`, `config_hash`, `round`, and
`input_artifact_hashes` — joined with a U+0001 separator so no two distinct input
sequences can concatenate to the same string. Hashing a few extra, already-
constant-within-a-run values cannot cause a *false* match (a false idempotency hit
that wrongly skips real work); it can, at most, make the key strictly more
specific than INTERFACES's own minimal formula requires, which is the safer
direction to err in given the two texts disagree. If a future spec revision picks
one list explicitly, this is the one function to narrow.

## D24 — K3's remaining undefined `StageContext` field types

`Stage`/`StageContext` (INTERFACES §6) reference four more types with no
definition anywhere in either spec file, the same category of gap D19 already
established for K0: `RunContext`, `Key`, `CostEstimate`, `StageError`,
`ProviderRegistry`, `EventSink`, `DeterministicRng`, `CancellationToken`.
Authored each from whatever anchor existed (full detail in each type's own doc
comment in `arbiter-kernel/src/stage.rs`):

- `RunContext` — assembled to carry exactly what [`idempotency_key`] needs (D23).
- `Key`, `CostEstimate`, `StageError` — no struct given anywhere; modelled
  minimally (a `blake3:`-prefixed hash newtype; calls/tokens/dollar-cost fields
  matching ARCHITECTURE §11's own cost-breakdown table; a single `Other(String)`
  error variant, matching `StoreError`'s D19 precedent of not inventing a failure
  taxonomy no real implementation has exercised yet).
- `DeterministicRng` — genuinely implemented (SplitMix64, seeded from the
  manifest's `rng_seed`), not a placeholder, since a real seeded PRNG costs
  nothing extra to build correctly and every future G-task needs one.
- `CancellationToken` — genuinely implemented (`Arc<AtomicBool>`), same reasoning.
- `EventSink` — a trait, not a concrete type, matching the `RunStore` seam pattern
  (D1): the real implementation needs `arbiter-store`'s hash-chaining machinery
  this crate cannot depend on.
- `ProviderRegistry` — stays a near-empty placeholder. Unlike the above, there is
  nothing real to build here yet: it would hold `Provider` trait objects, and
  `Provider` doesn't exist until P1. Kept as a named type now (not deferred
  entirely) so `StageContext`'s shape matches INTERFACES §6 today; P1 fills in
  its fields and lookup methods without needing to change `StageContext` itself.

`Stage::run`'s signature also required a choice INTERFACES §6's code block
doesn't make explicit: it writes `async fn run(...)`, which is stable Rust for a
non-`dyn` trait but is not by itself object-safe. Since nothing in K3's own scope
needs `Box<dyn Stage>` yet (no executor exists to hold one), this was left as
plain `-> impl Future<Output = ...> + Send` (RPITIT) rather than pre-emptively
solving a dyn-safety problem no caller has yet — if a future G-task's executor
needs `Box<dyn Stage<...>>`, that is the point to revisit this, not now.

## D25 — P1's `Provider`/`ProviderRequest`/`ProviderResponse`/`ProviderError`

Same category of gap as D19/D24: INTERFACES §5 and ARCHITECTURE §8.4 pin
`ProviderCapabilities`, `IdempotencyStyle`, and the `CallState` diagram exactly
(carried over from K0 unchanged), but neither spec file gives a code block for
the trait itself or its request/response/error types. Authored in
`arbiter-kernel/src/provider.rs`:

- `ProviderRequest` — `model`, `prompt` (fully-rendered text; template rendering
  is G1's job, this trait never sees a template), `params` (canonical serialized
  call parameters, matching [`crate::store::CacheKey`]'s own `params: String` so
  the two agree byte-for-byte on a cache lookup), `idempotency_key: Option<String>`
  (`Some` only when [`ProviderCapabilities::idempotency`] is `Some` and this is a
  retry — INTERFACES §5's `blake3(prompt_hash ‖ reservation_id)` formula), and
  `reservation: ReservationId`.
- `ProviderResponse` — `text`, `prompt_tokens`, `completion_tokens`, and
  `request_id: Option<String>` (the provider's own identifier, appended the
  moment it arrives per INTERFACES §5, so an orphaned call is reconcilable
  against a usage export; `None` for providers/responses that never issue one,
  including the mock).
- `ProviderError` — a single `Other(String)` variant, matching `StoreError`'s
  D19 precedent: no real adapter has exercised a failure taxonomy yet, so none
  is invented.

`Provider::call`'s signature deliberately diverges from `Stage::run`'s D24
choice: it returns `Pin<Box<dyn Future<Output = ...> + Send + '_>>` rather than
RPITIT, because unlike `Stage`, this trait needs `dyn` dispatch *now* —
`ProviderRegistry` (`arbiter-kernel/src/stage.rs`) genuinely holds a
heterogeneous `BTreeMap<ProviderId, Box<dyn Provider>>` today (mock, and
eventually Anthropic and OpenAI-compatible, behind one type), not once some
future executor exists to need it. This also completes D24's placeholder note
on `ProviderRegistry`: it is no longer near-empty — `register`/`get`/`is_empty`/
`len` are implemented against the now-real `Provider` trait.

## D26 — P2's `MockProvider`

No spec gap beyond D25's: `MockProvider` (`arbiter-providers/src/mock.rs`)
implements `Provider` per ARCHITECTURE §11.1 / IMPLEMENTATION_PLAN.md's own
description ("the mock is not a stub: it scripts the whole CI fixture suite and
opens no socket"). Scripted via a `Mutex<VecDeque<Result<ProviderResponse,
ProviderError>>>` consumed in FIFO order; an unscripted call returns
`ProviderError::Other` rather than panicking or blocking, so an under-scripted
fixture fails its assertion instead of hanging CI. Every received request is
logged to a `Mutex<Vec<ProviderRequest>>` for fixtures to assert against
(prompts, call count, ordering) without the mock itself interpreting them.
`mock_opens_no_socket` — the plan's own named acceptance test — is a structural
guarantee, not a behavioural one: `MockProvider`'s struct has no `reqwest`
client field, no socket, nothing in its fields or dependency graph capable of
reaching the network, so fully scripting and exhausting a multi-call scenario
and getting back only the exact scripted answers is the observable proof.

## D27 — S5's `gc_one_run`/`gc_run` referenced-set injection, and `blob_threshold`'s home

D5 already assigned `blob_threshold` to "the blob store"; no prior task had
actually defined it, so `arbiter-store/src/blob.rs` adds
`DEFAULT_BLOB_THRESHOLD_BYTES: usize = 128 * 1024` (§8.2's stated default) as
the concrete constant D5 promised.

ARCHITECTURE §8.2 describes `doctor --gc` as deleting "blobs not named by any
committed `cache_entries` or `artifacts` row" — but `run.db` does not have
those tables yet (D21: today it only has `events`/`run`/`schema_metadata`;
S4 adds the projection tables that would make `cache_entries`/`artifacts`
queryable). Rather than block S5 on S4 (the plan lists S5's only dependencies
as S3 and K2, not S4), `gc_one_run`/`gc_run` take the referenced-hash set as a
parameter the caller supplies, the same pattern S6's `reindex` used for the
same reason (D21's own precedent: honest about a real limitation rather than
querying tables that don't exist). Once S4 lands, its caller (a future
`doctor` implementation, L-task-scoped) queries `cache_entries`/`artifacts` for
the real referenced set and passes it in — `blob.rs` itself does not change.

The liveness check `is_run_lease_live`/`gc_run` use is not a new predicate:
`lease::owner_is_gone` and `lease::boot_id` were made `pub(crate)` (previously
private to `lease.rs`) so `blob.rs` asks the *exact* question §8.2 requires —
"the same liveness predicate `reopen` uses" — rather than a second,
independently-written one that could drift from it.

## D28 — G1's prompt pack manifest schema, template front-matter, and `Hash` rename

INTERFACES §23 gives `PromptPack { name, version, hash }`, `PromptTemplate {
stage, body, variables }`, and `pub fn prompt_hash(t, rendered) -> Hash`, but
defines `manifest.toml`'s own fields, how a template "declares its variable
schema," and `VariableSchema`/`Hash`/`PackHash`'s concrete shapes nowhere in
either spec file — the same category of gap as D19/D24/D25.

Resolved in `arbiter-kernel/src/prompt.rs`:

- `manifest.toml` carries only pack identity (`name`, `version`). The stage
  name for each template comes from its filename
  (`positions.generate.md` → stage `positions.generate`) rather than being
  listed a second time in the manifest — one source of truth for which
  templates exist, so the manifest and the directory can never disagree about
  the file list.
- Each `<stage>.md` file declares its own variable schema in a leading TOML
  front-matter block (`---` ... `---`), since INTERFACES §23 places `variables`
  on `PromptTemplate` (the per-file type), not on the pack-level manifest.
  Missing front-matter is a load error, not a silently empty schema — an
  omitted declaration is exactly the "silently malformed prompt" INTERFACES
  §23 says schema validation exists to prevent.
- `render()` requires an **exact match** between declared variables and both
  the supplied variable map and the placeholders actually present in the
  body — not a superset in either direction. A stray `{{...}}` the schema
  doesn't declare, a declared variable nothing supplies, or a supplied
  variable nothing declares are all treated as the schema and the template
  having drifted apart, per the conservative-reading default (§0.4).
- `prompt_hash` concatenates `rendered ‖ canonical_schema` with a single NUL
  byte separator (`serde_json`'s canonical array form of the sorted
  `BTreeSet<String>` schema) so no rendered-text/schema split is ambiguous;
  INTERFACES §23's `‖` notation doesn't specify one.
- INTERFACES §23 names the per-call hash type `Hash` — kept as `PromptHash`
  here instead, because `Hash` is also `std::hash::Hash`, already brought into
  scope unqualified by `#[derive(... Hash ...)]` elsewhere in this workspace
  (e.g. `arbiter-kernel::ids`'s `id_type!` macro); a type named `Hash` would
  collide with that the moment both are imported into the same module.
  `PackHash` (the whole-pack identity) needed no such rename.
- `verify_pack_hash` implements "replay refuses a differing `pack_hash`":
  detects a mismatch and returns an error, the same detect-never-repair
  posture `arbiter-store`'s hash-chain verification (S3) already takes. Minting
  a new run id under `--repack` is CLI-scoped (L3), out of G1's own reach.

Adds the `toml` crate (workspace dependency, `arbiter-kernel` only) to parse
`manifest.toml` and each template's front-matter — no TOML parser existed
anywhere in the workspace yet, and hand-rolling one risks a subtly wrong
implementation for a format ARCHITECTURE §15 names explicitly.

G1's own scope is the pack-loading/rendering/hashing machinery only, proven
against pack fixtures built in its own tests — not the actual prompt text for
any of the 15 pipeline stages. Authoring each stage's real `.md` template is
each `G2`–`G9` task's own job as it implements that stage, matching the task
table's "one task per stage group" division; a `prompts/` directory with real
production content is not part of this commit.

## D29 — S4's scope, the `Artifact`/`Tx` trait gap, and the four tables this pass adds

ARCHITECTURE §8.1's projection table names 15 tables by name only; S1 (D21)
deferred all of them. This pass adds four — `budget`, `provider_calls`,
`cache_entries`, `artifacts` — the ones INTERFACES §5's crash-recovery write
order and ARCHITECTURE §8.3's own SQL examples specify precisely enough to
implement without inventing a shape no real stage has exercised yet. `stages`
and the ten claim-graph/decision projections (`positions`, `claims`,
`claim_relations`, `disputes`, `challenges`, `rebuttals`, `judge_evaluations`,
`decision`, `decision_triggers`, `provenance`) stay deferred to `G2`–`G9`,
whose own stage implementations are what will pin their real payload shapes —
designing those columns now, before any stage exists to need them, risks
building against a shape the real stage code then has to work around, the
same reasoning D27 already applied to `cache_entries`/`artifacts` full replay.

**Two trait extensions, both required to make persistence actually
implementable, not stylistic:**

- `Artifact` (INTERFACES §6: "content-addressed, `serde`-typed, and
  versioned") had `artifact_type()`/`content_hash()` but no way to get its
  actual bytes onto disk — `to_json(&self) -> serde_json::Value` is added so
  `Tx::put_artifact` has something to persist. A conforming implementation
  must keep `content_hash()` in agreement with `to_json()`'s output (the hash
  of that value). Both existing `impl Artifact` blocks in the workspace
  (`arbiter-kernel::store`'s `TestArtifact`, `arbiter-kernel::stage`'s
  `Question`/`WordCount`) gained the method; a third was added to
  `arbiter-store::sqlite_store`'s own tests.
- `Tx::reserve_call(call_id, reservation_id, reserved_amount)` — not in
  INTERFACES §1's literal `Tx` trait. INTERFACES §5 step 0 requires, in one
  transaction: `BUDGET_RESERVED{reservation_id, estimate}`, an INSERT into
  `provider_calls` with `state RESERVED` and `reserved_amount`, and
  `budget.reserved += estimate`. `set_call_state(call_id, state)` alone has
  nowhere to carry `reserved_amount` or create the row in the first place —
  it can only transition a row that already exists. Rather than silently
  widen `set_call_state`'s meaning to "create if absent, using undocumented
  side data," this pass adds the one method the write-order sequence proves
  is missing.

**`provider_calls` is keyed by `call_id`, not `reservation_id`** —
`arbiter_kernel::ids::CallId`'s own doc comment already said this is "the key
... the `provider_calls` table [is] keyed on." `reservation_id` is a column: a
retry against an idempotent provider (INTERFACES §5: "the reservation stays
HELD across the retry — never released and re-reserved") shares one
`reservation_id` across more than one `call_id`, so `commit_budget` looks up
the reservation's held amount from the most recent `provider_calls` row for
that `reservation_id` rather than assuming exactly one row per reservation.

**`arbiter-store/src/project.rs`** (the plan's own named file) rebuilds
`budget`/`provider_calls` by replaying `events` — clears both tables and
re-derives them from `BUDGET_RESERVED`, `CALL_STARTED`, `CALL_REQUEST_ID`, and
`CALL_COMPLETED` events in `seq` order. Payload field names
(`{reservation_id, estimate}`, `{call_id, prompt_hash, reservation_id,
estimate}`, `{call_id, request_id}`, `{call_id, response_hash, actual_cost}`)
are copied directly from INTERFACES §5's own brace notation — nothing invented
beyond giving that notation a concrete JSON key spelling. Only the happy path
is reconstructed; `CALL_RETRYING`/`CALL_ORPHANED`/`CALL_RECOVERED` and
`BUDGET_RELEASED`/`BUDGET_EXHAUSTED` are not replayed here (their full
crash-recovery branch logic, INTERFACES §5's own table, belongs to K2/L3's
resume implementation, not this projection rebuild). A `BUDGET_RESERVED` with
no matching `CALL_STARTED` is, correctly, never applied to either table —
INTERFACES §5 states that exact case "resumes as FAILED with the reservation
released," i.e. nets to zero held, which is what not applying it produces by
construction. `cache_entries`/`artifacts` are written directly by
`put_cache`/`put_artifact` at write time and are not replayed by
`rebuild_operational_projections` — no per-stage event payload contract for
cache/artifact content exists yet to replay from (the same gap D27 already
named for blob GC's referenced-set).

## D30 — G2's `init` stage: question validation, `RUN_STARTED`'s payload, and its split across two crates

ARCHITECTURE §5's own words for `init` are "validate question, snapshot config
and prompt pack hash, seed RNG, open log" — no upper bound on question length,
no `RUN_STARTED` payload field list, and (like every other stage) no code
block. Three gaps, same D19 category, resolved as:

- **Question validation** rejects only empty or whitespace-only input.
  "Validate" without a stated rule is read at the conservative minimum the
  stage's own name implies — a question must exist to debate at all — rather
  than inventing a length ceiling or a content policy no worked example asks
  for.
- **`RUN_STARTED`'s payload** carries the question plus every field of
  [`Manifest`] (`policy_version`, `config_hash`, `pack_hash`,
  `correlation_table_version`, `rng_seed`). INTERFACES §23 already gives the
  reasoning for recording `pack_hash` specifically ("the run states which
  prompts produced it"); the same reasoning applies to every other constant
  `--repolicy`/`--repack` can vary, so the whole manifest is recorded rather
  than singling out one field arbitrarily.
- **The implementation is split across two crates.** `init`'s question
  validation is pure (no I/O) and lives in `arbiter-kernel/src/init.rs`. The
  concrete "open the run, then append a correctly hash-chained `RUN_STARTED`"
  orchestration needs `RunStore`'s concrete implementation *and*
  `arbiter-store::events::ChainState`/`append_chained` in the same call —
  `arbiter-kernel` cannot depend on `arbiter-store` (D1) — so that half lives
  in `arbiter-store/src/init.rs` instead, calling back into
  `arbiter_kernel::init::validate_question`. `init` is deliberately not a
  `Stage` impl: K3's `Stage`/`StageContext` (INTERFACES §6) presuppose an
  already-open run to hold `events`/`budget`/`cache` references into: `init`
  is what creates that run in the first place, so it runs one level below the
  `Stage` abstraction, not through it.

**G2 scope note:** the plan's G2 task bundles five stages (`init`,
`panel.resolve`, `positions.generate`, `claims.extract`, `claims.normalize`)
into one line. This commit implements `init` only — the one stage with no LLM
call, no panel/correlation-table dependency (`panel.resolve` needs
`correlation.toml`, not yet shipped), and no real provider wiring, making it
the cleanest, most self-contained unit to land correctly on its own. The
remaining four are large enough on their own (real provider orchestration,
grounding/repair, Kahn cycle detection, three-tier similarity matching) that
attempting all of G2 in one pass risks exactly the kind of rushed, under-tested
work this project's own §0.2/§0.4 discipline exists to prevent. Each will be
picked up as its own follow-on pass, in pipeline order.

## D32 — `claims.extract`: grounding, the repair loop, and the cycle-cutting simplification

INTERFACES §2 is unusually specific for a stage protocol — an exact validation
order, a repair contract, and a three-step cycle-untangling procedure — but
still leaves gaps this task had to fill, all D19-category:

- **`RawCandidate`/`RawRepair`** (this module's own names for the extractor's
  and repair model's JSON shapes) transcribe INTERFACES §2's two worked
  examples (`{"text","kind":"fact","grounding":{"quote"}}` /
  `{"kind":"inference","grounding":{"derived_from"}}`) directly, plus one
  addition: an inference's `confidence` field. Neither example shows it, but
  the cycle-cutting protocol names "ascending extractor confidence" as its
  tie-break with nowhere else that value could come from — added as an
  extractor-supplied `f64`, defaulting to a neutral 0.5 when omitted (a
  scripted fixture, say).
- **Exact/fuzzy matching** ("whitespace- and case-normalised substring
  search"; "trigram Jaccard ≥ 0.85 over a sliding window the length of the
  quote") is implemented token-based, not via a literal
  normalize-then-substring-find: the latter needs a lossy index-mapping step
  back to the original text's byte offsets once normalization changes the
  string's length, which a whitespace-tokenized sliding window avoids
  entirely while still matching the spec's stated intent.
- **`claims.extract`'s own output shape**: one singleton `CanonicalClaim`
  (one `ClaimMember`) per extracted claim, using `arbiter-core`'s existing
  `CanonicalClaim`/`ClaimMember`/`Grounding`/`EvidenceKind` types verbatim
  (C1-era, already spec-verified) rather than inventing parallel ones.
  Multi-member canonical claims are `claims.normalize`'s job (§5.2: "Claude:
  ... GPT: ... → CanonicalClaim{members:[...]}"), not this stage's — extract
  only ever mints provisional, per-position claim identity
  (`claim_<position_id>_<n>`); normalize is what merges equivalent members
  from different positions under one surviving id.
- **Repair budget enforcement** reuses `bounds::repair_budget`'s cap (K4,
  already built) as a constructor parameter, tracked via a `Mutex<f64>`
  cumulative counter checked before every repair call — "whichever binds
  first stops repairs" (INTERFACES §2): the count bound (one repair call per
  position, enforced structurally — there is only ever one repair call site
  per position in this code) and the cost bound (this counter) are both
  respected, and once the cap is hit, remaining failures are admitted as
  `Unsupported` without attempting a call that would exceed it.

**Cycle-cutting: greedy only, not the exact-for-|SCC|≤12 variant.** INTERFACES
§2's step 2 reads: "remove the minimum set of derivation edges that restores
acyclicity. Exact for |SCC| ≤ 12, otherwise greedy by ascending extractor
confidence." This task implements only the greedy half, applied uniformly
regardless of component size — repeatedly cutting the lowest-confidence edge
still inside the (recomputed) cyclic set and re-checking with Kahn's algorithm
until acyclic. A real minimum-feedback-arc-set solver for the small-graph case
is a separate, nontrivial algorithm in its own right (brute-force search over
edge subsets, even bounded to 12 edges, is still meaningfully more machinery
than the rest of this stage), and the greedy fallback is not a corner cut of
convenience — it is the spec's own stated algorithm for the general case, just
not switched away from for small components. It always terminates (edges are
finite) and always restores acyclicity correctly; it is simply not guaranteed
to find the *globally* minimum cut on a small SCC the way the exact variant
would. Revisit if a fixture ever demonstrates the difference matters
(`premise_cycle_grounded_fact`, once F2 exists).

Two tests (`a_premise_cycle_resolved_by_repair_leaves_no_claim_unsupported`,
`an_unresolved_cycle_falls_back_to_cutting_the_weakest_edge`) exercise both
untangle-before-degrade outcomes end to end, including the specific case
INTERFACES §2 calls out by name: a claim whose derivation chain still traces
back to a real, independently-grounded `DirectQuote` after the cut survives
as `Derived`, never falls to `Unsupported`.

## D33 — `claims.normalize`: which similarity machinery it reuses, and what it narrows

ARCHITECTURE's own description of this stage is one line: "cluster equivalent
claims across models; members preserved | cheap similarity + LLM tie-break on
top-K." No algorithm, no code block, nowhere else in either spec file gives
this stage its own worked mechanism. The only concrete "cheap similarity"
machinery given anywhere is INTERFACES §3's T1 (lexical: normalise → trigrams
→ IDF-weighted cosine → top-K, with the K-scaling formula) and T3 (one
batched LLM call — "group claims that state the same underlying point," with
`t3_merge_threshold`, `t3_max_claims_per_batch`, and the partition/pack/stitch
protocol for large claim sets). That machinery sits under ARCHITECTURE §5.4's
"Relationship detection" heading, and its own worked pipeline opens with
"claims → normalise" — i.e. it is written for `relations.analyze` (G4), which
consumes *this* stage's output, not the other way around. Two things settle
that this task should still reuse it rather than invent a second mechanism:

1. It is the *only* concrete algorithm INTERFACES gives for "cheap similarity"
   anywhere in the document — inventing a second, parallel one for
   `claims.normalize` specifically would be a bigger deviation than reusing
   the one given, and would leave two undocumented "cheap similarity"
   algorithms in the workspace instead of one spec-anchored one.
2. `prompts/<pack>/<version>/claims.group.md` — T3's own batched grouping
   call — is named explicitly in ARCHITECTURE §15's prompt-pack file list, a
   file name that only makes sense as this stage's (a *relationship*
   classification call would produce `RelationKind`s, not merge groups; "group
   claims that state the same underlying point" is a clustering decision,
   exactly `claims.normalize`'s stated job).

So this task implements exactly the T1+T3 half of the §3 machinery — the half
that needs no `options.cluster` output. **T2 (the polarity sweep) is
explicitly out of scope here**: it requires attachment to options, which
don't exist until `options.cluster` (G3) runs, two stages later in the
pipeline; T2 belongs to `relations.analyze`'s own future implementation, which
runs after options exist.

**Implementation choices, all D19-category (no code block given for any of
these):**

- **No SimHash blocking.** T1's own blocking step (64-bit SimHash) is a
  scalability optimization ahead of the cosine step, not a correctness
  requirement; `top_k_pairs` computes cosine directly over every pair, which
  is correct — just not the eventual production performance profile — at the
  claim counts a real debate produces before F2's fixture suite exists to
  stress it.
- **The K-scaling formula is transcribed as literally given**
  (`clamp(ceil(3.0 · log2(n+1)), 8, 24)`), not reverse-engineered from
  INTERFACES §3's own worked-example table (`n=12 → 11`, `n=32 → 16`, ...),
  which does not reproduce exactly under this reading with ordinary rounding
  — most likely a documentation rounding inconsistency in the table rather
  than in the formula, which is given as an actual expression. Tests assert
  the formula's stated properties (clamped to `[8,24]`, monotonic) rather than
  the table's exact numbers.
- **Merge-kind rule for a mixed-kind group**: the strongest (lowest
  `kind_weight`-rank) `EvidenceKind` among the group's members, e.g. a group
  with one `Fact` member and one `Unverified` member merges to `Fact`. Neither
  spec file states a rule for this; corroboration (multiple models converging
  on one point) should never make a claim look *less* evidenced than its
  best-supported member alone would, so this is the conservative choice in
  the evidence-favourable direction. (`arbiter-core::decision::evidence::effective_kind`
  already handles the one case this doesn't need to: a claim whose surviving
  members are *all* `Unsupported` still degrades to `Unverified` downstream,
  regardless of what this stage records.)
- **Surviving id on merge**: the lexicographically smallest claim id in the
  group — deterministic, independent of processing or LLM-response order.
- **Stitch recursion**: implemented to one level, not the full recursive
  depth-2 protocol INTERFACES §3 describes for extreme claim counts (300+).
  If more than one batch exists, one stitch call runs over the resulting
  per-batch representatives; if there are more than `t3_max_claims_per_batch`
  representatives even after batching (the spec's own words: "far past any
  realistic debate"), this stage emits `CANDIDATES_SELECTED { tier:
  "stitch_depth_exceeded" }` and skips the stitch call rather than recursing
  — matching the greedy-not-exhaustive posture already established for
  `claims.extract`'s cycle-cutting (D32): a real, terminating, documented
  fallback for a case the spec itself calls a rare defensive branch, not the
  common path.

## D34 — `options.cluster`: multi-artifact input, the cluster+attach contracts, and where Step 3 actually runs

INTERFACES §20 is the most concrete section for any G-task so far — it gives
a full `OptionClusterer` trait, `AttachmentMatrix`/`Attachment`/`Polarity`/
`AttachSource` structs, and a fully-worked, three-step algorithm. Almost none
of it needed inventing; `arbiter-core`'s C4 work already built and tested
Step 3 (`decision::attachment::propagate`) and the §6.5 scoring
(`score_options`) verbatim from this same section, months (in task-graph
terms) before this task existed to consume them. What this task adds:

- **`ClusterInput`, a combining wrapper.** `options.cluster` is the first
  stage needing more than one upstream artifact — INTERFACES §20's own
  `OptionClusterer` trait takes `positions` and `claims` as two separate
  method arguments, but K3's `Stage` trait (INTERFACES §6, copied verbatim)
  has exactly one associated `In` type, with no provision anywhere in either
  spec file for a multi-artifact stage. Resolved with a small `Artifact`
  wrapper (`ClusterInput { positions, claims }`, content-hashed over both)
  rather than changing `Stage`'s own shape, which would ripple back through
  every already-shipped single-input stage for no reason this task's own
  scope requires.
- **The cluster call's contract** (`prompts/default/v1/options.cluster.md`):
  a batched grouping call over ALL position text (not just "the
  recommendation" — positions don't carry a separately-extracted
  recommendation field anywhere in this codebase; `positions.generate`'s own
  prompt already asks for reasoning *and* a labelled recommendation within
  one text block, so the clustering call reads the whole thing and is asked
  to identify + group recommendations *and* supply a label for each group in
  one pass, since no earlier stage produces a machine-parsed recommendation
  field to cluster instead). Every position resolves to exactly one option —
  a position the model's response never mentions still becomes its own
  singleton option, and an unparseable response degrades to "every position
  is its own option" — both preserve INTERFACES §20's own invariant, "no
  option is ever invented," by construction: nothing is ever merged or
  dropped, only ever split further on failure.
- **The attach call's contract** (`prompts/default/v1/options.attach.md`):
  "Claims from a position that recommended O start as `Authored` toward O and
  may be revised by the classifier" is implemented literally — every claim is
  seeded `Authored`/`Supports`/1.0 toward its own position's clustered option
  *before* the classifier call runs, and the classifier's response for that
  exact `(claim, option)` pair overwrites the seed when it says `supports` or
  `opposes`, or removes it entirely when it says `neutral` (a neutral verdict
  on a specific pair the classifier was actually asked about is itself a
  revision, not silence). The classifier omits pairs it judges neutral by
  default (INTERFACES §20 doesn't specify whether the response must be dense
  or sparse; a sparse response — only entries "you have something to say
  about" — matches "one call for the whole matrix" being about avoiding
  |C|×|O| separate *calls*, not about padding one response with `(claim,
  option)` pairs neither side needs recorded).
- **Where Step 3 actually runs: not here.** `propagate` needs
  `relations: &[Relation]`, and `relations.analyze` — the stage that produces
  them — runs *after* `options.cluster` in the pipeline
  (`options.cluster → relations.analyze`). So this task's own output is the
  direct matrix only (`Authored`/`Classified` cells); calling `propagate` is
  whichever later stage first holds both a matrix and a relation graph
  together, which is outside this task's own scope to build or guess at.

## D31 — `positions.generate`: `Question`/`Position`/`Positions`, the missing per-provider semaphore, and the first real prompt

ARCHITECTURE §5.1's only description of a position is "position text"; no spec
file gives `Position` (or the input `Question`, or the stage's `Vec`-of-them
output) a struct anywhere — same D19 category. Authored in
`arbiter-kernel/src/stages/positions_generate.rs`:

- `Question { text }`, `Position { model, provider, text }`, `Positions(Vec<Position>)`
  — all `Artifact` impls. `Positions::content_hash` sorts its members by
  `(provider, model)` before hashing, since concurrent completion order is not
  deterministic and a stage's idempotency key (derived from its output's
  content hash by whatever consumes it next) must not depend on it.
- **Cache-then-reserve-then-call-then-commit**, matching INTERFACES §5's
  crash-recovery write order and §7's "provider stages consult the cache
  first": a `CacheKey` lookup runs before any reservation; only an *inline*
  cache hit is usable (a blob-backed one needs `arbiter-store` to read it
  back, D1, so it falls through to a real call — a documented, narrow gap,
  not a correctness bug, since the response is still obtained, just not from
  cache). `BUDGET_RESERVED`/`CALL_STARTED`/`CALL_REQUEST_ID`/`CALL_COMPLETED`/
  `BUDGET_COMMITTED` are all emitted through `ctx.events`, in the same order
  S4's `project.rs` replay already expects (D29) — nothing about that replay
  logic changes.
- **`FailurePolicy::SkipItem`** (INTERFACES §6, stated for this exact stage)
  covers every per-item failure mode: a scripted/real provider error, an
  unregistered provider, and `BudgetExhausted` are all "skip this position,"
  never fatal to the stage. A `ReservationGuard` that is dropped without
  `commit()`/`mark_orphaned()` releases itself (K1's own `Drop` guarantee) —
  no manual release code is needed on any of these paths.
- **No per-provider semaphore.** INTERFACES §6 asks for "a bounded join set
  and a per-provider semaphore"; this pass implements only the bounded join
  set (`futures_util::stream::buffer_unordered(max_parallelism)`, one global
  bound across the whole panel). A true per-provider `tokio::sync::Semaphore`
  matters for real rate-limited HTTP providers, which don't exist yet (P4);
  against `MockProvider`-shaped testing it has no observable effect, so
  adding it now would be unverifiable. Revisit when P4 lands.
- **No real per-token pricing.** `estimated_cost_per_call: Cost` is a flat,
  caller-supplied amount charged as both the reservation estimate and the
  commit's `actual_cost` — no pricing table exists anywhere in this workspace
  (P4's job). This never under-charges the ledger relative to what was
  reserved, which is the property the budget invariant (§8.3) actually needs;
  it is not real cost accounting.

Adds `futures-util` (workspace dependency, `arbiter-kernel` only) for
`buffer_unordered` — the minimal stream-combinator subset, not the full
`futures` metapackage, since nothing else here needs channels, executors, or
I/O traits from it.

**Test provider, not `arbiter-providers::mock::MockProvider`.** This module's
tests use a small local `ScriptedProvider` rather than P2's `MockProvider`:
`arbiter-kernel` cannot depend on `arbiter-providers` (D1) — that crate already
depends on `arbiter-kernel`, so the reverse would be a cycle. `ScriptedProvider`
is the same scripted-`VecDeque` shape, just local to this test module, the same
way `provider.rs`'s own `EchoProvider` is local to its tests rather than reused
from elsewhere.

**First real prompt content.** `prompts/default/v1/manifest.toml` and
`prompts/default/v1/positions.generate.md` are the first production template
G1 deferred (D28) — a real, functional prompt asking one panelist for
reasoning plus a concrete recommendation, with no cross-talk. Exact wording is
implementation detail no spec file mandates; a test loads this exact shipped
file (not an in-memory fixture) through `PromptPack::load` to prove it parses
and renders correctly.

## D35 — `relations.analyze`: shared `similarity.rs`, the T2 polarity sweep, and direction resolution

ARCHITECTURE §5.4 ("Relationship detection") and INTERFACES §3 give this
stage's shape — T1 lexical candidates plus a "T2" pass, batched pairwise LLM
classification into `RelationKind` — but, as with D33's `claims.normalize`,
neither spec file gives T1 its own copy of the algorithm here; it is the same
"cheap similarity" machinery INTERFACES §3 already describes once, textually
anchored under this stage's own ARCHITECTURE section.

- **`similarity.rs` extraction.** `claims.normalize` (D33) and this stage both
  need identical T1 candidate generation (`UnionFind`, `top_k`, trigram-IDF
  cosine, `top_k_pairs`, `partition_into_batches`). Rather than a second copy
  — which D33 had accepted as the lesser evil at the time, since only one
  consumer existed — a third consumer made the duplication cost outweigh the
  module-boundary cost, so the shared logic (with its own pure-function
  tests) now lives once in `arbiter-kernel/src/stages/similarity.rs`
  (`pub(crate)`), imported by both stage modules. `claims_normalize.rs` lost
  its local copies and their four duplicate tests; nothing about its own
  behavior changed, confirmed by its 7 remaining tests still passing
  unmodified.
- **T2, "the polarity sweep."** ARCHITECTURE §5.4 gives this exactly one
  sentence: every cross-model pair attached to opposing options is a T2
  candidate. No spec file expands this into a formula. The literal,
  non-inventive reading, implemented in `polarity_pairs`: for each clustered
  option, collect the claims with a `Supports` cell and the claims with an
  `Opposes` cell on that option (from `ClusteredOptions.direct_matrix`, D34's
  output), form the cross-product, and keep a pair only if at least one
  `Supports`-side/`Opposes`-side attachment combination involves two
  different `model` fields (the "cross-model" qualifier). "Opposing options"
  is read as "opposing polarity on the same option," since options
  themselves don't carry a polarity — attachments do (INTERFACES §20's own
  `Polarity` enum lives on `Attachment`, not on `DecisionOption`).
- **`AnalyzeInput`, reusing D34's combining-wrapper pattern.** This stage
  needs both `NormalizedClaims` and `ClusteredOptions` — the same
  multi-artifact-input gap D34 hit first (K3's `Stage` trait has exactly one
  associated `In` type). Resolved identically: a small `Artifact` wrapper
  (`AnalyzeInput { claims, options }`, content-hashed over both) rather than
  reshaping `Stage` itself.
  `#[derive(Debug, Clone, PartialEq)]` only (no `Eq`) on `AnalyzeInput` and
  `AnalyzedRelations`, matching `ClusteredOptions`'s own existing precedent —
  both transitively contain `f64` fields (`Cost`, `confidence`), which don't
  implement `Eq`.
- **Direction (`from`/`to`) resolution.** `Relation` (`arbiter-core`) already
  has a directed `from`/`to: ClaimId` shape; this stage's prompt
  (`prompts/default/v1/relations.classify.md`) asks the model for a
  same-shaped `"from": "A"|"B", "to": "A"|"B"` per pair, using the pair's own
  local `"A"`/`"B"` labels (not real claim IDs, to keep the prompt short),
  which the stage then resolves back to the real `ClaimId`s it substituted
  into that pair's block. For the two direction-insensitive kinds
  (`Unrelated`, `Uncertain`) the prompt asks for a fixed `"from": "A", "to":
  "B"` rather than leaving the field ambiguous, so parsing never special-cases
  those two kinds.
- **Batch size (30 pairs) and omitted-pair handling.** No spec file gives a
  numeric batch size for this call; 30 is chosen for the same token-budget
  reasoning D34 used for its own batched calls (kept well under typical
  context limits even with two claim texts quoted per pair). A pair the
  model's response omits, or whose `pair`/`kind` field fails to parse,
  produces no `Relation` for that pair rather than a stage failure — matching
  `FailurePolicy::DegradeWithEvent`'s intent of degrading gracefully rather
  than discarding the whole batch over one bad element.
- **Fewer than two claims.** With 0 or 1 claims there is no possible pair, so
  `run()` short-circuits to an empty `AnalyzedRelations` without making any
  provider call at all (asserted directly by
  `fewer_than_two_claims_never_calls_the_provider`) — consistent with every
  other stage's "don't spend budget on work with no possible output."

## D36 — `disputes.rank`: resolving the graph, `contested_mass`, and a gap in already-shipped G4

INTERFACES §21 gives `dispute_priority` as a full formula with named terms and
default weights (0.35 · 0.35 · 0.20 · 0.10) — the most concrete section for
any G-task since §20 (D34). What this task adds, in
`arbiter-core/src/decision/dispute.rs` and
`arbiter-kernel/src/stages/disputes_rank.rs`:

- **`dispute_priority`'s real signature vs. its pseudocode one.** §21 writes
  `fn dispute_priority(c: &CanonicalClaim, g: &ResolvedGraph, cfg:
  &PolicyConfig) -> f64` — neither `ResolvedGraph` nor `PolicyConfig` is given
  a concrete definition anywhere (D19's category), and the four terms are
  naturally computed by two different layers: `contested_mass`,
  `decision_leverage` and `evidence_gap` need only recorded artifacts (pure,
  belongs in `arbiter-core`), but `resolution_cost` ("estimated tokens for the
  exchange ÷ remaining budget") needs a real `BudgetLedger`, which
  `arbiter-core` cannot depend on (D1). `arbiter-core::decision::dispute`
  therefore takes four already-computed `f64` components plus a new
  `DisputeWeights` config struct (`w_contested`/`w_leverage`/`w_gap`/`w_cost`,
  defaults transcribed from §21's own comment) rather than a fictional struct
  standing in for values it cannot produce on its own.
- **`contested_mass`, "normalised" read literally.** §21: "`Σ
  standing(attackers) + Σ standing(defenders)` around `c`, normalised" — no
  formula for what "normalised" means. Read as the mean standing of every
  claim with a `Contradicts` or `Supports` edge into `c`: bounded to `[0,1]`
  by construction (every individual standing already is; a raw sum is not),
  and `0.0` for a claim nobody has any live relation into — "a claim nobody
  contests is not a dispute" is §21's own stated reason this term exists at
  all.
- **`decision_leverage` is not reimplemented.** It already exists, verbatim,
  as `CounterfactualFlip::leverage()` (C7, `decision::triggers`) — §21 says as
  much itself ("`decision_leverage` reuses the counterfactual machinery
  already built for change triggers"). This stage calls
  `triggers::counterfactual_flips` directly over the claims currently
  `Disputed`/`Unresolved` (`standing::classify_all`, C3) and reads `.leverage()`
  off each returned flip.
- **Where Step 3 (attachment propagation) actually runs: here.** D34
  deferred `options.cluster`'s own Step 3 call with an exact prediction:
  "calling `propagate` is whichever later stage first holds both a matrix and
  a relation graph together." `disputes.rank` is that stage — it is the first
  to be handed claims, relations, *and* the direct attachment matrix all at
  once (`RankInput`, the same combining-wrapper pattern D34/D35 already
  established for exactly this "no spec-given multi-artifact `Stage::In`"
  gap). `run()` therefore calls `attachment::propagate` before scoring
  anything, and `counterfactual_flips` is given the *propagated* matrix, not
  the direct one — a claim's leverage over an option it only reaches through
  the relation graph (not a direct attachment) would otherwise read as zero
  leverage, understating exactly the claims Step 3 exists to credit.
- **`RankedDisputes`, "the resolved graph."** §21's `ResolvedGraph` is never
  defined (above), but this stage's own output is a reasonable, literal
  reading of what that name would hold: claims/relations/options carried
  forward unchanged (later stages, starting with `challenge.plan`, need them
  again), plus the fixpoint standing map and the propagated matrix this stage
  computed. `evidence.rs`'s `judge_factor` needs a `scores:
  &BTreeMap<ModelId, Scorecard>` this stage does not have — `judge.evaluate`
  is stage 13, seven stages after this one — so it is passed an empty map,
  which `judge_factor` already degrades to `1.0` for (no judge signal yet,
  not zero evidence).
- **`FixpointNotConverged`.** `disputes.rank` is the first stage to call
  `fixpoint::solve` on real pipeline data; INTERFACES §12's "if `max_iterations`
  is reached with `Δ > ε`, the engine emits `FIXPOINT_NOT_CONVERGED {
  max_delta, iterations }`" is wired here, the one place it was possible to
  wire until now.
- **A gap in already-shipped G4, fixed in passing.** ARCHITECTURE §9 /
  INTERFACES §13 both name `RELATIONSHIP_FOUND` as one of the Debate-family
  events, but `relations.analyze` (G4, D35) never emitted it — only
  `CandidatesSelected`/`StageStarted`/`StageCompleted` and the provider-call
  events. Neither §13 nor §9 states the exact firing granularity for this
  event (no payload contract is given for it, the same category as every
  other D19 gap), but `claims.extract`/`claims.normalize`'s own precedent
  (one `ClaimExtracted`/`ClaimNormalised` per claim) makes "one
  `RelationshipFound` per successfully parsed relation" the consistent
  reading, not an invented one. Fixed directly in
  `relations_analyze.rs` (one `ctx.events.emit` added to the existing parse
  loop, one new assertion in `a_lexically_similar_pair_is_classified`) rather
  than deferred to a separate task, since it was caught while building the
  stage that consumes `relations.analyze`'s own output.

## D37 — `challenge.plan`: money-derived sizing, "the claim's author" generalised, and `ChallengeIssued` reserved for G6

ARCHITECTURE §5.5 and INTERFACES §21 give this stage's budget derivation and
pair-selection algorithm as literal pseudocode — the most mechanical G-task
so far. What required a decision:

- **`remaining_rounds`.** "each round takes `remaining_budget ÷
  remaining_rounds`" — neither spec file gives `remaining_rounds` a formula.
  Read as `max_rounds − current_round + 1` (the round about to be planned
  counts toward its own share): at `--depth standard` (`max_rounds = 1`,
  round 1) this is 1, so the whole remaining budget is available to the only
  round there is; at `--depth deep` (`max_rounds = 3`) it steps 3, 2, 1 across
  rounds 1–3. `max_rounds` itself is not a field either `StageContext` or
  `RunContext` carries (D19's category again), so it is a constructor
  argument on `ChallengePlan`, the same way every other tuning constant this
  crate cannot source from the artifact graph is (D31's `estimated_cost_per_call`
  precedent).
- **"The claim's author" (singular) generalised to the claim's asserters
  (plural).** §21's pseudocode: `defender = the claim's author`. A
  `CanonicalClaim` can carry members from several models at once — that is
  the entire point of `claims.normalize` (ARCHITECTURE §5.2: "cluster
  equivalent claims across models; members preserved"). Picking one of
  several asserting models as "the" author to compare against would be
  arbitrary; the rule's actual purpose — never let a model challenge a claim
  it itself asserted — generalises cleanly to "skip any challenger present in
  `claim.asserted_by()`", which is what `ChallengePair.defenders: Vec<ModelId>`
  and the selection loop implement. Verified directly:
  `a_defended_claim_is_never_challenged_by_its_own_author`.
- **Challenger selection walks ranked attackers, not just the single
  strongest.** §21: "challenger = the model whose claim most strongly
  contradicts it ... skip if challenger == defender ... skip if that model
  already has `max_challenges_per_model`". Read as: sort this claim's
  `Contradicts` attackers by `confidence × attacker_standing` descending
  (claim id ascending as a tie-break), then walk that list — and, within one
  attacking claim, its own `asserted_by()` models in order — taking the first
  model that is neither a defender nor already at cap. This is the literal
  algorithm the two `skip if` lines describe (try the best, and if it's
  disqualified, try the next), not an invented fallback; verified by
  `a_model_already_at_the_per_model_cap_is_skipped_in_favor_of_the_next` and
  `the_strongest_cross_model_attacker_is_chosen_as_challenger`.
- **The per-model cap is per round, across every planned pair — never reset
  per claim.** "that model already has `max_challenges_per_model` this
  round" (§5.5's own stated reason: it's a *fairness* limit inside the
  money-derived envelope, never what sizes it) — `per_model_count` is one
  running map for the whole `run()` call, incremented as each pair is
  accepted, exactly matching "this round" rather than "this dispute".
- **`ChallengeIssued` is not emitted here.** ARCHITECTURE §5's own pipeline
  table assigns "issue challenges in parallel" to `challenge.run`, not
  `challenge.plan` ("select targeted pairs within budget; never all-pairs" —
  selection, not issuance). This stage emits only `StageStarted`/
  `StageCompleted`; `ChallengeIssued` is left for G6 to fire when a challenge
  actually goes out.
- **`ChallengePlanned`, carrying `RankedDisputes` forward whole.** Same
  "resolved graph, carried forward" shape D36 established for
  `disputes.rank`'s own output — `challenge.run` will need the claim texts,
  relations and standing again, and re-deriving them from scratch would
  duplicate work this stage's input already did.

## D38 — `challenge.run` / `rebuttal.run`: issuing the challenge, applying the verdict, and versioned deltas without a claim-history artifact

ARCHITECTURE §5's own pipeline table gives both stages one line each ("issue
challenges in parallel"; "defend / modify / withdraw → versioned claim
deltas") and no dedicated subsection in either spec file works through the
mechanics — the least-specified G-task pair since G1. What was authored, in
`arbiter-kernel/src/stages/challenge_run.rs` and
`.../rebuttal_run.rs`:

- **`challenge.run` fans out `PerItem`, mirroring `positions.generate`'s own
  reasoning exactly**: independent calls to independent models (the
  challengers `challenge.plan` already chose), none waiting on any other, one
  bounded join set (`buffer_unordered`), `FailurePolicy::SkipItem` — a
  challenger's call failing means one fewer challenge issued this round, not
  a fatal stage.
- **A challenged claim's lifecycle moves to `Challenged` in `challenge.run`
  itself**, not deferred to `rebuttal.run`. ARCHITECTURE §6.1's own state
  list (`Proposed | Verified | Challenged | Defended | Modified(v) |
  Withdrawn | Rejected`) gives `Challenged` a place between "nothing has
  happened to it yet" and every possible rebuttal outcome; stamping it the
  moment a challenge actually goes out is the literal reading, and it means a
  claim whose *rebuttal* call later fails (or whose response is
  unparseable) still correctly reads `Challenged` — "has an open challenge,
  outcome not yet known" — rather than silently reverting to whatever it was
  before, or requiring `rebuttal.run` to special-case "no verdict" as its own
  branch.
- **`rebuttal.run` addresses one representative defender, not every
  co-asserting model.** D37 already generalised "the claim's author"
  (singular) to a claim's full asserter set for the self-challenge check;
  here the same plurality means a merged claim could in principle owe a
  rebuttal call to several models at once. Read as one call to the first
  asserting model in deterministic (sorted) order: the debate defends *the
  claim*, and multiple co-asserting models independently restating the same
  defence would not add dialectical information, only cost. `defenders` is
  still carried on `IssuedChallenge`/recorded in the exchange for provenance
  — this narrows *who is asked*, not what is recorded.
- **`Modify` appends a member; it does not rewrite `text`.** ARCHITECTURE
  §5.2's own invariant — "originals are never destroyed; every derived
  number traces back to a member" — is a statement about `members`, not
  about the canonical `text` field, and no spec text says a modification
  rewrites the cluster's representative wording. The conservative reading:
  `lifecycle` moves to `Modified{version}` (version computed as `1` for a
  claim modified for the first time, `v + 1` if it was already
  `Modified{version: v}` from an earlier round — no spec file gives this
  counting rule either), and the model's revised wording is appended as a
  **new** `ClaimMember` (synthetic `PositionId::new("pos_rebuttal_<claim>_<v>")`,
  since a rebuttal is not itself a position) rather than overwriting any
  existing member's `original_text`. The claim's top-level `text` is left
  untouched.
- **No grounding pipeline re-runs on a rebuttal.** `claims.extract`'s
  exact/fuzzy/derived/repair protocol (D32) is that stage's own job, not
  this one's — re-running it here would be a large, unrequested scope
  expansion for a one-line spec entry. The new member from a `Modify`
  verdict is admitted at `Grounding::Unsupported`, the same conservative
  floor `claims.extract` itself falls back to for anything it cannot verify
  — unevidenced-but-real risk, not silently dropped (ARCHITECTURE §5.1's own
  stated reason `Unsupported` is admitted rather than rejected).
- **No claim-version-history artifact.** INTERFACES §18's `C-024@v1` /
  `C-024@v2` citation format (Build Studio's provenance gates, out of 1.0
  scope per this plan) implies *some* notion of addressing a specific past
  version, but nothing in G1–G9's own scope needs it yet, and inventing a
  parallel history store ahead of a real consumer would be exactly the kind
  of unrequested abstraction this project's own discipline avoids. The
  version number lives where ARCHITECTURE §6.1 already puts it —
  `ClaimLifecycle::Modified { version }` on the current claim — and nothing
  more.
- **`RebuttalsRun.next_round_input` reuses `RankInput` verbatim**, not a
  bespoke output type. INTERFACES §11's controlled loop
  (`challenge.plan → challenge.run → rebuttal.run → controller.decide`) folds
  back into another `disputes.rank` pass for the next round (`round` in the
  idempotency key is what makes a resumed run re-enter the right iteration);
  giving `rebuttal.run`'s output exactly the shape `disputes.rank` already
  consumes means the next round needs no adapter stage. `standing` and the
  propagated matrix are deliberately **not** carried forward from
  `RankedDisputes` into this output — they were computed from the
  *pre-rebuttal* claims and are stale the instant a lifecycle changes;
  recomputing them from the updated claims is exactly what feeding back into
  `disputes.rank` is for.
- **`ChallengeIssued` fires in `challenge.run`; `RebuttalReceived` fires in
  `rebuttal.run`** — both already named in the event taxonomy (ARCHITECTURE
  §9 / INTERFACES §13), wired here for the first time now that both stages
  exist, at the same "one event per completed item" granularity D36's
  `RelationshipFound` fix established as this workspace's consistent
  reading.

## D39 — `controller.decide`: re-resolving the graph, the stop-predicate precedence, and the executor gap

ARCHITECTURE §5.5 gives both stop predicates as literal formulas and the
three tuning constants they need (`converged_margin_factor` 1.5,
`min_new_claims` 2, `min_standing_delta` 0.05) are already named in
IMPLEMENTATION_PLAN.md §0.6 as "kernel controller" constants (D5) — the most
concrete task since G5. What required a decision, in
`arbiter-core/src/decision/controller.rs` and
`arbiter-kernel/src/stages/controller_decide.rs`:

- **The round subgraph, taken at its word.** INTERFACES §11 / this crate's
  own `ControlFlow` doc comment (K3) describe the controlled loop as exactly
  `challenge.plan → challenge.run → rebuttal.run → controller.decide` —
  `disputes.rank` is not in that list, even though `challenge.plan`'s own
  input type is `disputes.rank`'s output (`RankedDisputes`) and the claims
  have changed since `rebuttal.run` ran. Resolved by **not** re-invoking
  `disputes.rank` as a stage a second time, but by extracting its own pure
  "resolve the graph and rank disputes" computation
  (fixpoint → Step 3 propagation → standing classification → counterfactual
  flips → `dispute_priority` ranking) into a shared function,
  `disputes_rank::resolve_and_rank`, that both `DisputesRank::run()` (once,
  before the loop) and `ControllerDecide::run()` (every iteration, on the
  post-rebuttal claims) call identically. The subgraph's four *stages* stay
  exactly as named; the *logic* `disputes.rank` owns is reused, not
  reinvented, inside the fourth one. `disputes_rank.rs`'s own tests were
  re-run unmodified after this extraction to confirm it changed nothing
  about that stage's own behavior.
- **No executor exists to build "the round loop" into.** INTERFACES §11's
  "the executor re-instantiates the round subgraph" describes something that
  actually drives repeated stage invocation — no `StageGraph` runner exists
  anywhere in this codebase yet (every stage so far is exercised by
  constructing and calling it directly in tests; wiring one together is
  L1–L4/CLI's job). This task's scope is therefore the decision itself:
  given one round's artifacts, produce a correct `ControlFlow` and a
  `resolved` graph a future executor would feed into the next iteration —
  not the loop that would act on it.
- **Stop-predicate precedence.** Neither spec file states an evaluation
  order across `StopReason`'s eight variants. Read as: the four *hard*
  bounds first, in the order a real process would actually hit them
  (`Cancelled` — the token was flipped externally — before `Deadline`
  before `RoundLimit` before `BudgetExhausted`), then the two *computed*
  predicates (`Converged`, `NoNewInformation`). This is what makes
  §5.5's "at standard depth the controller exits on `RoundLimit`, by
  construction" literally true: `RoundLimit` is checked (and wins) before
  `Converged`/`NoNewInformation` are ever consulted for control flow, even
  though both are still computed unconditionally every round and recorded
  on the output (`converged`/`no_new_information` fields) — "evaluated for
  the record... but they do not gate anything," exactly as stated.
- **`has_live_dissent_against` reads the *propagated* matrix's `Opposes`
  cells, not the raw relation graph a second time.** "no live attacker ≥
  τ_dissent against the top option" could mean re-deriving attackers from
  `Contradicts` relations against the option's supporting claims — but Step
  3 (`attachment::propagate`) already performs exactly that translation
  ("a claim contradicting a supporter of O counts against O, through the
  relation graph", ARCHITECTURE §5.3): an `Opposes` cell on the propagated
  matrix *is* a live attacker against that option once Step 3 has run.
  Reusing it is not a shortcut, it is the same computation surfaced through
  the API method Step 3 already exists to produce.
- **`no_new_information` is generically a set-difference of claim ids
  between rounds**, not hardcoded. Under this pipeline's current stage set —
  `rebuttal.run` never introduces a brand-new `CanonicalClaim`, only
  transitions/appends members to existing ones (D38) — `new_claim_count` is
  always `0` in practice, which correctly satisfies the predicate's first
  half rather than being a special case. If a later stage ever does
  introduce genuinely new claims inside the round loop (ARCHITECTURE §5.3's
  own allowance for "claims first stated in a rebuttal"), this function
  needs no change.
- **`converged_margin_factor` / `min_new_claims` / `min_standing_delta` live
  in `arbiter-kernel/src/bounds.rs`** (`DEFAULT_CONVERGED_MARGIN_FACTOR`,
  `DEFAULT_MIN_NEW_CLAIMS`, `DEFAULT_MIN_STANDING_DELTA`), not
  `arbiter-core`, per D5's explicit assignment — the pure predicate
  functions in `arbiter-core::decision::controller` take them as plain
  parameters rather than reading a core-owned config struct, so ownership
  and computation stay cleanly separated exactly as D5 intended.
- **Refactor of already-shipped G5 code, caught in passing.**
  `challenge_plan.rs` had hand-rolled `remaining_budget / remaining_rounds`
  and `(round_budget - judge_share).max(0.0)` arithmetic inline — duplicate
  logic K1/K2/K4 had already built, tested, and shipped as
  `bounds::round_budget`/`bounds::challenge_budget` before G5 was written,
  simply not discovered at the time. Replaced with calls to those functions;
  `challenge_plan.rs`'s own 5 tests were re-run unmodified and still pass,
  confirming the arithmetic is identical.

## D40 — `judge.evaluate`: dossier assembly, one shuffle for every judge, and where `Scorecard` aggregation actually happens

ARCHITECTURE §5.6 and INTERFACES §4 together give the dossier's contents and
the 9-metric rubric precisely, but neither gives an algorithm for
anonymisation mechanics, surface normalisation, or how multiple judges'
scores combine into the single `Scorecard` `decision::evidence::evidence_map`
already consumes. Authored in
`arbiter-kernel/src/stages/judge_evaluate.rs`:

- **One shuffle, shared by every judge in the round.** The pipeline diagram
  (§5.6) shows "shuffle" as a single step before "9-metric rubric", not one
  per judge. A fresh shuffle per judge call would be marginally more
  resistant to a hypothetical cross-judge identity-triangulation attack, but
  nothing in either spec file asks for that, and it would mean the same
  position appears at a different pseudonym in each judge's own dossier —
  purely a bookkeeping cost for a property not requested. One
  `DeterministicRng`-seeded Fisher-Yates shuffle (reproducible from the
  manifest seed like every other randomised choice this kernel makes) is
  computed once and reused for every judge's dossier this round.
- **`normalize_surface_form` is a reasonable subset, not a markdown
  parser.** "tables flattened, headings stripped, bullets unified" names
  three properties, not a grammar. Implemented line-by-line: a pipe-table's
  separator row (`|---|---|`, pure punctuation) is dropped entirely rather
  than rendered as noise; a content row's `|` delimiters become `, `;
  heading markers (`#`+) and the three common bullet glyphs (`-`, `*`, `+`)
  are stripped from line starts. "Length not truncated" is honoured by
  construction — nothing here shortens text, only reformats it.
- **Where `Scorecard` aggregation happens, and why two shapes survive.**
  `decision::evidence::evidence_map` (already wired into every
  `resolve_and_rank` call, D36/D39) needs exactly one `Scorecard` per model —
  `scores_by_model`, the mean of every judge's scorecard for that model,
  field by field. But `decision::confidence`'s `judge_dispersion` needs the
  *spread* across judges for one fixed dossier — averaging first would
  destroy exactly the signal it measures. So this stage's own output keeps
  both: `scores_by_model` (mean, ready for `evidence_map`) and
  `per_judge_scores: BTreeMap<ModelId, Vec<Scorecard>>` (every judge's own
  scorecard, undestroyed, for whichever position(s) `decision.synthesize`
  (G9) ultimately needs dispersion for). Neither `arbiter-core` function's
  own signature had to change — `confidence()` already takes `judges: &[Scorecard]`
  and this is exactly what it expects, once G9 selects which model's
  entry to hand it.
- **`FailurePolicy::DegradeWithEvent`, not `Fatal`.** Unlike `disputes.rank`/
  `challenge.plan`/`controller.decide` (pure computation, `Fatal`), this
  stage makes real provider calls — a judge's call failing or its response
  failing to parse must not fail the whole decision (a debate that can't get
  a second cross-vendor judge at `--depth deep` should still be scored by
  the first, not abort). A judge that contributes nothing simply leaves
  every position's `per_judge_scores` one entry shorter.
- **The dossier's "claims" section includes claim text, not just id and
  kind.** INTERFACES §4's own worked example elides it (`C-011 fact ·
  C-018 inference · …`), but a judge scoring Factual Correctness or Logical
  Reasoning needs to read the actual claim, not just its identifier and
  evidence kind — the elision reads as the doc's own brevity, not an
  instruction to withhold content the judge structurally needs to do its
  job.

## D41 — `decision.synthesize`: wiring C1–C8 to real artifacts, `Completeness`'s dependency problem, and three ambiguous ratios

ARCHITECTURE §5's own table gives this stage exactly two facts — "runs the
decision core," "calls no model" — and record.rs's own D18 note already
flagged the fields it deliberately deferred here. This task is almost
entirely wiring, with a handful of genuine gaps:

- **`resolve_and_rank` gains a `scores` parameter.** Every caller before
  this one (`disputes.rank`, `controller.decide`) runs before
  `judge.evaluate` does and always passed an empty judge-score map,
  previously hardcoded inside the function itself. `decision.synthesize` is
  the first caller with real scores (`judge.evaluate`'s `scores_by_model`,
  G8) to give it, so the empty map moved out to the two existing call
  sites (both re-tested unmodified, still passing) and `resolve_and_rank`
  now takes it as a parameter — the same "second/third consumer earns an
  extraction" precedent `similarity.rs` and `resolve_and_rank` itself were
  each built under.
- **`Completeness` cannot live in `arbiter-core`.** INTERFACES §9 types it
  as `Truncated { reason: StopReason, missing_stages: Vec<StageName> }` —
  both kernel types (`StopReason` in `stage.rs`, `StageName` in `ids.rs`),
  and `arbiter-core` cannot depend on `arbiter-kernel` (D1). `record.rs`'s
  own D18 note already anticipated this exact conflict without resolving
  it ("`StopReason`/`StageName` are pipeline/kernel concepts this pure
  decision core has no need of yet"). Resolved by defining `Completeness`
  in the kernel (`decision_synthesize.rs`) rather than smuggling
  kernel-typed fields into `arbiter-core`'s `DecisionRecord` — this
  stage's own output (`SynthesizedDecision`) wraps the untouched, C8-shipped
  `DecisionRecord` alongside a sibling `completeness` field, instead of
  reopening C8's already-tested type.
- **"Truncated" is not every non-`Converged` `StopReason`.** `RoundLimit`
  and `NoNewInformation` are both the controller *deciding* the debate was
  done (ARCHITECTURE §5.5: "at standard depth the controller exits on
  `RoundLimit`, by construction" — read as the normal, designed way a
  standard-depth run ends, not an interruption). Only the four genuinely
  external cutoffs — `BudgetExhausted`, `TokenLimit`, `Deadline`,
  `Cancelled` — plus `ProviderFailure` (a real failure, not an adaptive
  choice) count as `Truncated`. Treating `RoundLimit` as truncated would
  mark nearly every `--depth standard` run truncated by construction (§5.5
  itself says standard-depth rarely clears `Converged`'s bar in one round),
  which would be a strange, punitive reading of a mode the spec calls its
  own MVP default.
- **`missing_stages` is always empty.** No stage-execution tracking exists
  anywhere in this codebase (D39's own scope note) to know which stages a
  truncated run never reached — inventing that tracking is not this task's
  own scope, so the field is present (as the type requires) but honestly
  empty rather than guessed at.
- **Three ratios/means `OutcomeInputs`/`PenaltyInputs` are defined over but
  never given a derivation**, resolved in the new `arbiter-core::decision::synthesize`
  module:
  - `evidence_mass` ("mean standing of the claims decisive for the winning
    option") = mean standing over claims with a `Supports`/`Opposes` cell on
    top1 specifically in the *propagated* matrix — exactly the set
    `score_options` itself sums over for that option.
  - `unresolved_critical_ratio`/`assumption_dependency_ratio` ("decision-
    critical claims", no option named) = computed over the **union** of
    decisive claims across every live-scored option, not top1 alone — the
    two ARCHITECTURE phrasings ("decisive for the winning option" vs.
    "decision-critical") are read as deliberately different scopes, not the
    same set under two names.
  - `assumption_dependency_ratio`'s own "unverified assumption" is read as
    `EvidenceKind::Assumption` specifically, not `Unverified` too — a stated
    assumption and ungrounded extraction are different failure modes
    (`claims.extract`, D32) and conflating them would double-count claims
    `Unverified` already penalizes through `kind_weight` (§6.2) on its own.
- **`judges_for_confidence` reads the winning option's own authoring
  model(s), not a debate-wide average.** `confidence()`'s own signature
  (`judges: &[Scorecard]`) is agnostic to whose scorecards they are;
  `judge_score`/`judge_dispersion` feed the *decision's* confidence, so the
  literal, narrowest-scope reading is the judged quality of the case for
  the option actually being recommended — found via the direct matrix's own
  `Authored`+`Supports` cells on top1 (the same seeding `options.cluster`,
  D34, already performs), then that model's `per_judge_scores` entry from
  G8. An unattached top1 (no authoring model found) degrades to an empty
  judge slice, which `confidence()` already handles (`judge_score` 0.0,
  `judge_dispersion` `None`) rather than panicking or fabricating a score.

## D42 — L1: `arbiter run`, the first `StageGraph` executor, `SyntheticProvider`, and a `content_hash` collision found in already-shipped G4–G9 code

No `StageGraph` executor exists anywhere in the codebase before this task
(D39's own scope note explicitly deferred it); every one of G2–G9's stages
was previously only exercised by its own unit tests, each constructing its
`StageContext` and inputs by hand. L1 is the first task that actually
chains all thirteen stages together end to end against real persistence,
which is also why it is the task that surfaced the collision bug below —
no earlier task could have.

- **`RunHandle` bridges `EventSink` to the real store.** `EventSink::emit`
  (`arbiter-kernel/src/stage.rs`, fixed by every G2–G9 stage already
  shipped) returns `()`, not a `Result` — there is no channel for a store
  write failure to travel back through it without reopening every stage's
  own already-tested call site, which is out of this task's scope.
  Resolved by having `RunHandle` (`arbiter-cli/src/run_handle.rs`) record
  the first such failure and having the orchestrator poll
  `RunHandle::take_error()` after every stage completes, rather than
  losing the error silently or panicking inside a trait method whose
  signature this task does not own. `RunHandle` also owns the one open
  `RunWriter`/`ChainState` pair for the run's lifetime and is the sole
  caller of `put_artifact` — no stage calls it directly, consistent with
  every stage's own `run()` returning its artifact rather than persisting
  it.
- **The chain must continue from `init`'s own `RUN_STARTED`, not restart.**
  `arbiter_store::init::init` seals and appends `RUN_STARTED` against its
  own internal `ChainState` before `RunHandle` exists. Constructing
  `RunHandle` with a fresh, empty `ChainState` would make its first
  appended event wrongly claim `previous_event_hash: None` a second time,
  breaking `verify_chain` against what `init` already wrote. Caught by
  reasoning through chain continuity before ever running the code, not by
  a test failure; fixed by reading back `RUN_STARTED` via
  `sqlite_store.reader(&run_id)?.events()?.last()` immediately after
  `init` returns and seeding `RunHandle::continuing_from(Some(&run_started))`
  with it.
- **`--panel mock` only; real provider adapters (P3/P4) are out of
  scope.** `arbiter-providers`'s `MockProvider` is a hand-scripted
  `VecDeque` — correct for a fixture test that already knows its exact
  call sequence, useless for a command that must run the whole pipeline
  without knowing in advance how many candidate pairs T1 finds or how many
  rounds the controller runs. `SyntheticProvider`
  (`arbiter-cli/src/synthetic.rs`) instead inspects each call's *rendered*
  prompt text — which already contains the real interpolated claim/
  position text, since rendering happens before the provider ever sees it
  — and returns a plausible, schema-correct response by matching literal
  text this session's own shipped `prompts/default/v1/*.md` templates are
  known to contain. Any `--panel` value other than `mock` is rejected with
  an explicit error citing this entry, rather than silently degrading or
  attempting real network calls no credential-resolution path exists for
  yet.
- **Prompt-pack directory resolution order is not pinned by any spec
  section.** Resolved as: `ARBITER_PROMPTS_DIR` env var first, else the
  workspace-relative dev path baked in via `env!("CARGO_MANIFEST_DIR")` —
  a conservative default that works out of the box for a workspace
  checkout, overridable for any other layout.
- **`--stream` is only partially implemented this pass.** Every event is
  durably recorded through the real hash chain and readable via the store
  exactly as `--stream`'s non-streaming sibling would produce, but live
  mirroring of each event line to stdout as the run progresses is not yet
  wired — `run_command` prints a one-line notice to stderr instead of
  silently accepting the flag and doing nothing observably different.
- **Artifact `content_hash` collision across two different artifact types,
  found by this task's own end-to-end integration testing and fixed
  across twelve already-shipped files spanning G4 through G9.** The
  `artifacts` table's primary key is `artifact_id` (= `content_hash`), and
  `put_artifact` is `INSERT ... ON CONFLICT(artifact_id) DO NOTHING`
  (S1/S2's idempotency contract, exercised by its own
  `put_artifact_is_idempotent_on_identical_content` test — correct for its
  intended case, a retried write of the *same* artifact). None of the
  nineteen `Artifact::content_hash()` implementations across
  `arbiter-kernel/src/stages/*.rs` mixed the artifact's own `artifact_type()`
  into its hash, so two structurally different artifact types could hash
  identically whenever their type-specific "extra" content happened to be
  empty and their shared sub-hash was otherwise the same input.
  Concretely: `ChallengePlanned` (empty `pairs`) and `ChallengesIssued`
  (empty `challenges`, same unchanged `resolved` graph) both reduced to
  `blake3(ranked_disputes_hash \u{1} "[]")` whenever a round produced zero
  challenge pairs, so the second `put_artifact` call silently no-op'd and
  `challenges_issued.v1` vanished from the persisted log — a real,
  user-visible data loss that no G4–G9 unit test could have caught, since
  no earlier test ever persisted two different artifact types into the
  same physical table where a cross-type collision could actually
  manifest; it took this task's own multi-stage, real-store run to expose
  it. Fixed by adding `self.artifact_type()` as a hash-mixing prefix to
  all nineteen implementations (not just the two that collided, since the
  same structural gap existed in every one of them), across
  `decision_synthesize.rs`, `rebuttal_run.rs`, `controller_decide.rs`,
  `challenge_run.rs`, `challenge_plan.rs`, `relations_analyze.rs`,
  `claims_normalize.rs`, `claims_extract.rs`, `positions_generate.rs`,
  `options_cluster.rs`, `disputes_rank.rs`, and `judge_evaluate.rs`.
  Verified via a full `cargo test --workspace` before and after (identical
  pass counts — no existing test asserts on an exact hash *value*, only on
  hash properties like determinism and equality-under-reordering) and a
  direct SQLite query confirming all twelve distinct artifact types now
  persist per run, up from eleven.

## D43 — L2: `show`/`explain`/`claims`/`history` — the missing artifact-read path, `defeat_chains`, and what a finished run still doesn't persist

L2's own file scope (`arbiter-cli/src/`) suggested this would be wiring
against already-built primitives, the way L1's own scope note first assumed
before finding no `StageGraph` executor existed. The same thing happened
here: `RunReader` (K0/S2) exposed `events()`/`verify_chain()` only — nothing
could read an artifact back out of a finished run at all — and most
artifacts' own `to_json()` (G3–G9) was written minimal, sufficient only for
`content_hash` and audit, never meant to round-trip enough data to *render*
anything. Both gaps are this task's own to close, since no other task's
scope names them and `show`/`explain`/`claims` cannot exist without them.

- **`RunReader::artifacts_by_type(artifact_type) -> Vec<Value>`, new.**
  Ordered by SQLite `rowid` (insertion order), not `created_at` — the
  latter's string timestamp cannot reliably distinguish two puts issued
  within the same millisecond, which a synthetic-provider run routinely
  does. A caller after "the current state" (e.g. the round loop's last
  `controller_decision.v1`) takes `.last()`.
- **`RankedDisputes::to_json()` extended to carry the full resolved graph**
  (`claims`, `relations`, `options`, `propagated_matrix` cells), not just
  `standing`/`ranked` as G5 shipped it — the only artifact in a finished run
  that still carries claim text, relation edges and attachment once the
  round loop has moved past `disputes.rank`. `content_hash` was extended to
  mix in the propagated cells too, so this is a genuine content change, not
  cosmetic — re-verified via the full workspace test suite (unchanged pass
  counts, same reasoning as D42: no test pins an exact hash value).
- **`DecisionRecord` gains `claim_standings: BTreeMap<ClaimId, ClaimStanding>`**
  (`arbiter-core`, C8's own type). `build()` already computed this map
  internally to produce `ClaimCounts`/`unresolved_claims`; it was discarded
  rather than stored. `arbiter claims --state agreed|disputed|unresolved|
  defeated` has no other source for the three non-unresolved states — no
  persisted artifact carries a per-claim classification otherwise. Zero new
  parameters: `build()`'s existing `claim_standings` argument is now also
  cloned onto the record it returns.
- **A new `arbiter-core::decision::explain` module, `defeat_chain_for`,**
  reconstructs INTERFACES §22's `defeat_chains` (`steps` with `by`/
  `relation`/`attacker_standing`/`weight`/`delta`, plus `saturated`) from
  data the fixpoint already computed — the final `standing` map and the
  relation list that produced it — rather than a second, possibly-drifting
  computation. Per-edge `delta` pro-rates the aggregate `gain * min(raw,
  cap)` term back across the edges that produced `raw`, so summing every
  step for one claim reproduces the fixpoint's own term exactly, capped or
  not. This is arithmetic decomposition of an already-computed number, not
  new decision logic invented for the CLI's sake — the same standard C8's
  `explain_confidence` was already held to. **Not reproduced:** the worked
  example's separate `"evidence"` field (`E(c)`) — recomputing it exactly
  needs the claim's judge scores and lifecycle
  (`decision::evidence::evidence`'s own signature), and no persisted
  artifact joins a finished claim back to either. Omitted rather than
  guessed at.
- **Decision-level `defeat_chains` selection (no `claim_id` given) is a
  product choice, not a spec rule.** INTERFACES §22 shows one entry even at
  decision scope but does not say which claims qualify. Chosen: every
  unresolved or disputed claim, plus every claim named in
  `change_triggers`, deduplicated and capped at 10 — the claims actually
  driving "why not more confident" and "what would flip this," rather than
  the whole graph.
- **`arbiter history` needed `history.db` writes L1 never added** — L1's own
  scope was `run` alone, and its acceptance test only checked the printed
  decision, not the catalogue. `run_command` now opens `history.db` as
  `--store`'s parent directory (ARCHITECTURE §8's own sibling layout:
  `history.db` next to `runs/`) and calls `insert_running`/
  `update_completion` around the pipeline call. Best-effort: a catalogue
  write failure (e.g. a read-only filesystem) does not fail the run, since
  the run itself is still fully persisted and replayable — only its
  catalogue row is missing, exactly the gap `arbiter reindex` (S6) already
  exists to repair.
- **`Completion.cost`/`orphaned_cost` are written as `0.0`.** No aggregate
  budget reader exists anywhere yet to source a run's total committed spend
  from — the same honest gap `reindex`'s own doc comment already leaves for
  columns it cannot yet derive, applied here to the write path instead of
  the read path. `model_count` (`cfg.panel.len()`) and `margin` (reusing
  `confidence.dimensions`' own `"decision_margin"` entry, not re-derived)
  needed no such gap.
- **`list_runs`'s `--since` takes an RFC3339 timestamp**, matching
  `started_at`'s own column format — ARCHITECTURE names the flag but not
  its value format.
- **Relation/polarity strings inside a resolved-graph payload are parsed by
  literal match, not `serde`.** `AnalyzedRelations`/`AttachmentMatrix`'s own
  `to_json()` (G4/G3) writes `format!("{:?}", kind)` (`"Contradicts"`), not
  the type's own `#[serde(rename_all = "snake_case")]` form
  (`"contradicts"`) — so the CLI's read-side view structs match the actual
  literal string the artifact contains, rather than assuming a `Deserialize`
  round-trip that was never wired on the write side for these fields.
  `DecisionRecord` itself has no such mismatch (its `to_json()` serializes
  the real struct via `serde` directly), which is why `read_decision_record`
  needs no equivalent workaround.

## D44 — L3: `resume`/`replay` — the response cache was never persisted, and three more read/write primitives nothing had called yet

Like L1 and L2 before it, L3's own file scope (`arbiter-cli/src/`) suggested
wiring against already-built primitives. It mostly was — `classify_on_resume`
(K3), `get_for_replay` (K3), `Tx::put_cache`/the `cache_entries` table (S4)
all existed, fully unit-tested, with no caller anywhere in the codebase.
Building `resume`/`replay` is what gives every one of them its first caller.
One gap surfaced only by actually running the result end to end, the same
way D42's collision bug did:

- **`cache_entries` had never been written to by any run, ever.** A
  `Stage`'s `StageContext::cache: &ResponseCache` is a bare in-memory
  structure with no store handle of its own — `ResponseCache::put`
  (`arbiter-kernel/src/cache.rs`) only ever updated its own `Mutex<BTreeMap>`
  view. Discovered by running `arbiter run` then `arbiter replay` against
  the same run id and getting `InsufficientEvidence` back instead of the
  original `SplitDecision`: `reader.cache_entries()` returned empty, every
  call missed, `ReplayProvider` (below) refused each one, and the pipeline's
  own per-item degradation (`SkipItem`, ARCHITECTURE §8.4) quietly absorbed
  every refusal into a claims-less, judge-less "successful" run instead of
  a loud failure — a worse outcome than an error would have been, and the
  only reason the bug was visible at all was that `replay`'s own
  stored-vs-recomputed comparison (below) flagged the mismatch. Fixed with
  `ResponseCache::snapshot()` (every entry currently held) and
  `RunHandle::put_cache_entry` (writes one through `Tx::put_cache`);
  `arbiter run`'s own `run_command` now persists a snapshot after every
  pipeline invocation, succeeded or not, closing the same gap for the
  command that already existed. Not incremental — a process that crashes
  before reaching this point still loses that attempt's cache, which is
  the honest limit of a fix scoped to `arbiter-cli` rather than threading a
  persistence hook through `StageContext` into every G-stage call site.
  `resume`'s other recoveries (reservation release, orphaned-spend
  reporting, budget capping) do not depend on it and work regardless.
- **Three new `RunReader`/`Tx` primitives, none of which existed before
  this task needed them:** `RunReader::cache_entries` (rehydrates a fresh
  process's `ResponseCache` from what a prior one persisted — `resume`'s and
  `replay`'s shared mechanism for serving already-answered calls for free,
  via `run_pipeline`'s own cache-before-call order, D31, with zero changes
  to any stage); `RunReader::provider_calls` and `RunReader::budget_totals`
  (`resume`'s inputs — the first gives `classify_on_resume` real rows to
  classify, the second lets a freshly built `BudgetLedger` be capped at what
  is actually left of the hard cap instead of the full amount again);
  `Tx::release_reservation` (`resume`'s `RESERVED → FAILED` branch,
  ARCHITECTURE §8.4 — `commit_budget` cannot serve this, since it
  unconditionally marks the call `Completed`).
- **`run_pipeline` (`arbiter-cli/src/orchestrator.rs`) now takes `budget`/
  `cache` as caller-supplied references** instead of constructing
  `BudgetLedger::new(...)`/`ResponseCache::new()` itself — the one change to
  L1's own executor, needed so `resume`/`replay` can seed both before a
  single stage runs. Every stage call inside it is untouched; `arbiter run`
  passes the same fresh pair it always implicitly built.
- **`ReplayProvider`** (mirrors L1's `SyntheticProvider`) always errors —
  replay is cache-only by construction (a rehydrated cache plus a provider
  that can never succeed), rather than threading a "replay mode" flag
  through every stage to reach `ResponseCache::get_for_replay` (K3) that
  particular way. Structurally the same "opens no socket" guarantee, one
  layer up.
- **`NullWriter`/`NullTx`** discard every write — `replay` must never mutate
  the run it is replaying (nothing in ARCHITECTURE describes replay as a
  second execution of the same run id; `--repolicy`/`--repack` are the two
  operations that mint a new one), so `RunHandle` is given a writer that
  accepts and drops everything rather than a second connection onto the
  real `run.db`.
- **`replay` cross-checks its own recomputed record against the one already
  on record** (via `render::read_decision_record`) and warns, rather than
  failing, on a mismatch — cheap since both are already in hand, and it is
  what caught the cache-persistence bug above in the first place, before
  the acceptance test's own `diff` would have.
- **`--repolicy`/`--repack` are not implemented.** Both "mint a new run id"
  (ARCHITECTURE §7/§15) — a materially different code path (a new `init`,
  not a reopen) from exact replay's own "verify this run reproduces
  itself." `replay`/`resume` both refuse outright if the recorded
  `policy_version`/`pack_hash` differs from what this build would use,
  rather than silently attempting a re-derivation with no rule for how one
  should work yet.
- **`depth` has nowhere durable to live except `history.db`.** Neither
  `Manifest` (frozen at `init`) nor any artifact carries it; `resume`/
  `replay` read it back from `run_catalog.depth` (L2's own write path), and
  degrade to `Standard` with a loud warning if that row is missing or was
  never written (a best-effort catalogue write, D43, or a run pre-dating
  L2's catalogue wiring). No round loop can be correctly replayed under the
  wrong depth, so this is a real limitation, not just cosmetic — logged
  rather than silently guessed at.
- **`arbiter resume` on an already-finished run** (a `RunCompleted`/
  `RunFailed` event already present) short-circuits to printing the stored
  decision rather than re-running anything — otherwise a second `resume`
  call would needlessly re-walk an already-complete pipeline through a live
  writer, growing the event log for no reason.

Verified end to end: `arbiter run` then `arbiter replay --json` produces
output **byte-identical** to `arbiter show --json` for the same run
(the IMPLEMENTATION_PLAN.md L3 acceptance test's own comparison) once the
cache-persistence fix above landed; `arbiter resume` against a run holding
one genuinely `RESERVED` provider call (simulated directly against the
store) correctly reports "released 1 reservation" and completes the run;
a second `resume` against the now-completed run correctly reports nothing
to resume. Full workspace: build clean, `cargo test --workspace` green
(43 arbiter-store, up from 41 for the two new store-primitive tests; all
other crates' counts unchanged), `cargo fmt --all -- --check` clean,
`cargo clippy --workspace --all-targets --all-features -- -D warnings`
clean.

## D45 — L4: `accept`/`doctor`/`reindex`/`export`/`keys`/`providers` — a real event-id collision, a real `doctor` false positive, and P3/P4's own honest stub

L4's own dependency line in IMPLEMENTATION_PLAN.md names `L2, P3` — and P3
(credential resolution: OS keychain, write-path redaction, secret
zeroization) is explicitly deferred to its own pass by that same plan's own
P1-P4 scope note ("security-sensitive... deserves focused attention and its
own review rather than being rushed alongside P1/P2"), with P4 (real HTTP
adapters) deferred alongside it for the same reason. Rather than block this
entire task on a dependency the plan itself already chose to defer, L4 is
split along the line the plan's own scope note draws: `accept`, `doctor`,
`reindex`, `export` need nothing from P3/P4 and are built for real;
`keys`/`providers` are the two subcommands that genuinely cannot exist
without it, and get honest stubs that name the gap rather than fabricate
credential state or a provider roster that doesn't exist.

- **`arbiter export <run_id> --format json|markdown|ndjson`**, initially
  missed on this task's first pass — its full command line appears only in
  IMPLEMENTATION_PLAN.md's own detailed L4 command table (`accept
  [--override...], keys ..., providers ..., doctor [--gc], reindex,
  export --format`), not in the short summary row this task's scope note
  was first read from. Added once re-checking the plan against what had
  actually shipped surfaced the gap. Writes to `<run_dir>/exports/`,
  matching the directory tree's own comment (§8's layout: "exports/ --
  anything the operator asked for"). Distinct from §8.6's own "Copying a
  run" (`VACUUM INTO` + a recursive blob copy) — that section describes a
  filesystem-level backup/transport operation with no format choice, not
  this command's own `--format` rendering; `json` is the stored
  `DecisionRecord` (matching `show --json`), `markdown` a human-readable
  report of the same, `ndjson` the run's own event log, one envelope per
  line (§8.8: "NDJSON... remains the interchange format").

- **`DecisionAcceptance`/`DecisionOverride`** (INTERFACES §17), new in
  `arbiter-core::acceptance` — pure recorded data, not computed, the same
  category `DecisionRecord` itself is. `DecisionOverride::from` is always
  `Value::Null`: INTERFACES §17's own example (`path:
  "technical.cloud_provider"`) is a *Build Studio* document field, and Build
  Studio (ARCHITECTURE §13) does not exist in this codebase — there is no
  generated spec to read a prior value from, so it is left honestly absent
  rather than guessed at. `arbiter accept` persists the acceptance as a new
  `decision_acceptance.v1` artifact and emits `DecisionAccepted`/one
  `DecisionOverridden` per override, refusing outright if the run has no
  synthesized decision yet or an override arrives with an empty `--reason`
  (INTERFACES §17: "an unexplained override is rejected"). It never checks
  or blocks anything against the record it writes — the gate it exists to
  feed (`arbiter build`, Build Studio) is out of scope, so `accept` is
  write-only.
- **A real bug, found by `accept` itself: `RunHandle`'s event ids only stay
  unique within one process.** `RunHandle::append`'s id was
  `evt_<run_id>_<counter>`, with the counter restarting at 1 on every
  `RunHandle::new` — fine for `arbiter run`, which only ever constructs one
  `RunHandle` per run, but `accept` is the first command to construct a
  *second* `RunHandle` against a run that already has a full event history
  from a first one, and its counter's `000001` collided with an id the
  first process had already used, failing loudly on `events.event_id`'s own
  `UNIQUE` constraint. `resume` (L3) carried the same latent bug — invisible
  there only because the one scenario it was tested against (a run
  interrupted after `init` but before any stage-driven event) had no prior
  `RunHandle`-minted events to collide with. Fixed by mixing a
  nanosecond timestamp captured once per `RunHandle` construction into
  every id it mints (`evt_<run_id>_<instance_tag>_<counter>`), making each
  instance's own id space disjoint from every other's without needing to
  read prior state back first. Re-verified: `replay --json` still
  byte-identical to `show --json` after the fix (`NullWriter` never
  persists ids, so it was never exposed to this bug, but the fix still had
  to not regress it), and `accept`/`resume` both exercised against a run
  with ~75 pre-existing events, cleanly.
- **A real `doctor` false positive, found by running it against a normal,
  cleanly-completed run: every finished run was reported "stuck in
  running."** A dead lease (`is_run_lease_live`, S5) is not, on its own,
  evidence of an abandoned run — a run that finishes normally also ends with
  no live owner, since the process just exits without taking any special
  action on the `run` table. Fixed by only counting a dead-lease run as
  stuck when its own event log also lacks a `RunCompleted`/`RunFailed`
  event — the same "did this run actually finish" check `resume` (L3, D44)
  already makes before deciding whether there is anything to resume.
- **`doctor`'s ledger-invariant check** recomputes `Σ reserved_amount` over
  every `provider_calls` row in a non-terminal state (`CallState::
  is_non_terminal`, already built) and flags any run where that disagrees
  with `budget`'s own persisted `reserved` column — the invariant
  `arbiter-kernel/src/budget.rs`'s own doc comment already states, given its
  own first real reader here. Orphaned spend and (with `--gc`) blob
  reclamation reuse `provider_calls`/`cache_entries` (L3) and `blob::gc_run`
  (S5) exactly as built, with zero changes to either.
- **`keys`/`providers` are honest stubs, not fabricated ones.** `keys list`
  and `providers list` state plainly that no credential source is resolved
  and no real provider adapter exists in this build, and that `mock` is the
  only provider `--panel` can select; `keys set/test/rm` and `providers
  test` refuse outright, naming P3/P4 as the reason, rather than pretending
  to read a keychain or make a network call that has nowhere to go.
  `doctor`'s own "credentials: not available" line says the same for the
  same reason.

Verified end to end: `reindex` against a real store; `doctor` against a
real completed run (no false "stuck" report after the fix) and a
simulated interrupted one; `accept` with and without `--override`, and its
rejection of an unexplained override; `export` in all three formats
(content checked, not just exit status) plus its rejection of an unknown
`--format`; `keys`/`providers` list and their refusals. Full workspace:
build clean, `cargo test --workspace` green (all
counts unchanged — this task added no new unit tests, verified instead by
the CLI-level regressions above, the same split this session has used
throughout for CLI-only code), `cargo fmt --all -- --check` clean, `cargo
clippy --workspace --all-targets --all-features -- -D warnings` clean.

## D46 — P3: credential resolution + redaction — a lint/spec conflict, `unsafe`-free env tests, and what stays untestable in this sandbox

New `arbiter-providers::keys` (ARCHITECTURE §11.1, INTERFACES §25): the
three-source resolution order, `SecretString`, `KeyState`/`KeySource`,
config-file scanning, a `Redactor`, and the 24h verification cache.
`IMPLEMENTATION_PLAN.md`'s own P1-P4 scope note had already deferred P3 as
"security-sensitive... deserves focused attention and its own review" —
this task is that pass.

- **`SecretString` has a manual, redacting `Debug` impl, not no `Debug` impl
  at all.** INTERFACES §25's own words are "`SecretString` does not
  implement `Display` or `Debug`." Taken completely literally that conflicts
  with this workspace's own `missing_debug_implementations = "warn"` lint
  (every crate's `[lints.rust]`), which under this session's own
  `-D warnings` quality gate turns "no `Debug` impl on a public type" into a
  hard build failure. The spec's own reasoning names the actual goal: "the
  most common way a secret reaches a log is a struct derived with
  `#[derive(Debug)]` three layers up from anything that knows it holds
  one." A hand-written `Debug` that always prints `"[REDACTED]"` satisfies
  that goal exactly — an outer struct's own `#[derive(Debug)]` calls this
  impl for its `SecretString` field and never sees the plaintext — while
  also satisfying the lint. `Display` is never implemented, full stop, per
  the letter of the spec (nothing needs it). `Redactor` gets the same
  treatment for the same reason: it holds the plaintext secrets it scans
  for, so its own `Debug` reports only a count, never the values.
- **`KeySource::ArbiterEnv`/`ProviderEnv` hold owned `String`s, not
  `&'static str`.** INTERFACES §25's literal signature
  (`ArbiterEnv(&'static str)`) cannot compile as given: the env var name is
  derived per-provider at runtime (`ARBITER_{PROVIDER}_API_KEY`), and no
  runtime `String` can be borrowed as `'static` without leaking it. The
  same class of "the literal signature doesn't compile as written" resolved
  by adjusting it that K0's own D19/D20 already established a precedent
  for.
- **`SecretString::fingerprint()` returns the first 16 hex *characters* of
  `blake3(key)`, not 16 bytes.** ARCHITECTURE §11.1's own prose is
  `blake3(key)[..16]`; its own `GET /api/providers` JSON example shows a
  4-character fingerprint (`"4f2c"`), which reads as the example's own
  shorthand for display, not a literal width contradicting the prose's
  explicit slice notation. 16 hex characters matches every other
  fingerprint-shaped string already in this codebase (`content_hash`,
  `pack_hash`, ...).
- **Tests for the two environment-based resolution sources inject a fake
  lookup instead of mutating real process env vars.** `std::env::set_var`/
  `remove_var` are `unsafe fn` as of this edition — real env vars are also
  global, process-wide mutable state shared across every test in the
  binary regardless. `#![forbid(unsafe_code)]` (every crate) blocks
  `unsafe` even inside `#[cfg(test)]`, so mutating real env vars in a test
  was never an option here. The actual resolution logic
  (`resolve_from_env`) is factored out to take an injectable lookup
  closure; `EnvCredentialSource` calls it against `std::env::var` for
  real, tests call it against a local `BTreeMap` — same logic, no shared
  mutable state, no `unsafe`.
- **The OS keychain source (`KeychainCredentialSource`, via the `keyring`
  crate) is real wiring, not a stub — but its actual round-trip cannot be
  exercised in this sandbox.** No D-Bus session bus is running here (Linux
  Secret Service needs one), and there is obviously no real macOS/Windows
  to test the other two backends against — the same class of gap P4 was
  already deferred for ("no CI-testable acceptance criterion"), just
  narrower: only this one resolution source, not the whole task. Verified
  live instead: `arbiter keys set anthropic` against this sandbox fails
  with a real, honest `keyring` error ("No default store has been set"),
  exactly the failure mode expected with no backend reachable — not a
  silent no-op, not a fabricated success.
- **Config-file scanning is a line scan for an `api_key`-shaped
  assignment, not a full TOML-schema check.** ARCHITECTURE §11.1 asks for
  catching the *shape* wherever it appears across several files
  (`config.toml`, `.arbiter/config.toml`, a plugin's `plugin.toml`) that
  are never read for a key — no config-loading module exists anywhere in
  this codebase yet to define what a validated schema for any of them
  would even be, and inventing one is not this task's job. The scan takes
  a list of candidate paths (a missing one is not an error — most
  candidates never exist on a given machine) and reports the first
  key-shaped line's file and 1-indexed line number, matching the
  acceptance test's own name exactly.
- **The verification cache is in-memory only**, the same "no persistence
  path yet, since nothing that would populate it exists as a live command
  yet either" precedent `ResponseCache`/`BudgetLedger` were also built
  under before L1/L3 gave them one. `arbiter keys test` (the only thing
  that would ever call `VerificationCache::put`) still needs P4 to make
  the real request it would cache the result of.
- **The `Redactor` type is built and tested in isolation; it is not yet
  wired into any real write path** (event payloads, cached responses, the
  manifest, exports, error strings) ARCHITECTURE §11.1 names. There is no
  live secret flowing through any of those paths yet — `--panel mock` (the
  only panel this codebase can run, L1) needs no key at all — so there is
  nothing to integration-test the wiring against until P4 lands a real
  adapter that actually makes a network call with a real key. Deferred
  rather than wired in speculatively.
- **L4's `keys`/`providers` CLI stubs were upgraded in this same task**,
  not left stale: `keys list/set/rm`, `providers list`, and `doctor`'s own
  credential line now report/act on real state via
  `arbiter-providers::keys`, for the one provider id this codebase's spec
  ever names (`anthropic`) plus `mock` (needs no key). `keys test` and
  `providers test` still refuse, now naming P4 specifically rather than
  P3, since P3 no longer blocks them — only the real adapter that would
  make the verification request does.

Verified: `cargo test -p arbiter-providers` — 17 passed (5 pre-existing
`mock` tests unchanged, 12 new); `keys list` against a clean environment and
again with `ARBITER_ANTHROPIC_API_KEY` set, both correct; `providers list`;
`keys test`/`providers test` refusing and naming P4; `keys set` failing
honestly against this sandbox's absent keychain backend; `doctor`'s
credential line reflecting the same real state. Full workspace: build
clean, `cargo test --workspace` green (17 arbiter-providers, up from 5; all
other crates unchanged), `cargo fmt --all -- --check` clean, `cargo clippy
--workspace --all-targets --all-features -- -D warnings` clean.

## D47 — F1/F2: the fixture harness and 32 of the 36 CI fixtures — a toolkit, not a second pipeline, four fixtures genuinely blocked, and three real gaps the fixtures themselves found

F1 (`arbiter-fixtures/src/harness.rs`) and F2 (`arbiter-fixtures/tests/*.rs`,
32 of the 36 fixtures in IMPLEMENTATION_PLAN.md §8's ledger).

- **F1 is a toolkit, not a wrapper around `arbiter-cli::orchestrator::run_pipeline`.**
  `arbiter-fixtures` cannot depend on `arbiter-cli` (X2's own
  `nothing_depends_on_cli` test), so `run_pipeline`'s wiring is simply
  unreachable from this crate. `harness.rs` instead builds the same
  primitives (`ProviderRegistry`, `BudgetLedger`, `ResponseCache`, a
  `RecordingSink` `EventSink`, a `StageContext` constructor, a
  minimal-construction `PromptTemplate` helper) directly against
  `arbiter-kernel`'s already-public API — the same primitives
  `orchestrator.rs` itself uses, wired independently. Most of the 36
  fixtures need only `arbiter-core`'s pure functions or one or two kernel
  stages called directly, not a full 13-stage run, so this is deliberately
  a set of shared helpers a fixture assembles from, not a second copy of
  `run_pipeline`. Only `simple_consensus` (F2's own fixture, the one the
  ledger names as genuinely end-to-end) wires every stage — done directly
  in that test itself, following `orchestrator.rs`'s exact stage order,
  since a second reusable "mini-orchestrator" inside the harness would be
  exactly the unrequested abstraction this project's own discipline avoids
  for a single caller.
- **Four fixtures are genuinely blocked, not built:**
  - `serve_localhost_only`, `serve_rejects_foreign_origin` — owned by U1
    (`arbiter serve`), which does not exist anywhere in this workspace.
  - `panel_without_keys` — owned by U2 (a UI screen), which does not exist
    anywhere in this workspace.
  - `cites_defeated_claim` — owned by C1, but "an assertion citing a
    defeated claim" needs a `Provenance`/`Assertion` type from Build
    Studio (INTERFACES §18), and Build Studio (ARCHITECTURE §13) is
    explicitly out of this plan's 1.0 scope. No such type exists anywhere
    in `arbiter-core` (confirmed by grep: the only match anywhere in this
    workspace is a single doc-comment mention of `Provenance::UserOverride`
    in `arbiter-core::ids`, D45's own scope note).
  This leaves 32 buildable fixtures, all 32 written and passing.
- **The ledger's `G4` ownership for `premise_cycle`/`premise_cycle_grounded_fact`
  is a labeling mismatch already covered by D32, not a new gap.**
  IMPLEMENTATION_PLAN.md's own G2-G9 stage table (line 907) also describes
  "premise cycles: Kahn sort, minimum edge cut..." against
  `relations.analyze`, but the actual mechanism (`topo_sort`,
  `cut_cycle_edges`, the untangle-then-degrade sequence) lives entirely in
  `claims.extract` (`arbiter-kernel/src/stages/claims_extract.rs`) — G2's
  own scope note already documents this in full, and D32 is the deviation
  that first recorded it. `g4_premise_cycles.rs` builds both fixtures
  against `ClaimsExtract`'s real `Stage::run`, not against
  `relations_analyze.rs` (which has no cycle-handling code to call).
  Separately noticed while locating this: `EventType::PremiseCycleDetected`
  (`arbiter-kernel/src/event.rs`) is declared but never emitted anywhere in
  this workspace — `claims_extract.rs`'s cycle path only ever emits
  `ClaimUngrounded` for whatever remains ungrounded after the untangle
  sequence runs, never a distinct event when a cycle is first detected.
  Not fixed here (emitting it correctly is `claims.extract`'s own scope,
  already closed, and inventing an emission site now — with no consumer
  anywhere that reads it — would be exactly the unrequested scope creep
  this project's discipline avoids); flagged here since a future task
  wiring the Integrity event family end-to-end will need to know this one
  is currently a dead enum variant.
- **`judge_failure`'s "retry" wording has no literal counterpart in
  `judge.evaluate`.** ARCHITECTURE §18's own proves-line reads "invalid
  judge JSON → retry → judge term degrades," but `JudgeEvaluate::run`
  makes exactly one call per judge with no per-judge retry loop anywhere —
  an unparseable response is simply skipped (this file's own
  `an_unparseable_judge_response_degrades_without_scoring_anyone` proves
  it). The fixture built instead proves the part that is real: a model
  still gets scored from its remaining valid judges when one judge's
  response fails to parse, rather than the whole evaluation failing.
- **`decision_override`'s "provenance carries UserOverride" is unreachable
  from this crate for two independent reasons, both already logged.**
  `Provenance::UserOverride` is a *generated Build Studio spec's* own
  pointer type (D45, and this entry's own `cites_defeated_claim` note
  above) — no `Provenance` type exists anywhere to carry that variant.
  Separately, `arbiter accept`'s own validation ("an unexplained override
  is rejected," INTERFACES §17) lives in `arbiter-cli::accept_command`,
  unreachable per the dependency rule. The fixture instead proves what
  *is* real and in this crate's reach: `DecisionAcceptance`/
  `DecisionOverride` (`arbiter-core::acceptance`, D45's own scope) carry
  the full override audit trail — id, path, from, to, reason — intact
  through a real serialize/deserialize round trip, the actual persistence
  path `accept` uses.
- **`arbiter-fixtures` gained a `rusqlite` dev-dependency** (workspace
  version, matching every other crate that uses it) for
  `interrupted_commit`/`projection_rebuild`, which need to drive
  `arbiter-store`'s real `SqliteRunStore`/`rebuild_operational_projections`
  against a throwaway on-disk or in-memory database. `arbiter-store` was
  already a normal dependency of this crate; only the direct `rusqlite`
  import needed adding, to construct `Connection`s and raw query rows the
  same way `arbiter-store`'s own test modules already do. Verified this
  does not violate the dependency rule: `cargo test -p
  arbiter-workspace-checks --test dependency_rule` (all five rules,
  `nothing_depends_on_cli` included) still passes — `rusqlite` is an
  external crate, outside the internal `arbiter-*` graph the rule checks.

Verified: `cargo test -p arbiter-fixtures` — 32 fixtures, all passing, no
fix-up cycles needed on 29 of them (every API signature looked up against
the real source before writing the call). Full workspace: `cargo test
--workspace` green (all pre-existing suites unchanged), `cargo fmt --all --
--check` clean, `cargo clippy --workspace --all-targets --all-features --
-D warnings` clean.

## D48 — U1: `arbiter serve` — an endpoint-count mismatch, a `Sec-Fetch-Site` reading, an invented SSE envelope, and the HTTP crate choice

New `arbiter-cli/src/serve/` (ARCHITECTURE §17.1, INTERFACES §24): the
loopback-only HTTP server, its five-step admission gate, and the eight
endpoints IMPLEMENTATION_PLAN.md's own U1 table names.

- **The plan's own U1 table lists eight endpoints; ARCHITECTURE §17.1's own
  prose says "one embedded HTML page and five endpoints."** The plan's
  table is the more detailed, concrete, and directly testable of the two
  (it names every method/path/response this task's own acceptance
  commands exercise), so it is what got built — `GET /`, `POST /api/runs`,
  `GET /api/runs`, `GET /api/runs/:id`, `GET /api/runs/:id/events`, `POST
  /api/runs/:id/accept`, `GET /api/providers`, `POST
  /api/providers/:p/test`. Read as the plan's own elaboration of an
  imprecise summary count in the higher-ranked spec document, not as a
  conflict the plan loses under §0.1's authority order — nothing in
  ARCHITECTURE names a *different* five endpoints this task should have
  built instead, only a smaller number than the plan's own detailed table.
- **`Sec-Fetch-Site` admission does not reject the literal ARCHITECTURE
  §17.1 wording ("absent, or same-origin").** A real browser's Fetch
  Metadata header carries `none` on any direct, user-initiated navigation
  — exactly what happens the instant `--open`'s own printed URL is
  followed, or a bookmark is used. Rejecting anything but "absent or
  same-origin" literally would 403 that very first page load. Implemented
  instead: refuse only `cross-site` (the drive-by-form-post case this
  requirement exists to stop), matching the documented Fetch Metadata
  mitigation this table is otherwise quoting almost verbatim
  (`admission::sec_fetch_site_is_acceptable`). No acceptance test in the
  plan's own 9-command list constrains this value directly, so this is the
  conservative-in-the-sense-of-still-functioning reading, not a guess with
  no fallback.
- **The SSE envelope has no existing `--stream` output to match "byte-for-byte"
  against.** ARCHITECTURE §17.1 says the stream is "byte-identical to
  `--stream`," but `--stream` itself does not emit anything yet — L1's own
  scope note (D42) left it printing a one-line notice instead, since the
  event log is durably recorded and readable from the store either way.
  `GET /api/runs/:id/events` instead emits each event as the same JSON
  shape `arbiter show --transcript --json` already produces
  (`serde_json::to_value(&Event)`) — a real, already-existing, already-
  serde'd shape, not an invented one — with the event's own `sequence` as
  the SSE `id:` field, which is what makes `Last-Event-ID` resume correct
  regardless of what the eventual `--stream` implementation ends up
  choosing to look like.
- **The HTTP server crate is `axum` (0.8), a decision neither spec file
  pins.** ARCHITECTURE §17.1's "no build step, no npm, no bundler, no
  framework, no CDN" is about the embedded page's own JS/CSS (U2-U7's own
  scope), not the Rust process serving it — nothing rules out a normal Rust
  web crate for the backend, and hand-rolling HTTP/1.1 framing, routing and
  SSE chunked-encoding from raw `tokio::net::TcpListener` would be
  exactly the kind of unrequested, error-prone reinvention this project's
  own discipline avoids. `axum` was picked over the alternatives already
  in this dependency graph's orbit (`hyper` directly, `tiny_http`) for the
  same reason `reqwest` was already chosen for the kernel's own provider
  calls: it is the standard, widely-audited choice for exactly this job,
  already pulls in nothing this workspace didn't already trust
  transitively (`hyper`, `tower`), and its own SSE response type
  (`axum::response::sse::Sse`) is what makes the resumable stream
  genuinely simple rather than hand-rolled chunked framing.
- **Three small, justified touches outside `arbiter-cli/src/serve/` itself:**
  `arbiter-store::catalog::RunSummary` gained an `orphaned_cost: f64`
  field (and `list_runs`'s own `SELECT` gained the column) because U5's
  own plan text explicitly requires showing it "when non-zero" and no
  existing reader exposed it — the column already existed in the schema,
  only the read path was missing it. `accept.rs`'s private
  `AcceptanceArtifact` became `pub(crate)` so `POST /api/runs/:id/accept`
  persists through the *exact* same `Artifact` impl `arbiter accept`
  uses, rather than a second, drift-prone copy of the same one-purpose
  wrapper. `maintenance.rs`'s `known_providers`/`credential_sources`
  became `pub(crate)` for the same reason, for `GET /api/providers`. All
  three are minimal visibility/field additions to already-shipped,
  already-tested code, not behavioral changes to it.
- **The admission token is minted with `getrandom` directly, not the `rand`
  crate.** One 16-byte OS-random fill, once per process — `rand`'s own
  generator/distribution machinery has no caller here, so pulling in the
  smaller, purpose-built crate (already a common transitive dependency in
  this exact ecosystem) matches the precedent `zeroize` alone (not a
  larger crypto crate) already set for `SecretString`.

Verified: `cargo test -p arbiter-cli serve::` — 9/9 tests passing,
including all 9 IMPLEMENTATION_PLAN.md acceptance command names plus
`token_absent_from_store_and_log`. A manual smoke test against a live
`cargo run -- serve`: `GET /`, `GET /api/providers`, `POST /api/runs`
(a real synthetic run to completion), `GET /api/runs/:id` (matching the
CLI's own `explain --json` byte-for-byte), `GET /api/runs` (history),
admission rejection on a wrong `Host`/missing token, and the absence of
any `Access-Control-*` response header — all confirmed by hand against
the running process, not only the automated suite. Full workspace: `cargo
test --workspace` green, `cargo fmt --all -- --check` clean, `cargo
clippy --workspace --all-targets --all-features -- -D warnings` clean.

## D49 — U2-U7: the embedded UI's 5 screens and their acceptance suite — "Playwright" becomes a direct-CDP crate, `GET /api/runs/:id` grows beyond `explain --json`, and a server-computed estimate

New `arbiter-cli/src/serve/ui.html` (the single embedded page, ARCHITECTURE
§17.1's "no build step, no npm, no bundler, no framework, no CDN" taken
literally: one `<style>` block, one `<script>` block, no dependency of any
kind) and `arbiter-cli/tests/ui.rs` (the plan's own "Playwright" acceptance
suite, `cargo test -p arbiter-cli --test ui`).

- **U6's "Add key" element (a masked input plus a "Save to keychain"
  button) is a CLI-redirect notice, not a working form.** The plan's own
  U6 table describes it literally, but no endpoint anywhere in this
  build's own API surface accepts a secret: U1's 8-endpoint table (D48),
  ARCHITECTURE §17.1, and INTERFACES §24's own "five endpoints" prose all
  list the same set, and none of them is a key-write route — `POST
  /api/providers/:p/test` only ever *verifies* a key already resolved by
  `CredentialSource`. Inventing a ninth endpoint that accepts a raw secret
  over this loopback socket would be exactly the kind of unrequested
  surface this project's own discipline avoids, and sits uneasily next to
  INTERFACES §25's own stated paranoia about where a key is allowed to
  come from (three fixed sources, no `ConfigFile` variant "deliberately").
  The Keys screen's "Add a key" panel instead states plainly that the
  field is for reference only and points at `arbiter keys set
  <provider>`, the CLI path (L4) that already writes to the OS keychain —
  U6's own "never render a key" and "config files are never read for a
  key" requirements are unaffected either way, since no key ever transits
  this page or this process's HTTP surface either now.
- **The acceptance suite is `chromiumoxide` driving this sandbox's
  pre-installed Chromium directly over CDP, not a literal Node.js
  `@playwright/test` run.** IMPLEMENTATION_PLAN.md's own U2-U7 acceptance
  commands say "Playwright" by name, but a literal Playwright run needs
  `npm install` at test time against a live package registry — exactly the
  kind of non-reproducible, network-dependent step this workspace's own
  `cargo test --workspace` has never had anywhere else in it. This sandbox
  already ships a pre-installed Chromium meant to be driven by
  `executablePath` (`/opt/pw-browsers/chromium`) — exactly the shape
  `chromiumoxide::BrowserConfig::chrome_executable` wants — so the suite
  stays Cargo-native, offline-reproducible, and exercises the identical
  real browser engine Playwright itself would have driven, just without a
  second language runtime and package manager in the loop. Read as
  fulfilling U2-U7's own acceptance intent (a real browser, not jsdom or a
  hand-rolled DOM shim, running against a real `arbiter serve` subprocess)
  rather than its literal tool name.
- **`GET /api/runs/:id` (added under U1, D48) returns more than a bare
  `explain --json` passthrough — Screen 3 (Result) genuinely needs fields
  `explain --json` was never asked to carry.** U5's own plan text describes
  a Result screen showing the outcome tag, live-objection banner, options
  table, confidence breakdown, claims table, change triggers, and run
  integrity (chain/fixpoint/completeness/orphaned cost) in one screen load
  — no combination of existing CLI JSON outputs carries all of that in one
  shape. The response nests the untouched `explain --json` payload
  verbatim under an `"explain"` key (so anything depending on that
  sub-object byte-for-byte, including D48's own
  `explain_endpoint_matches_cli_byte_for_byte` test, still holds) and adds
  sibling fields — `outcome`, `recommendation`, `claims`
  (`render::claim_rows`), `integrity` — built from the same reader/render
  primitives `arbiter show`/`arbiter claims` already use, not new
  computation. This is read as filling in what U1's summary table left
  implicit rather than as a conflict with it: nothing in U1's own text says
  this endpoint's response is *limited to* `explain --json`, only that it
  exists.
- **`GET /api/providers`'s `estimates` object is a worst-case formula, not
  a measured cost.** U2/U7 both require the New Run screen to show a call
  count/cost/wall-clock estimate that visibly falls when a provider becomes
  unusable — nothing in ARCHITECTURE or INTERFACES defines an estimation
  algorithm, only that one must exist and respond to the usable-provider
  count. `run_estimate(depth, usable_count)` builds its number from the
  same flat per-call cost constants the real orchestrator already uses
  (`orchestrator::CALL_COST`, made `pub(crate)` for this one caller) times
  the worst-case call count a full run at that depth could reach — the same
  conservative-upper-bound spirit as the budget ledger's own admission
  check (INTERFACES §6), not a promise of the run's actual eventual cost.
  `usable_count` is the input that makes "the estimate falls when models
  are unusable" true and testable end-to-end
  (`estimate_falls_when_a_model_is_unusable`), rather than a UI number
  nothing on the server side actually computes.
- **Two real application bugs found and fixed by the acceptance suite
  itself, not test-harness artifacts:** (1) `EventType`'s wire form is
  `SCREAMING_SNAKE_CASE` and `ClaimStanding`'s is `snake_case`
  (`#[serde(rename_all = ...)]` in `arbiter-kernel::event`/
  `arbiter-core::claim` respectively) — `ui.html`'s first draft compared
  against PascalCase/capitalized strings for both, so the Running screen's
  stage tracking and the Result screen's claims-table filter silently
  never matched anything. (2) `SqliteRunStore::reader()` silently creates
  an empty `run.db` for any run id that does not exist yet (needed for
  `init`'s own first-write use case), which meant `GET /api/runs/:id`,
  `GET /api/runs/:id/events`, and `POST /api/runs/:id/accept` all returned
  200 on an unknown run id (`{"status":"running"}` forever) instead of 404
  — fixed with a `run_exists` file-existence check performed before ever
  opening a reader, applied uniformly across all three handlers.
- **A non-`CONSENSUS` outcome with empty `defeat_chains` is a real, valid
  case, not one this UI can assume away.** `SplitDecision` can arise from
  pure option-share fragmentation with every claim already `Agreed` —
  exactly the synthetic panel's own three independent, non-contradicting
  positions produce. U5's "Live objection... above the fold" requirement
  reads as applying to every non-consensus outcome, so `renderResult()`
  falls back to a margin-based explanation (built from the same
  `outcome`/`recommendation` fields already in the response) when
  `defeat_chains` is empty, rather than omitting the banner for this
  outcome shape.
- **`sse_reconnect_resumes_without_duplicate_events` exercises
  `Last-Event-ID` resumption via the browser's own `fetch`, not by reading
  the rendered Running-screen DOM live.** The mock-provider pipeline this
  sandbox runs completes fast enough that the very first `GET
  /api/runs/:id` status check `ui.html` makes before ever opening an
  `EventSource` frequently already sees `"status": "complete"` — the
  Running screen never renders a populated event log at all in that case,
  making "catch the log mid-stream" a genuine, unfixable race rather than
  a timing bug in the test. The suite instead fetches the completed run's
  full event backlog once, picks a sequence id partway through, and
  re-fetches with a `Last-Event-ID` header set to it — the same header
  `EventSource`'s own native reconnect sends — asserting the resumed
  stream starts at exactly that id's successor with no gap or replay. This
  runs from inside the real page's own browser context (`X-Arbiter-Token`
  header, same-origin `fetch`, real admission gate) rather than a bare
  `reqwest` call, which is what a browser-driven acceptance suite adds
  over `serve::tests::sse_resumes_from_last_event_id` (D48's own
  HTTP-client-side coverage of the same contract) rather than duplicating
  it.
- **Addendum — `panel_without_keys` (D47's own list of genuinely-blocked
  fixtures) stays blocked even now that U2 (the screen that owns it in the
  ledger) is done.** ARCHITECTURE's own fixture table (§19) states its
  requirement precisely: "models with no key are listed, disabled,
  excluded from the estimate, and lower independence." The first three
  hold and are tested (`estimate_falls_when_a_model_is_unusable`,
  `start_disabled_with_0_usable_models`); the fourth does not, because
  `GET /api/providers`'s response never gained the `usable_models`/
  `independence_groups`/`independence_if_selected` fields INTERFACES
  §25's own worked example shows. INTERFACES §25 requires these be
  "computed by the same code path §6.2 uses — not a UI approximation," and
  §6.2's `independence` formula (`arbiter-core::decision::evidence::
  independence`) takes a `CanonicalClaim`'s own `correlation_groups()` —
  it has no caller that operates over "the currently usable provider
  roster" rather than one claim's supporting evidence, because that
  caller is G2's own `panel.resolve` stage, already deferred out of this
  build for its own documented reasons (D30-D33). Wiring `GET
  /api/providers` straight to `arbiter-core::decision::evidence::independence`
  without `panel.resolve` existing to feed it a real correlation-group
  partition would be exactly the kind of invented-in-place-of-missing-work
  this project's own discipline avoids, so the two `independence_*`
  fields, and `panel_without_keys` itself, stay unbuilt — not silently
  dropped, but re-attributed here from "blocked on U2" (now false) to
  "blocked on G2's `panel.resolve`" (still true), so the ledger keeps
  pointing at the fixture's real remaining dependency.

Verified: `cargo test -p arbiter-cli --test ui` — 11/11 tests passing,
covering all 5 screens (New Run, Running, Result, History, Keys), the
5-panel-key-state matrix, the 0-usable-providers disabled-start case, the
falling estimate, keyboard navigability, the detach note, SSE
reconnect/resume, override-reason enforcement, and the live-objection
fallback — run twice at different `--test-threads` values with no
flakiness observed. Full workspace: `cargo test --workspace` green, `cargo
fmt --all -- --check` clean, `cargo clippy --workspace --all-targets
--all-features -- -D warnings` clean.

## D50 — `arbiter-store::lease`: real macOS and Windows liveness checks, added past spec at user request

`crates/arbiter-store/src/lease.rs`'s `compile_error!` gate (D-less until now — it
shipped as part of S2, before this deviation log's own numbering caught up to it)
previously refused to build anywhere but Linux, on the grounds stated in its own
doc comment: without a real per-OS liveness check, [`pid_is_alive`] would always
return `false` off Linux, making every lease look abandoned and stealable out
from under a live owner. Neither ARCHITECTURE.md nor INTERFACES.md discusses any
platform beyond what this workspace's CI runs (`ubuntu-latest`) — this task is
genuinely outside both spec files' scope, done because the user explicitly asked
for the engine to run on "all machines," not because §0's authority order
required it.

- **What changed:** `boot_id()`, `hostname()`, and `pid_is_alive()` each gained
  real `#[cfg(target_os = "macos")]` / `#[cfg(target_os = "windows")]` arms
  alongside the existing Linux one, and the `compile_error!` gate now only fires
  for a platform with none of the three. No `unsafe` code was introduced
  (`#![forbid(unsafe_code)]` stands); every new arm shells out to a command
  already present on its OS by default (`sysctl`, `kill`, `hostname` on macOS;
  `powershell` on Windows) via `std::process::Command`, the same technique
  `serve/mod.rs`'s `webbrowser_open` (D48) already used for its own per-OS
  `open`/`start`/`xdg-open` calls — not a new dependency, not a new pattern.
- **macOS:** `boot_id()` reads `sysctl -n kern.bootsessionuuid`, a per-boot-session
  UUID Apple ships specifically for this purpose — the direct analogue of
  Linux's `/proc/sys/kernel/random/boot_id`. `pid_is_alive()` runs `kill -0
  <pid>` (the portable command-line form of the `kill(pid, 0)` libc call
  `#![forbid(unsafe_code)]` rules out calling directly) and reads its result:
  success or any failure *other than* a "No such process" stderr message is
  read as alive, not just a clean exit code — `kill -0` also fails with the
  same exit status on `EPERM` (a live process owned by someone else), and
  collapsing that into "dead" would make a live foreign-owned process's lease
  falsely stealable.
- **Windows:** `boot_id()` reads `(Get-CimInstance Win32_OperatingSystem).LastBootUpTime`
  via PowerShell — deliberately not `wmic`, which Microsoft has been removing
  from default Windows installs. `pid_is_alive()` runs `Get-Process -Id <pid>
  -ErrorAction SilentlyContinue` and checks whether anything came back.
  Windows has no boot-session UUID equivalent to macOS's; a boot timestamp
  serves the same purpose here (constant for one boot, different across any
  other) even though it isn't a UUID.
- **Fail-closed on ambiguity, every new platform:** if the OS command can't be
  spawned at all, or its output doesn't parse as expected, `pid_is_alive`
  returns `true` (alive) rather than `false` — a lease that stays open past
  its owner's real death is a stuck run (recoverable: `arbiter doctor` reports
  it, an operator can act); a lease stolen out from under a still-live owner
  is silent, undetectable corruption. Only Linux's own `/proc/<pid>` check
  resolves every case outright with no fallback needed, since it is the one
  platform this workspace's own CI (`ubuntu-latest`) can actually verify.
- **What is *not* verified: this workspace's CI, and this whole session, run on
  Linux only.** The Linux arms are unchanged and still pass their existing 5
  `lease::` tests unmodified. The macOS and Windows arms compile clean —
  checked two ways: `cargo check`/`cargo clippy --target x86_64-pc-windows-gnu`
  against the real crate (a `gcc-mingw-w64-x86-64` cross-toolchain was
  installed in this sandbox specifically to let `rusqlite`'s bundled C sources
  build for that target), and, since a real macOS SDK isn't available to cross-
  compile against from Linux, the platform-specific functions themselves were
  extracted into a dependency-free standalone file and type-checked directly
  against `rustc --target x86_64-apple-darwin`'s real prebuilt std with `--deny
  warnings`. Neither of those proves the *behavior* is correct on real
  hardware — that `sysctl kern.bootsessionuuid`, `kill -0`, `Get-CimInstance
  Win32_OperatingSystem`, and `Get-Process -Id` all behave exactly as their
  documentation says, under whatever locale/permission/PowerShell-execution-
  policy configuration a real machine has. This should be smoke-tested on a
  real macOS box and a real Windows box before being trusted with a run that
  actually matters, the same way the Linux arm already has 1.0's own worth of
  CI runs behind it and these two do not.

Verified: `cargo test -p arbiter-store lease::` — 5/5 passing (Linux, unchanged).
`cargo check`/`cargo clippy -p arbiter-store --target x86_64-pc-windows-gnu -- -D
warnings` — clean. Standalone `rustc --target x86_64-apple-darwin --crate-type
lib --deny warnings` on the platform-specific functions in isolation — clean.
Full workspace: `cargo test --workspace` green, `cargo fmt --all -- --check`
clean, `cargo clippy --workspace --all-targets --all-features -- -D warnings`
clean, all on the Linux host.

## D51 — P4/P4b: the real provider adapters, `--panel` resolution, and a swallowed-error defect the first real key exposed

P4 in the plan is one line ("real provider adapters"). Implementing it turned
up four things worth writing down, three of which are corrections to code the
plan had already signed off.

- **Non-streaming JSON, per adapter, deliberately.**
  `Provider::call` returns one `ProviderResponse` carrying final
  `prompt_tokens`/`completion_tokens`. A streamed body would have to be
  reassembled into exactly that shape, and every one of these vendors reports
  usage more reliably on the non-streamed response than mid-stream. Streaming
  in this product is `arbiter serve`'s SSE, which streams *events from the
  store*, not tokens from a vendor. `eventsource-stream` stays a dependency for
  that, not for these.

- **`idempotency: None` for Anthropic and Gemini is load-bearing, not a TODO.**
  Neither documents an idempotency header for its generate endpoint. Declaring
  one anyway would let the kernel retry a call that may already have been
  billed; `None` sends that call to ORPHANED instead, which is exactly what
  ARCHITECTURE §8.4 asks for. Only the OpenAI-compatible family declares
  `Header("Idempotency-Key")`, and the header is sent only when the kernel
  actually supplies a key.

- **A named provider with no key is a hard error, not a silent drop** (`panel.rs`).
  §6.2's independence term is computed over the panel that *ran*. Quietly
  running an operator's three-model panel as two would inflate confidence
  against a panel nobody approved. The error names the env var and offers
  `--panel mock`.

- **The defect P4 exposed, and the fix.** `positions_generate.rs` discarded the
  provider's error at `Err(_) => return None`, and every sibling stage did the
  same at `.ok()?`. With `mock` this path never fired. With a real key it fired
  immediately: a revoked key produced `Outcome: InsufficientEvidence /
  Completeness: complete` with 0 claims, no failure event, and no
  `BUDGET_RELEASED` — a confident-looking empty decision with nothing in the
  transcript to explain it. Three changes, all inside existing spec:
  - `generate_one` returns `Result<Position, PositionSkip>`; the reason rides
    `STAGE_COMPLETED`'s payload. No new `EventType` variant — INTERFACES §13's
    enum is authoritative and ARCHITECTURE §8.4 gives FAILED no event of its
    own (its Event column is "—").
  - **Zero positions from a non-empty panel is now `StageError`**, emitting
    `STAGE_FAILED`. INTERFACES §6's `SkipItem` covers "the debate continues
    with four positions and the record says so"; it does not cover continuing
    with zero. `k2_provider_timeout`'s 3-of-4 case is untouched and still
    passes — the two halves of that rule now have a test each.
  - `BUDGET_RELEASED` is emitted (shared `stages::emit_budget_released`) on
    every path that abandons a reservation, in all 8 provider stages.
    `ReservationGuard::drop` does the ledger half but holds no event sink, so
    the release had never once reached a transcript.

- **`rustls-tls-native-roots`, not `rustls-tls`** (workspace `Cargo.toml`).
  Plain `rustls-tls` trusts only the Mozilla roots compiled into
  `webpki-roots` and ignores the machine's trust store, so every provider call
  fails `UnknownIssuer` behind a corporate TLS-inspecting proxy — a
  configuration a desktop CLI meets routinely and an operator cannot fix
  without recompiling. Found by hitting it in this sandbox.

- **Screen 1's panel checkboxes became real** (`ui.html`, `handlers.rs`).
  They had been `checked disabled` decoration, correct only while `mock` was
  the sole runnable panel: with the adapters shipped, `arbiter serve` would
  otherwise have kept running mock forever while the CLI ran real models.
  What is ticked is now `POST /api/runs`'s `panel`, resolved through the same
  `panel::resolve` the CLI uses. Real providers with keys are the default
  selection and `mock` is left unticked when any exist — a panel mixing
  synthetic answers with real ones produces a §6.2 independence figure that
  means nothing.
- **The estimate stopped assuming a 3-model panel.** `run_estimate` borrowed
  `mock_panel().len()` whenever *anything* was usable, which was accurate only
  while every panel was the mock panel. It now takes the model count directly,
  and `GET /api/providers` returns one precomputed estimate per possible panel
  size (`estimates.per_model_count`) so ticking a box changes which row the
  page *reads*. U7 forbids the page computing a number; summing each row's own
  `models` field to index that table is the only arithmetic it does, and
  `models` comes from `panel::models_contributed_by` — the same function that
  knows `mock` is three models behind one name — so the two cannot drift.
- **A test-suite flake fixed at the cause** (`tests/ui.rs`). Each UI test spawns
  a real `arbiter serve` *and* a real Chromium, and `cargo test` defaults
  `--test-threads` to the core count; 11 at once starved the container and 4
  tests failed that pass every time in isolation. A semaphore caps concurrent
  browsers at 2 rather than lengthening timeouts again or requiring every
  future runner to remember `--test-threads=1`. The suite is also *faster*
  this way (16s vs 25s serial).

**A correction, found by a real key in real use.** The first version of this
had only `Rejected` for "the vendor answered and would not serve us", so a
*live* Anthropic key with an exhausted balance — which answers
`400 invalid_request_error`, "Your credit balance is too low" — was reported as
`rejected`. That told the operator to replace a key that authenticated
perfectly. `Blocked` now covers it: 401 and 403 are the only two statuses HTTP
reserves for authentication, so anything else got *past* auth by definition and
the key is not the problem. Blocked results are never cached, because topping
up a balance or waiting out a rate limit changes the answer immediately; they
render amber rather than red, and say in words that the key is fine and the
account needs attention. `arbiter keys test` still exits non-zero for both —
neither can serve a debate — but names which problem it is.

**And a second correction, from the same report.** Even once classified
correctly, the result read as a wall of JSON: the raw error envelope was being
passed through verbatim, so the operator saw
`{"type":"error","error":{"type":"invalid_request_error","message":"Your credit
balance is too low..."},"request_id":"req_011..."}` when one sentence of that
was the point. `http::message_in` now digs the vendor's own sentence out of
whichever envelope it arrived in — `/error/message` covers all five providers,
with `/message`, `/error/detail` and a bare `{"error": "string"}` as fallbacks
for gateways — and an unparseable body (an HTML page from a load balancer) still
falls through to itself, because an unrecognised shape is exactly when the
operator most needs to see it whole. Fixed in `http.rs` rather than in the Keys
screen, so Compare cards and debate failure messages get it too.

The result is also split into three fields instead of one blob: a `headline` in
our words ("Your key works. anthropic refused the request — check that
account's billing, plan or rate limits."), the vendor's `detail` unparaphrased,
and the `status`. The first attempt put our framing and the vendor's text in one
string and then repeated the framing underneath, which said the same thing twice.

Verified end to end against the live Anthropic API with a deliberately invalid
key: `Error: positions.generate: every model in the panel failed to produce a
position (1 of 1): anthropic/claude-sonnet-4-5: anthropic HTTP 401
Unauthorized: {"type":"error","error":{"type":"authentication_error",
"message":"API key is invalid."}...}`, with the event log reading
`RUN_STARTED → STAGE_STARTED → POSITION_STARTED → BUDGET_RESERVED →
CALL_STARTED → BUDGET_RELEASED → STAGE_FAILED → RUN_FAILED`. `--panel mock`
still runs fully offline and unchanged.

What is *not* verified: no successful real completion from any vendor — that
needs a valid key, which this session does not have. The adapters' wire format
is covered instead by `tests/http_round_trip.rs`, which serves each request
from a local socket and asserts the auth header, URL, and body that actually
went out.

## D52 — Screen 6 (`Compare`), a published-price table, and the retirement of `tools/multiplex/`

No plan section describes a Compare screen; U2–U7 name five. It exists
because this repository briefly held two applications: Arbiter, which could
reason but could not call a model, and `tools/multiplex/`, a Node app that
could call five models but could not reason about the answers. P4 gave
Arbiter the adapters, which made the second application's only remaining
advantage — showing several answers side by side — a screen this one was
missing rather than a reason for a second program. It is now
`arbiter serve`'s sixth screen, and `tools/multiplex/` plus
`run-multiplex.{bat,sh}` are deleted: one binary, one UI, one launcher.

- **`POST /api/compare` deliberately breaks three of the pipeline's rules,
  because a comparison is not a run.**
  - *A missing key is a skip, not an error.* `panel::resolve` refuses a panel
    it cannot run in full (D51), because §6.2 computes independence over the
    panel that ran. Compare asserts nothing about the answers beyond "here is
    what each one said", so a keyless provider gets a greyed card reading
    `no key configured` and the rest proceed — which is also what an operator
    asking "who can answer this?" means by the question.
  - *Nothing is stored.* No `RunId`, no event chain, no artifacts. Writing a
    comparison into the run store would put a row in `arbiter history` that
    no `explain` could account for.
  - *No budget ledger.* §8.3's reservation protocol exists so a run can be
    resumed and reconciled after a crash; a single unstored call has nothing
    to resume. `ProviderRequest` still needs a `ReservationId`, so a
    comparison's is prefixed `compare_` and can never be mistaken for a real
    reservation in a log the two share.
- **Per model, not per token.** Multiplex streamed vendor deltas.
  `Provider::call` returns one finished `ProviderResponse` (D51's reasoning),
  so a card fills in when its model finishes. The fastest model still lands
  first, which is the part of a side-by-side race worth watching; token-level
  streaming would mean changing the provider seam, which nothing else wants.
- **`arbiter-providers::pricing`** ports Multiplex's list-price table so an
  answer can show what it cost. It is **not** wired into the budget path:
  §8.3's ledger is authoritative about money and reconciled against a
  vendor's usage export, and a stale constant must never be able to move a
  reservation. A provider with no entry returns `None`, rendered as
  "cost unknown" — a `0.0` would read as "this one was free". `money()`'s
  three decimals were also wrong here: a real $0.00004 answer displayed as
  `$0.000`, so sub-tenth-of-a-cent figures now show two significant digits.
- **`SSE` over a streamed `fetch`, not `EventSource`.** `EventSource` cannot
  issue a POST, and the prompt does not belong in a URL where a proxy's
  access log would keep it. The frames use the same `event:`/`data:` shape
  the run stream uses, so the page has one framing to understand rather
  than two.
- **Compare is the one screen that widens the container** (880px → 1400px, via
  a `body.compare` class). At 880px only two answers fit side by side, and two
  is not much of a comparison; every other screen reads better narrow.

Verified: `POST /api/compare` driven over HTTP against the real endpoint with
one provider keyed — `model-skipped` ×4, then `model-start`, then
`model-error` carrying the live API's own `401 ... "API key is invalid."` and
an elapsed time, then `run-done`. The screen itself was driven in a real
browser with a canned stream to confirm the four-across grid, the per-card
token/cost/latency line, the red-bordered failure card and the greyed skip.
`compare_renders_one_card_per_model` covers the keyless path end to end in
CI. What is *not* verified, as in D51: a successful real completion, which
needs a valid key this environment does not have.

## D53 — the embedded UI rebuilt in Multiplex's design, at the user's direction

D52 folded Multiplex's *function* into `arbiter serve` as a sixth screen but
kept Arbiter's own plain styling, and made `#/new` the landing route. The user
was explicit that this was the wrong end of the trade: *"i do not entertain
arbiter and multiple i should only see multiplex with all functionality in the
multiplex ui"*. So the design went the other way round — Multiplex's interface
is now the whole application, and Arbiter's screens live inside it.

- **The shell is Multiplex's**, recovered from `a543bbd:tools/multiplex/public/index.html`
  before that file was deleted: sticky topbar with the gradient brand mark,
  pill tabs, a coloured card per model, the response grid, stat tiles, the
  stage stepper, and its light/dark token set. The brand reads "Multiplex"
  because that is the application the user wants to see; `arbiter` remains the
  binary and the CLI, which is what every doc and script already calls.
- **Six tabs, not four.** Multiplex had Compare / Flow / Usage / Overview, and
  the last three were mockup-driven with no backend. Here they are Compare /
  Debate / Flow / Usage / History / Keys, and each is wired to a real endpoint.
  Flow and Usage are the two that changed meaning: Flow draws §5's 15-stage
  pipeline from the run's own stored events, and Usage charts the last
  comparison's real per-model tokens, latency and cost.
- **Compare is the landing route** (`#/compare`), not `#/new`. It is the screen
  that works with one key and no setup; a debate needs a panel and a budget.
  Six UI tests navigated to `""` expecting the debate form and now say `#/new`.
- **Fonts are embedded, not linked.** Multiplex pulled Sora and IBM Plex Mono
  from `fonts.googleapis.com` and highlight.js from a CDN. Both are dropped:
  ARCHITECTURE §17.1 allows no CDN, `arbiter serve` binds loopback and must
  render identically with no network at all, and a page holding provider API
  keys should not announce itself to a third party on every load. The two
  typefaces (SIL OFL 1.1, latin subsets) are inlined as base64 — 72KB, and the
  syntax highlighter is simply gone.

Two defects the rebuild exposed, both fixed here:

- **`GET /api/runs` 500'd on a fresh install.** `history.db` resolves to the
  store root's *parent*, a directory nothing creates until the first run, so
  History and Usage both showed "unable to open database file" where the true
  answer is an empty list. The handler now creates that directory. The old UI
  hid this because History was the only screen that called the endpoint and a
  first-time user rarely opened it before running anything.
- **`.bar-fill` drew nothing.** The usage bars are `<span>`s, and `width` and
  `height` do not apply to an inline box; the track escaped it only by being a
  flex item (which blockifies), while the fill one level deeper stayed inline.
  Both now declare `display: block`.

Five more the acceptance suite caught in the rebuild, all in code written for
it, all fixed rather than asserted around:

- **`page.goto` never completes on a fragment-only change.** `run_to_result`
  reached the debate form with `goto(url("#/new"))`, but every caller already
  has the app loaded, so that is a same-document navigation and `goto` waits
  for a load event that never fires. It sets `location.hash` now, the way the
  app itself navigates. Four tests hung on this.
- **The skip reason was shouted and clipped.** `.stage-txt` is
  `text-transform: uppercase`, so "no key configured" rendered as
  `NO KEY CONFIGURED` across a card header. The status word is now just
  "skipped" and the reason moved to the card body, beside where an error
  message already goes.
- **`table:nth-of-type(3)`** silently matched nothing once each section moved
  into its own card, because `nth-of-type` counts among shared siblings. The
  penalties table carries an id now, and the test asserts the requirement
  ("all five penalty rows render") rather than a position on the page.
- **The detach note** had been paraphrased into something weaker; U3 names it,
  so it is restored verbatim.
- **The keyboard-navigation sweep** asserted the debate question field has
  `autofocus` on the landing screen — true only while `#/new` was the front
  door. It now covers `#/new` as well and checks whichever field each screen
  actually opens with, which is broader than before rather than looser.

Verified: all 13 UI tests pass against the rebuilt page, and every tab was
driven in a real browser and screenshotted — Compare with five answers (including a
failed card and a keyless one), Debate with the panel picker and a live
estimate, Usage with per-model token/latency/cost bars, and Keys. What is not
verified is unchanged from D51: no successful real completion from any vendor,
which needs a valid key this environment does not have.

## D54 — key verification made real, and the Keys screen given the two controls it was missing

`arbiter keys test`, `arbiter providers test` and `POST /api/providers/:p/test`
had all refused honestly since L4: there was no adapter to make the one minimal
request they verify against. P4 built the adapters, and this wires them up. The
Keys screen also gained an **Add key** form at the user's request.

- **`ProviderError` grew a structured variant.** Verification has to tell "this
  key is wrong" (401/403 — the operator can fix it) from "the vendor is unwell"
  (5xx — they can only wait) and from "we never reached them at all" (DNS, TLS,
  a proxy), and recovering a status code by parsing a human-readable string
  would be guesswork. `ProviderError::Http { status, message }` carries it.
  Purely additive: 28 construction sites, zero of them destructuring.
- **One verification path, two callers.** `verify.rs` is shared by the CLI
  command and the serve endpoint, so the terminal and the page can never
  disagree about whether a key works. It sends a four-token completion, not a
  models-list call: several of these providers accept a key for listing that
  they then reject for inference, so listing proves less than it appears to.
- **Only an authentication failure is cached.** §11.1 asks for a 24h cache
  keyed by fingerprint; caching a 429 or a 503 for a day would keep telling an
  operator their key is bad long after the rate limit cleared.
- **`POST .../test` answers `200` even when the key was refused.** "Your key is
  rejected" is a fact the endpoint successfully established, not a failure of
  the request; the page reads `state`.
- **`Verification` is wider than `VerifyResult`.** The stored enum has only
  `Verified` and `Rejected{status}`, and no way to say "never reached the
  vendor" — collapsing an unreachable network into `Rejected` would tell an
  operator to replace a key that is perfectly good. The wider enum is the API
  shape; only the two cases §11.1 names are ever cached.

**A reversal worth naming.** The Keys screen used to say, in copy I wrote:
"Keys are never entered through this page — it would put them in a browser's
memory and history for no benefit." The user asked for exactly that capability,
and on inspection the "no benefit" half was wrong: an operator who has just
been told "no key configured" *on this screen* should be able to fix it here,
not be sent to a different program. The exposure is bounded by what already
guards every other endpoint — loopback-only bind, per-process token, Host and
Origin and `Sec-Fetch-Site` checks — and the key goes straight to the OS
keychain, the same place `arbiter keys set` puts it, never to a file this
program writes. The page carries it in a `type="password"` field with
`autocomplete="off"`, POSTs it in a body (never a URL, where a proxy log or
browser history would keep it), clears the field on success, and the response
returns only the state and the last four characters of the fingerprint. The
key is never logged and never echoed — `setting_a_key_refuses_what_it_cannot_store`
asserts a refusal does not hand it back.

Verified end to end against the live Anthropic API with a deliberately invalid
key: the Keys screen's **Test** flipped the row to `rejected` and showed the
vendor's own `HTTP 401 ... "API key is invalid."`, with the source and
fingerprint (`env:ARBITER_ANTHROPIC_API_KEY`, `…c9a3`) and no key anywhere on
the page; `arbiter keys test anthropic` printed the same thing and exited
non-zero. Four serve tests and one UI test cover the refusals, the keyless
path, `mock` verifying without a socket, and both endpoints sitting behind
admission. What is still unverified is unchanged from D51: no *successful*
verification against a real vendor, which needs a valid key this environment
does not have — the `Verified` arm is exercised only through `mock`.

## D55 — credential handling compared against `master-prompt-generator`, and what was worth taking

At the user's request, this codebase's API-key logic was compared against
`nagabalaji88/testavatar`'s `master-prompt-generator` (branch
`claude/master-prompt-generator-5bfovx`), which solves the same problem for a
multi-user server app. Four things came back; one of them was a real bug here.

**Adopted.**

- **A blank credential is a missing one, not an empty one.** Their
  `_api_key_for` guards this explicitly ("passing `""` to litellm reads as 'no
  auth' on some providers and as a malformed header on others"). This codebase
  did not: `ARBITER_ANTHROPIC_API_KEY=` reported the provider as **present**,
  sent an empty `x-api-key`, and the Keys screen then told the operator their
  key was *rejected* — pointing at the wrong thing entirely. Worse, because
  §11.1 reads the environment before the keychain, a blank variable in a shell
  profile silently **shadowed** a perfectly good stored key. Both sources now
  trim and treat blank as absent. Trimming also catches
  `export KEY=$(cat keyfile)`, whose trailing newline travels into the header.
- **A status-aware explanation.** Their `_describe_http_error` notes that "401
  means the key is wrong while 429 means the key is fine and the account is out
  of quota, and those have opposite fixes". D54's `Blocked` had the right shape
  but one headline for every non-auth status, so a 404 (the plan does not
  include this model) and a 503 (the vendor is down) both told the operator to
  check their credit balance. The headline now follows the status.
- **Where to get a key.** Their provider-family table carries a `console_url`
  per family. A screen that says "no key configured" and offers a box to paste
  one into is half an answer if the reader does not know where the key comes
  from; `console_url_for` now sits beside the Add-key form.
- **Say when a stored key is outranked.** Their store deliberately puts the
  database above the environment, reasoning that "a key an admin has just typed
  into the UI has to take effect, and if a stale environment variable outranked
  it the edit would silently do nothing". That trap is live here now that the
  Keys screen can write to the keychain — but §11.1 fixes the resolution order
  with the environment first, and that is not this endpoint's to change. So the
  write reports its `effective_source`, and the page says "Saved to the
  keychain — but env:ANTHROPIC_API_KEY is still what gets used" rather than
  reloading unchanged and looking broken.

**Deliberately not adopted.**

- **Encrypted-at-rest database storage.** Theirs is right for a multi-user
  server: several replicas, an admin UI, no OS keychain to reach. It costs an
  encryption-key lifecycle they document honestly — deriving from
  `JWT_SECRET_KEY` when `CREDENTIAL_ENCRYPTION_KEY` is unset, and every stored
  credential becoming undecryptable if that secret rotates. A single-user local
  CLI has a strictly better option already: the OS keychain, where the platform
  owns the key material and no backup or `pg_dump` can contain a provider key.
- **Provider families and aliases** (`claude` → `anthropic`). Friendly, and
  cheap in their design because a family is already a first-class record. Here
  a provider id is simultaneously a keychain username, an environment-variable
  stem, a `--panel` token and a verification-cache key; introducing a second
  accepted spelling means normalising at every one of those or having two
  spellings resolve to two different keychain entries. Not worth it for the
  spelling convenience alone.

Verified: `ARBITER_ANTHROPIC_API_KEY=` now reports `missing` where it reported
`present`, and `ARBITER_ANTHROPIC_API_KEY="  " ANTHROPIC_API_KEY=sk-ant-real`
resolves to the real key rather than the blank one. Six new tests cover blank
and whitespace-only values, the shadowing case, whitespace trimming, the
per-status headlines, and that every runnable provider has a console URL.

## D56 — verification asks the free question first

A user's Anthropic key worked in `master-prompt-generator` and was reported
`blocked` here, and both tools were right. MPG checks a key with
`GET /v1/models`; this checked it with a completion. A listing call needs a
valid key and nothing else, so an account with an exhausted balance passes it
and fails the completion — one tool says the key works, the other says it does
not, and an operator holding both has no way to tell which question each
answered.

`probe::probe_key` now asks the listing question first, for every provider
(endpoints taken from MPG's own `model_discovery`). Three consequences:

- **A bad key costs nothing to find.** 401 on the listing call returns
  immediately; the paid completion is never made. D54 chose a completion over a
  models-list on the grounds that listing "proves less" — true, and the answer
  is to do both in order rather than to pick one.
- **A blocked account can prove its key is fine.** The headline now cites the
  evidence — "The key authenticated against anthropic's model list (47 models)
  moments ago, so it is valid" — instead of asserting it, which is what
  reconciles this screen with any tool that checks by listing alone.
- **It degrades correctly.** A provider with no known listing endpoint, or a
  listing call that fails for some non-auth reason, falls through to the
  completion — the question that actually matters. Nothing depends on the
  probe succeeding.

Confirmed afterwards from the other tool's own UI, which reports
**"key is valid — 73 chat models reachable (does not check remaining quota)"**.
That is the same distinction in its own words: its check establishes validity
and says so honestly, and it never claimed to test whether the account can run
anything. Nothing was wrong in either tool — the two answered different
questions and neither screen said which.

So this one now says what it proved, rather than only that it passed:
"Verified — a real completion succeeded, so the key works and the account can
run inference." A reader holding both screens can see which is the stronger
claim without having to know how either was implemented.

Verified: a deliberately invalid Anthropic key is now reported `rejected` by
the listing call alone, with the vendor's own "API key is invalid." and no
completion spent. The `blocked`-with-proof path cannot be exercised here — it
needs a valid key on an account without credit, which is precisely the state
this session has no access to.

## D57 — two real defects a user's own screenshots exposed, and the reason the other tool worked

A comparison run failed on every provider at once, while the same keys produced
answers in `master-prompt-generator`. The three failures were three different
things, and only one of them was mine:

```
anthropic  HTTP 400  Your credit balance is too low ...
openai     HTTP 429  You have no credits remaining ...
gemini     HTTP 404  This model models/gemini-2.0-flash is no longer available.
                     Please update your code to use models/gemini-3.6-flash
```

- **Gemini's default model was retired out from under it.** A genuine bug here,
  and the one thing in that screenshot this codebase could fix. The new id is
  taken from the vendor's own 404, not guessed. Its test now asserts against
  `default_model()` rather than repeating the literal, so the next retirement
  is a one-line change instead of two places that can disagree.
- **The other two are exhausted accounts**, on the two vendors this build
  reached directly. Nothing to fix in code; already reported correctly.
- **And MPG worked because it was not calling those vendors at all.** Its
  network response names `OPENROUTER_API_KEY` and `credential_family:
  openrouter`; its enabled models are `openrouter/deepseek/deepseek-chat`,
  `openrouter/mistralai/mistral-small-3.2-24b-instruct`,
  `openrouter/meta-llama/llama-3.3-70b` and GPT-OSS on Groq. It routes through
  an aggregator, billed by the aggregator, so a direct OpenAI or Anthropic
  account running dry does not touch it.

So **`openrouter` and `groq` are now providers here too**, which is what makes
this app usable with the keys the operator already has. Both speak Chat
Completions, so both are `Flavor`s of the existing `openai_compatible` adapter
rather than new ones — the whole addition is four match arms, two listing
endpoints and two environment variables.

Neither gets a pricing entry, deliberately. An aggregator's cost is whatever
the routed model charges, so there is no single rate to publish; `None` renders
as "cost unknown", and an invented average would be a wrong number displayed
with the same confidence as a right one. `every_direct_provider_has_a_published_price`
now asserts exactly that split, so a future provider cannot quietly acquire a
made-up price.

While here: every provider's conventional environment variable is now
recognised, not just Anthropic's. `OPENAI_API_KEY`, `GEMINI_API_KEY`,
`XAI_API_KEY`, `DEEPSEEK_API_KEY`, `OPENROUTER_API_KEY` and `GROQ_API_KEY` all
resolve — a key already exported for another tool is found without renaming it,
which is §11.1's whole point in having source 2.

## D58 — A panel is a list of models, and the picker now says so

`--panel` always took a list of *models*: `parse_spec` splits on the first
comma-separated entry, `resolve` pushes one roster row per entry, and it
registers one adapter per **distinct** provider precisely so a provider named
twice is two panel members on one key. `PositionId` is `pos_{provider}_{model}`,
so those members stay distinct all the way through `positions.generate`.

The Debate screen did not say so. Its Panel card was one checkbox per provider
and submitted `selectedProviders().join(",")`, which made the panel size equal
to the number of working keys. An operator holding two keys could run a
two-model debate and no larger — and five models is where §6.2's confidence
arithmetic starts having enough positions to be worth reading.

So each usable provider row now carries its own extra-model lines: a
`+ another model on this key` button appends a text field, and the submitted
spec is `provider` for the default model plus `provider:model` for each line
filled in. That is the same string `--panel` takes, so the two entry points
cannot drift.

Two consequences worth naming, because both are places this could have quietly
lied:

- **The estimate table had to grow.** It was sized `0..=usable_models`, one row
  per possible tick-count, and any larger panel fell back to the whole-roster
  figure — which is *smaller*. A panel of six on two keys would have quoted the
  price of two. The table now runs to `MAX_PANEL_MODELS` (12), and a panel past
  its end is quoted from the last row as a floor ("at least"), never from the
  roster figure. `MAX_PANEL_MODELS` is a display limit only; neither `--panel`
  nor the kernel caps panel size.
- **The independence warning still counts providers.** Five models behind one
  OpenRouter key are one source, not five, and §6.2's independence term is
  computed over the providers that actually ran. The Panel card therefore
  reports both numbers separately ("5 models · 2 providers") and the warning
  says the panel is not independent however many of them answer. Growing the
  panel must not be a way to talk the confidence figure up.

The judge seat is unchanged: it rides the first entry in the panel, so listing
the provider you want judging first still decides it.

## D59 — Model ids come from the vendor, not from this binary

D58 made a panel able to hold five models. It left the operator typing the ids
from memory, which is the wrong place to put that knowledge: an aggregator's
catalogue turns over weekly, and a mistyped id does not fail at submit — it
fails as a 404 from the vendor part-way through a paid run, which is the most
expensive place to discover a typo.

So `arbiter_providers::catalogue` reads the list live, through the same
model-listing endpoints `probe` already uses for key verification (`probe`
asks for one row, the catalogue for all of them; one set of URLs, so the two
can never point at different endpoints). `GET /api/providers/:p/models` and
`arbiter providers models <p>` are the two ways to see it, and neither spends
a completion — listing is free at every vendor, which is the same property
that made it the right first question for key verification.

Two properties come back, and they are **not the same kind of claim**:

- `free` is the vendor's own published price. OpenRouter quotes
  `pricing.prompt` and `pricing.completion` per model; both zero means free
  *by the vendor's statement*. A listing that quotes no price reports `None`,
  which renders as blank — never as free. A model whose input is free and
  whose output is billed is `false`, not `true`.
- `open_weights` is inferred from the model's name and is a **family label,
  not a licence audit**. It says "this belongs to a family whose weights are
  published", which is a fact about Llama, DeepSeek, Qwen, Mistral, Gemma and
  the rest. It does not say the licence is OSI-approved: Llama's community
  licence and Gemma's terms carry conditions, and `command-r`'s weights are
  non-commercial. Both the UI note and the CLI footer say to read the vendor's
  licence, rather than letting the flag stand in for having done so.

The name test is not a vendor-prefix rule, because two vendors ship both kinds
under one prefix: `google/gemma-*` is open and `google/gemini-*` is not,
`openai/gpt-oss-*` is open and every other `openai/*` is not. The model half of
the id decides, and is checked for prefixed and bare ids alike (Groq's carry no
prefix at all).

`free_open_weight_panel` picks **one model per family**. Five fine-tunes of one
base model fail together on exactly the questions that base model is weak on —
five positions and one opinion — and §6.2's independence term is about members
that fail together. Llama, then DeepSeek, then Qwen, then Mistral, then Gemma
is the most independence a single aggregator key can buy, and it is still not
five independent providers; the panel warning continues to say so.

One consequence worth naming: every usable provider now shows its default model
as a **visible, editable line** rather than an invisible fallback.
`--panel openrouter` and `--panel openrouter:<its default>` are the same run,
but only one of them can be read off the screen — and that default is a
*billed* model, so an operator building a free panel has to be able to see it
and replace it. The one-click free picker replaces those lines rather than
adding to them, for exactly that reason: a panel asked to be free must not
still be carrying the paid default.
