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
