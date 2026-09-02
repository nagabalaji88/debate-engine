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
