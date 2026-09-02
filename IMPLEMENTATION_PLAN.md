# Arbiter — Implementation Plan

**Target spec:** `ARCHITECTURE.md` v2.9 · `docs/INTERFACES.md` v2.9
**Audience:** an autonomous coding agent (any capable LLM) executing tasks in order.
**Status of this document:** executable. Every task has a command that decides whether it is done.

---

## 0. How to execute this plan

### 0.1 Authority order

When two sources disagree, the higher one wins. Do not resolve a conflict by picking
the more convenient statement.

| Rank | Source | Notes |
|---|---|---|
| 1 | `ARCHITECTURE.md` | pipeline, decision math, scope, criteria, fixture list |
| 2 | `docs/INTERFACES.md` | trait signatures, wire protocols, event enum, schemas |
| 3 | This plan | sequencing, file layout, acceptance criteria |
| 4 | Existing code | it is at the v2.0 baseline and is *behind* the spec — see §1.2 |

If this plan and the spec disagree, **the spec wins and this plan has a bug**. Record it
in `PLAN_DEVIATIONS.md` (create it), then proceed per the spec.

### 0.2 Rules that hold for every task

1. **Do not invent policy.** Every threshold, weight, cap and rate is named in the spec.
   If you cannot find one, it is a spec gap: write it to `PLAN_DEVIATIONS.md`, pick the
   most conservative reading, mark the constant `provisional = true`, and continue.
2. **Do not widen scope.** A task lists the files it may touch. Touching others needs a
   line in the commit body saying why.
3. **No `unwrap()` or `expect()` outside tests.** Errors are typed and propagated.
4. **No `panic!` on external input.** Malformed provider output, corrupt rows and bad
   config are handled, not asserted away.
5. **Determinism is a hard requirement in `arbiter-core`.** No clock, no RNG, no IO, no
   async, no float-order dependence. Iterate `BTreeMap`/`BTreeSet`, never `HashMap`.
6. **Floats are compared with a tolerance**, never `==`. Use `1e-9` unless the spec names
   another. `assert!((a - b).abs() < 1e-9)`.
7. **Every public item gets a doc comment** saying *why*, not *what*. The signature says what.
8. **Commit per task**, message `<task-id>: <what changed>`, body explaining the *why* and
   any deviation. Never squash two tasks into one commit.

### 0.3 Definition of done, per task

A task is done when **all** of these hold:

```bash
cargo fmt --all -- --check      # formatted
cargo clippy --all-targets --all-features -- -D warnings   # no warnings
cargo test --workspace          # all tests pass
```

plus the task's own **Acceptance** command. If any fail, the task is not done. Do not
proceed to a dependent task with a red suite.

### 0.4 How to handle a blocked task

Do not guess, and do not skip silently. Write the blocker to `PLAN_DEVIATIONS.md` with:
the task id, what you needed, where you looked, and the two or three readings you can see.
Then take the **most conservative** one — the reading that spends less money, keeps more
evidence, or refuses rather than proceeds — and mark the code `// PROVISIONAL: <task-id>`.

### 0.5 The one open decision

**`ARCHITECTURE.md` §5 draws the round loop but never names its re-entry point.**

Until a human decides, implement the loop as re-entering at **`options.cluster`**, because
§11 states attachment re-runs each round and the loop cost then reconciles with both the
standard ($0.480) and deep (~$0.90) cost targets. Mark it:

```rust
// PROVISIONAL: loop re-entry point. ARCHITECTURE.md §5 does not name it.
// Following §11 (attachment re-runs each round). See PLAN_DEVIATIONS.md.
```

Everything else in the spec is decided. Do not treat other gaps as open without checking.

### 0.6 What "the spec says" means for numbers

These are load-bearing and must appear in code as named constants, not literals:

| Constant | Value | Spec |
|---|---|---|
| fixpoint damping λ | 0.50 | §6.3 |
| support weight α | 0.25 | §6.3 |
| attack weight β | 0.60 | §6.3 |
| qualify weight γ | 0.15 | §6.3 |
| attack saturation cap | 1.5 | §6.3 |
| support saturation cap | 2.0 | §6.3 |
| confidence weights | 0.35 mass · 0.30 margin · 0.35 judge | §6.7 |
| penalties | 0.25 unresolved · 0.15 assumption · 0.10 truncation · 0.05 convergence · 0.20 dispersion | §6.7 |
| dispersion threshold | 0.15, inactive when `judge_count == 1` | §6.7 |
| `option_floor` | 0.20 | §6.6 |
| τ_gap | 0.15 | §6.6 |
| `truncation_factor` | 1.2, multiplies the rule-1 evidence floor only | §6.6, INTERFACES §9 |
| `converged_margin_factor` | 1.5 | §5.5 |
| `min_new_claims` | 2 | §5.5 |
| `min_standing_delta` | 0.05 | §5.5 |
| `budget_headroom` | 0.05 | §5 bounds |
| `repair_budget_fraction` | 0.15, **per round** | §5.1 |
| `blob_threshold` | 128 KB | §8.2 |
| `busy_timeout` | 5000 ms | §8.5 |
| verification cache TTL | 24 h | §11.1 |

All ship `provisional = true` until the tuning sweep **and** the red-team session pass (§6.3).

---

## 1. Starting state — verified, not assumed

### 1.1 What exists

Re-verify before starting; do not trust this table if the commands disagree.

```bash
git ls-files | grep -c '^crates/'        # 10
find crates -name '*.rs' | xargs wc -l | tail -1   # 582 total
cargo test --workspace 2>&1 | grep 'test result'   # 6 passed
```

| File | Holds | Complete? |
|---|---|---|
| `crates/arbiter-core/src/ids.rs` | `RunId`, `ClaimId`, `ModelId`, … | partial |
| `crates/arbiter-core/src/claim.rs` | `EvidenceKind`, `Grounding`, `ClaimLifecycle`, `ClaimStanding`, `ClaimMember`, `CanonicalClaim` | yes for v2.0 |
| `crates/arbiter-core/src/relation.rs` | `RelationKind`, `Relation` | yes |
| `crates/arbiter-core/src/judge.rs` | `Scorecard` + rubric weights | yes |
| `crates/arbiter-core/src/option.rs` | `DecisionOption`, `OptionScore` | v2.0 shape — **needs rework**, see C4 |
| `crates/arbiter-core/src/config.rs` | `DecisionConfig`, `Weights`, `GraphParams`, `Thresholds`, `ConfidenceWeights` | partial |
| `crates/arbiter-core/src/decision/evidence.rs` | `E(c)` and its 5 factors | yes, 5 tests |
| `crates/arbiter-core/src/decision/mod.rs` | module wiring only | stub |

### 1.2 What the existing code does **not** have

Verified by `grep`; all of these are v2.1–v2.9 additions the code predates:

```
fixpoint            standing classification   option scoring    outcome classification
confidence          change triggers           correlation_group attack_cap / support_cap
dispersion          policy_version            OptionVersion     supersedes
AttachmentMatrix    provenance gates          prompt packs      everything in arbiter-*
```

**Do not assume any file is current.** Each core task below states whether it extends an
existing file or creates one, and what to change in the existing one.

### 1.3 Crates to create

`arbiter-core` exists. These do not:

```
arbiter-store       arbiter-kernel      arbiter-providers
arbiter-plugin      arbiter-cli         arbiter-fixtures     arbiter-build
```

Dependency rule, enforced by CI in task X2: `core` depends on nothing internal ·
`kernel` depends on `core` · everything else depends on `kernel` · nothing depends on `cli`.

---

## 2. Task graph

Execute top to bottom. A task may start when every dependency is **done** per §0.3.

| ID | Task | Depends on | Crate |
|---|---|---|---|
| **X1** | Workspace skeleton, 7 new crates, CI | — | workspace |
| **X2** | Dependency-rule test | X1 | workspace |
| **C1** | Bring `arbiter-core` types to v2.9 | X1 | core |
| **C2** | Argumentation fixpoint | C1 | core |
| **C3** | Standing classification | C2 | core |
| **C4** | Options: identity, versions, attachment matrix, scoring | C1 | core |
| **C5** | Outcome classification | C3, C4 | core |
| **C6** | Confidence: 3 dimensions − 5 penalties | C5 | core |
| **C7** | Counterfactual change triggers | C6 | core |
| **C8** | `DecisionRecord` + `explain --json` payload | C6, C7 | core |
| **S1** | SQLite schema + migrations | X1 | store |
| **K0** | `Store`/`Provider` trait seams (no impl) | X1 | kernel |
| **S2** | `RunStore` impl (SQLite) + lease CAS | S1, K0 | store |
| **S3** | Event append, hash chain, `verify_chain` | S2 | store |
| **S4** | Projections + rebuild-from-events | S3, C8, K3 | store |
| **S5** | Blob store above threshold | S3, K2 | store |
| **S6** | `history.db` catalogue + `reindex` | S3 | store |
| **K1** | Budget ledger + reservation protocol | K0 | kernel |
| **K2** | Provider-call state machine + recovery | K1 | kernel |
| **K3** | StageGraph + checkpointing | K2 | kernel |
| **K4** | Bounds, headroom, per-round budget sizing | K1 | kernel |
| **K5** | Response cache | K1 | kernel |
| **P1** | `Provider` trait + capabilities | X1 | providers |
| **P2** | Mock provider (scriptable) | P1 | providers |
| **P3** | Credential resolution + redaction | P1 | providers |
| **P4** | Anthropic + OpenAI-compatible adapters | P3, K5 | providers |
| **G1** | Prompt packs | S1 | kernel |
| **G2** | Stages 1–5: init … claims.normalize | K3, S4, P2, G1 | kernel |
| **G3** | `options.cluster` + attachment | G2, C4 | kernel |
| **G4** | `relations.analyze` + premise cycles | G3 | kernel |
| **G5** | `disputes.rank` + `challenge.plan` | G4, K4 | kernel |
| **G6** | `challenge.run` + `rebuttal.run` | G5 | kernel |
| **G7** | `controller.decide` + the round loop | G6 | kernel |
| **G8** | `judge.evaluate` | G7 | kernel |
| **G9** | `decision.synthesize` | G8, C8 | kernel |
| **L1** | CLI skeleton + `run` + `--stream` | G9 | cli |
| **L2** | `show`, `explain`, `claims`, `history` | L1, S6 | cli |
| **L3** | `resume`, `replay` | L1, K2 | cli |
| **L4** | `accept`, `keys`, `providers`, `doctor`, `reindex` | L2, P3 | cli |
| **F1** | Fixture harness | P2, L1 | fixtures |
| **F2** | The 36 CI fixtures | F1, all G, all C | fixtures |
| **U1** | `arbiter serve`: server, admission, embedding | L4, F2 | cli |
| **U2** | UI screen 1 — New run | U1 | cli |
| **U3** | UI screen 2 — Running | U2 | cli |
| **U4** | UI screen 3 — Result | U2 | cli |
| **U5** | UI screen 4 — History | U2 | cli |
| **U6** | UI screen 5 — Keys | U2, P3 | cli |
| **U7** | UI cross-cutting: states, a11y, fixtures | U2–U6 | cli |

**Milestone 1.0** = X1–F2 green. **Milestone 1.1** = U1–U7 green.
Build Studio and plugin hosts are **1.5** and are out of this plan.

> **Dependency direction, corrected (see `PLAN_DEVIATIONS.md` D1).** `arbiter-kernel`
> depends on `arbiter-core` only; it defines the `Store` and `Provider` trait seams
> (task K0). `arbiter-store` and `arbiter-providers` depend on `arbiter-kernel` and
> implement those traits. A `K*` task never adds `arbiter-store` as a crate
> dependency — it either defines a trait (K0) or consumes one it already owns.

---

## 3. Tasks — foundation

### X1 · Workspace skeleton

**Files:** `Cargo.toml`, `crates/arbiter-{store,kernel,providers,plugin,cli,fixtures,build}/`
**Spec:** §4.1

Create seven crates with `lib.rs` containing only `#![forbid(unsafe_code)]` and a doc
comment naming the crate's one job. Wire `workspace.dependencies`: add `rusqlite`
(feature `bundled`), `tokio`, `reqwest`, `clap`, `thiserror`, `tracing`.
`arbiter-cli` is the only `[[bin]]`, named `arbiter`.

Add `.github/workflows/ci.yml` running fmt, clippy `-D warnings`, and `cargo test --workspace`.

**Acceptance**
```bash
cargo build --workspace && cargo test --workspace
test "$(cargo metadata --no-deps --format-version 1 | grep -o '"name":"arbiter-[a-zA-Z-]*"' | sort -u | wc -l)" = 9
# 8 shipped crates + arbiter-workspace-checks (dev-only, publish = false, X2's home)
```

**Do not** add a dependency not named in §16. No `anyhow` in library crates — errors are
typed with `thiserror`; `anyhow` is allowed in `arbiter-cli` only.

---

### X2 · Dependency-rule test

**Files:** `tests/dependency_rule.rs`
**Spec:** §4.1

A test that parses `cargo metadata` and asserts the four rules in §1.3. It must fail if
someone adds `arbiter-store` to `arbiter-core`'s dependencies.

**Acceptance**
```bash
cargo test --test dependency_rule
# then prove it bites:
# temporarily add arbiter-store to arbiter-core deps -> test must FAIL -> revert
```

---

### C1 · Core types to v2.9

**Files:** extend `claim.rs`, `ids.rs`, `config.rs`, `decision/evidence.rs`; create `policy.rs`
**Spec:** §6.2, INTERFACES §15 (correlation groups) · **Corrected per** `PLAN_DEVIATIONS.md` D3–D6

Existing files are at the v2.0 baseline. Add, without breaking the 6 passing tests:

- `ids.rs`: `GroupId` (opaque, defaults to provider — id-style, arbitrary construction
  is fine), `OptionVersion` (**constructible only from `blake3(canonical_text)`, no
  public arbitrary `new`** — the v2.5 bug was letting anything call `OptionId::new(text)`),
  `PolicyVersion` (opaque label, e.g. `"argument-v1"`).
- `claim.rs`: `correlation_group: GroupId` on `ClaimMember`, defaulting to `provider`.
- `decision/evidence.rs`: rewrite `independence()` to partition on `correlation_group`
  (INTERFACES §15's exact function), not `distinct_providers()`. Leave `corroboration()`
  alone — the spec defines it over providers, not groups.
- `config.rs`: add `option_floor: f64` (0.20) to `Thresholds`. Add a doc comment on
  `min_margin` and `dissent` noting they are τ_gap and τ_dissent respectively — **do not
  rename them**, they are already correct and tested.
- `policy.rs` (new): `Policy { version: PolicyVersion, provisional: bool, config:
  DecisionConfig }` with `Policy::argument_v1()`. Also add `attack_cap: f64` (1.5) and
  `support_cap: f64` (2.0) to `GraphParams` in `config.rs` — C2 needs them and nothing
  in the codebase has them yet.

**Not in C1** (moved or dropped — see `PLAN_DEVIATIONS.md`): `provenance.rs` (Build
Studio, 1.5, D3) · `supersedes` (belongs on `DecisionOption`, C4's job, D4) ·
`converged_margin_factor` / `min_new_claims` / `min_standing_delta` (kernel controller,
G7) · `budget_headroom` / `repair_budget_fraction` (kernel budget ledger) ·
`blob_threshold` (store) — all D5. `ReservationId` / `CallId` / `Sequence` belong to
`arbiter-kernel`, added when K0/K1/S3 need them, not to `arbiter-core`.

**Acceptance**
```bash
cargo test -p arbiter-core
cargo test -p arbiter-core policy::tests::argument_v1_ships_provisional
cargo test -p arbiter-core ids::tests::option_version_has_no_public_arbitrary_constructor
cargo test -p arbiter-core decision::evidence::tests::independence_partitions_on_correlation_group_not_provider
```
A confidence-weights test must assert `0.35 + 0.30 + 0.35` sums to 1.0 **within 1e-9** —
not `== 1.0`, which is false in f64 (this lands in C6, but the weights themselves are
already in `config.rs` and worth asserting here too).

**Do not** make `OptionVersion` constructible from arbitrary text — only from
`blake3(canonical_text)`. C4 is where this matters most: the v2.5 bug was letting
anything call `OptionId::new(text)`, which minted a new id on every reword and orphaned
every attachment cell. `OptionId` itself stays a plain opaque id in C1; `OptionVersion`
is the one C1 must lock down, since C4 builds directly on it.

---

### C2 · Argumentation fixpoint

**Files:** `crates/arbiter-core/src/decision/fixpoint.rs`
**Spec:** §6.3 · **Corrected per** `PLAN_DEVIATIONS.md` D7 — read that before trusting
the formula below if you are working from an older copy of this plan.

Damped Jacobi iteration to a fixpoint over the claim graph.

```rust
pub struct FixpointResult {
    pub standing: BTreeMap<ClaimId, f64>,
    pub iterations: u32,
    pub converged: bool,           // false -> FIXPOINT_NOT_CONVERGED + 0.05 penalty
    pub max_delta: f64,            // for that event's payload
    pub saturated: BTreeSet<ClaimId>,
}
pub fn solve(claim_ids: &[ClaimId], evidence: &BTreeMap<ClaimId, f64>,
             relations: &[Relation], p: &GraphParams) -> FixpointResult;
```

Per iteration, for each claim: cap the **raw, unweighted** sum first, weight it
second — `support_term = α · min(Σ w·standing(s), support_cap)`, `attack_term = β ·
min(Σ w·standing(a), attack_cap)`, where `w` is `Relation::confidence`. Qualify has no
stated cap: `qualify_term = γ · Σ w·standing(q)`. `Unrelated`/`Uncertain` relations
carry no weight and are excluded entirely (`relation.rs`). Then `target =
clamp01(E(c) + support_term − attack_term − qualify_term)`, and the next value is
`prev + λ·(target − prev)` — damped *toward* the target, not the target itself.
Every read within one sweep uses the *previous* sweep's values (Jacobi, not
Gauss-Seidel), which is what makes the result order-independent by construction
rather than by convention. Stop when max delta < `epsilon` or at `max_iterations`;
record which. Initial condition (not stated by the spec): `standing_0(c) = E(c)` — the
only value that needs no information about neighbours, and already a fixed point for
any claim with no incoming edges.

**Getting the cap order backwards is the single highest-value mistake to avoid here**
— `α · min(raw, cap)` and `min(α · raw, cap)` diverge numerically (D7 works the
example), and only the first matches the spec's own worked numbers.

**Acceptance** — these behaviours, each its own test:
```
one strong attacker leaves a fact contested, not dead
two strong attackers kill it
ten weak attackers cannot outweigh one strong refutation   (attack_cap)
an oscillating graph terminates and is deterministic
iteration order does not change the result (BTreeMap, not HashMap)
```

---

### C3 · Standing classification

**Files:** `decision/standing.rs` · **Spec:** §6.4 · two gaps resolved per
`PLAN_DEVIATIONS.md` D8

`Defeated` (< 0.15 or Withdrawn/Rejected) · `Disputed` (≥1 live attacker ≥ 0.30) ·
`Unresolved` (Unverified/Unsupported, never resolved) · `Agreed` (≥ 0.50, no live attacker).
Evaluated **in that order**, first match wins — same convention as §6.6's outcome
classification.

`Defeated` is **terminal per version**; there is no `Revived`. A later version of the same
claim is a new version, and the gate resolves final standing (INTERFACES §18, Provenance
chains — not §14, which is the confidence formula; corrected here after the plan cited
the wrong section).

Two things §6.4's prose leaves genuinely underspecified, resolved conservatively (D8):
what "resolved by challenge" means (taken as lifecycle `Defended`/`Modified` — a challenge
that concluded with an outcome), and what a claim with no live attacker, non-`Unverified`
kind, and standing short of `agreed` classifies as (the four rules are not jointly
exhaustive; falls to `Unresolved` rather than silently `Agreed`).

**Acceptance** `cargo test -p arbiter-core decision::standing::` — one test per class,
one per D8 resolution, plus `has_live_attacker` and `classify_all` each getting their
own test rather than only being exercised incidentally through `classify`.

---

### C4 · Options, versions, attachment, scoring

**Files:** rework `option.rs`; create `decision/attachment.rs`
**Spec:** §5.3, §6.5, INTERFACES §20 · **Corrected per** `PLAN_DEVIATIONS.md` D9, D10

`OptionId` is the **cluster's identity**, stable across rewording. `option_version` is the
text hash. `supersedes: Option<(OptionId, OptionVersion)>` carries lineage. The
attachment matrix is `BTreeMap<(ClaimId, OptionId), Attachment>` where `Attachment {
polarity: Polarity, confidence: f64, source: AttachSource }` — **not** the simpler
enum this plan originally sketched (D9): `source` (`Authored`/`Classified`/`Propagated`)
is load-bearing, not decoration — it is what makes Step 3's propagation checkable at all.

**Only Step 3 (propagation) and scoring belong in `arbiter-core`.** Steps 1–2
(clustering, batched attach) call an LLM and belong to the kernel's `options.cluster`
stage (G3) via an `OptionClusterer` trait this task does not define. C4 owns exactly the
deterministic half: given direct (`Authored`/`Classified`) cells, the relation graph and
claim standings, propagate to `attachment_propagation_depth` (default 2) —

```
c contradicts s ∧ s supports O   →  c opposes O      (strength × relation confidence)
c supports    s ∧ s supports O   →  c supports O
c qualifies   s ∧ s supports O   →  c opposes O at γ weight
```

— tagging the results `Propagated`; and then scoring: `raw = Σ standing(supporting) −
0.5 · Σ standing(opposing)`, clamped at 0 and normalised to `share`. D10: `raw` can be
negative (net-opposed option) with no convention given for it, so clamp *before*
normalizing — a negative share has no meaning — and when every option's clamped `raw`
is 0, every share is 0 (not NaN, not an even split that would manufacture confidence
that isn't there).

**Model vote share is not an input at any point.** A test must assert this: a graph where
4 models back A and 1 backs B, but B's claims carry the evidence, must score B higher.

**Acceptance**
```
rewording a recommendation keeps OptionId and mints a new option_version
attachment cells survive a reword (the v2.5 regression test)
model votes do not affect score
shares sum to 1.0 within 1e-9 in the non-degenerate case (D10)
an all-non-positive-raw graph produces all-zero shares, not NaN
propagation respects attachment_propagation_depth and does not cross it
```

---

### C5 · Outcome classification

**Files:** `decision/outcome.rs` · **Spec:** §6.6

Evaluated **in order**: `INSUFFICIENT_EVIDENCE` → `SPLIT_DECISION` → `CONSENSUS` →
`MAJORITY_WITH_DISSENT`. `option_floor` (0.20) is required in rules 1–3. Rule 1's
evidence floor is `min_evidence_mass × truncation_factor` (1.2, only when the run was
truncated — D12); rule 3's is `min_evidence_mass` alone, per §6.6's literal text — do
not carry the truncation multiplier into rule 3. `score(top1)`/`score(top2)`/`margin`
read `OptionScore.share`, not `raw` (D13).

`classify()` takes `evidence_mass`, `unresolved_critical_ratio`, and
`live_dissent_against_top1` as plain scalar/bool inputs, plus a `truncated: bool` —
**not** the full `Completeness{reason: StopReason, missing_stages: Vec<StageName>}`
enum INTERFACES §9 describes. `StopReason`/`StageName` are pipeline/kernel concepts
with no definition anywhere yet; C5's own scope is `decision/outcome.rs` and its
dependencies are C3+C4 only, not any K/G task. `Completeness` itself is introduced in
C8, where `DecisionRecord` actually needs to serialize it.

**Acceptance** — the latent bug from §6.6 must be covered:
```
score(A)=0.11, score(B)=0.08  ->  INSUFFICIENT_EVIDENCE, never SPLIT_DECISION
```

---

### C6 · Confidence

**Files:** `decision/confidence.rs` · **Spec:** §6.7, INTERFACES §14

Three dimensions minus **five** penalties. `dispersion` is inactive when `judge_count == 1`.
`ConfidenceWeights` (added in C1) only carried two of the five penalty coefficients —
`truncation_penalty`, `convergence_penalty`, `dispersion_weight` and
`dispersion_threshold` were missing and are added now (D14). `judge_score` for
`judge_count > 1` is the mean of each judge's `Scorecard::weighted()`, and
`judge_dispersion` its population stdev — neither aggregation is stated explicitly in
the spec, so this is the documented, conservative reading (D15).

**Acceptance** — pin the spec's worked example exactly:
```
base      = 0.35*0.88 + 0.30*0.81 + 0.35*0.91 = 0.8695
penalties = 0.25*0.08 = 0.0200 ; 0.15*0.07 = 0.0105
total     = 0.8390        (assert within 1e-9)
Σ contributions == total within 1e-9
```
and the dispersion table from INTERFACES §14: judges 0.85/0.75 → penalty **0**;
0.90/0.50 → 0.010; 1.00/0.00 → 0.070 (the two-judge maximum).

---

### C7 · Change triggers · C8 · DecisionRecord

**Files:** `decision/triggers.rs`, `decision/record.rs` · **Spec:** §6.8, §6.9, INTERFACES §22

C7: for each **unresolved or disputed** claim (D16 — not unresolved alone), pin its
standing to the extreme opposite its current baseline lean (D17) and re-run the
fixpoint, reporting `margin_before`, `margin_after` and whether the winner changes.
`counterfactual_flips` returns one entry per candidate regardless of `is_trigger`,
since INTERFACES §21's `decision_leverage` reuses this exact pass for every claim,
not only the ones that flip the winner.

C8: `DecisionRecord` carrying `policy_version`, and the `explain --json` payload of
INTERFACES §22 — **including `dispersion`**, whose absence was a v2.9 finding.
Omits `model_agreement`, `dissent`, `assumptions`, `acceptance` and `completeness`
(D18) — none has a formula or fully-specified type this crate has been given; they
land in `G9 decision.synthesize`, which is why the task graph has `G9` depend on
`C8` rather than the reverse.

**Acceptance**
```bash
cargo test -p arbiter-core record::tests::explain_json_matches_schema_v1
cargo test -p arbiter-core record::tests::contributions_sum_to_total
cargo test -p arbiter-core record::tests::penalties_array_has_five_entries
```

---

## 4. Tasks — store

### S1 · Schema and migrations

**Files:** `arbiter-store/migrations/0001_initial.sql`, `src/schema.rs`
**Spec:** §8.1, §8.5, §8.7

`run.db`: `events` (**`seq INTEGER PRIMARY KEY`** — rowid alias, so `ORDER BY seq` is a
scan not a sort), `run`, plus `schema_metadata`. `history.db`: `run_catalog` exactly
as §8.5 writes it, with both indexes.

Scoped to exactly the tables the spec gives complete columns for (D21) — §8.1 names
~15 more projection tables by title only, with no column list anywhere in either
spec file. `budget`/`provider_calls`/`cache_entries`/`artifacts` wait for K1/K2/K5,
the tasks that actually read and write them; the debate/decision projection group
(`positions`, `claims`, `claim_relations`, `disputes`, `challenges`, `rebuttals`,
`judge_evaluations`, `decision`, `decision_triggers`, `provenance`, `stages`) waits
for S4, which already depends on C8 and K3 for exactly this reason.

`schema_metadata` carries **`db_schema_version`**, never `schema_version` — that name
already means the event envelope (§9).

**Do not** create a compound `(run_id, seq)` index: `run_id` is constant inside a `run.db`
and it would duplicate the primary key.

**Acceptance**
```bash
cargo test -p arbiter-store schema::tests::seq_is_rowid_alias
cargo test -p arbiter-store schema::tests::opening_a_newer_db_schema_is_refused
```

---

### S2 · Implement the traits; the lease

**Files:** `src/lib.rs`, `src/lease.rs` · **Spec:** INTERFACES §1 · **Depends on:** K0

Implement `arbiter_kernel::{RunStore, RunWriter, Tx, RunReader}` for a SQLite-backed
type — the trait signatures come from K0 (`arbiter-kernel`), verbatim from INTERFACES §1.
This crate does not redeclare them; it depends on K0's crate and writes the bodies.
No signature may name a directory, a lock, a flush or a torn tail — if implementing one
forces you to change the signature, the trait leaked and belongs back in K0's report.

Lease acquisition is a **compare-and-swap on `lease_epoch`**, not a liveness check:

```sql
-- create: PK does the work; a second create loses -> AlreadyOpen
INSERT INTO run (run_id, owner_pid, boot_id, hostname, started_at, engine_version, lease_epoch)
VALUES (?,?,?,?,?,?,1);
-- reopen: read the epoch, decide the owner is gone, then CAS on it
UPDATE run SET owner_pid=?, boot_id=?, hostname=?, started_at=?, lease_epoch=lease_epoch+1
 WHERE run_id=? AND lease_epoch=?;
-- changes()==1 -> ours. changes()==0 -> AlreadyOpen.
```

An owner is gone when `boot_id` differs from the current boot, **or** `boot_id` matches and
the pid is not alive. That is the *precondition*; the CAS is the *decision*.

**Acceptance**
```bash
cargo test -p arbiter-store lease::tests::two_racing_reopens_only_one_wins
cargo test -p arbiter-store lease::tests::second_create_is_already_open
cargo test -p arbiter-store lease::tests::stale_boot_id_is_stealable
```
The race test must spawn two threads that read the same epoch and both attempt the steal.

`lease.rs` is Linux-only (`/proc/sys/kernel/random/boot_id`, `/proc/<pid>` for the
liveness check — `#![forbid(unsafe_code)]` rules out `kill(pid, 0)`), matching CI's
`ubuntu-latest`; neither spec file discusses any other platform.

`sqlite_store.rs` implements `RunStore::create`/`reopen`/`reader`,
`RunWriter::transact` and `Tx::append_event` (mechanical persistence: assign `seq`,
store whatever `content_hash`/`previous_event_hash` the caller already computed —
computing those correctly is S3's job, sitting above this layer) and
`RunReader::events`. `Tx::put_artifact`/`put_cache`/`commit_budget`/`set_call_state`
and `RunReader::verify_chain` return an explicit "not yet implemented, see &lt;task&gt;"
error rather than a silently-wrong implementation — their tables (D21) or their
logic (verify_chain's hash recomputation) belong to S3/S4/K1/K2/K5.

---

### S3 · Events and the hash chain

**Files:** `src/events.rs` · **Spec:** §8.1, §8.3, §9, INTERFACES §13

Append inside a transaction. `content_hash = blake3(canonical payload)` — resolved
as the event's whole content (every field but the two hash fields and the
DB-assigned `sequence`), not literally just the `payload` JSON field, since only
that reading catches every column an edited row could tamper with (D22).
`previous_event_hash` chains to `seq - 1`. `verify_chain` recomputes and reports —
reading raw stored strings for `event_type`/`payload`, not the typed
`EventType`/`serde_json::Value`, so a row whose `event_type` this binary can't
parse is still hashable and chain-verified even though `RunReader::events()`'s
typed view skips it (INTERFACES §13's forward-compatibility promise).

A break is **detected, never repaired** — truncating a table would destroy the projections
derived from it. `CHAIN_BREAK_DETECTED` emission itself is deferred: it is an
`EventType` variant written *to* the very log being verified, and this task only
ships `verify_chain`'s detection; wiring the emission into a caller is a later
kernel-side concern once something actually calls `verify_chain` during a run.

Every read is `ORDER BY seq`.

**Acceptance**
```bash
cargo test -p arbiter-store events::tests::chain_verifies_over_10k_events
cargo test -p arbiter-store events::tests::an_edited_row_is_detected_not_repaired
cargo test -p arbiter-store events::tests::unknown_event_type_is_skipped_but_still_chained
```

---

### S4 · Projections · S5 · Blobs · S6 · Catalogue

**S4** (`src/project.rs`, §8.1): rebuild every projection from `events`. The
`projection_rebuild` fixture asserts the rebuilt tables equal the pre-crash ones. Where a
projection and the log disagree, **the log wins**.

**S5** (`src/blob.rs`, §8.2): payloads live in the DB below `blob_threshold` (128 KB).
Above it: **write blob → fsync → THEN commit the row.** Never the reverse. A blob with no
row is collectable; a row with no blob is corruption. GC is lazy (`doctor --gc`),
content-addressed so no refcounting, and **skips runs whose lease is live** — a blob
fsynced before its row commits is indistinguishable from an orphan and is not one.

**S6** (`src/catalog.rs`, §8.5, INTERFACES §1): one insert at run start (`running`), one
update at completion. WAL + `busy_timeout` 5000 ms. `reindex` is now a scan and an upsert —
no watermark, no delta pass, no lock choreography. `VACUUM INTO` for export, blobs copied
**second**.

Done ahead of S4/S5 (both blocked: S4 needs K3, not yet built; S5 needs K2, not yet
built — S6 needs only S3). `reindex` is honest about a real limitation: `run.db`
today only has `events`/`run`/`schema_metadata` (D21), so `question`/`outcome`/
`confidence`/`margin`/`model_count`/`depth` aren't derivable from a run directory
yet — `reindex` upserts `run_id`/`run_path`/`started_at` and leaves the rest until
S4 gives `run.db` a `decision` projection to read them back from. `VACUUM INTO`
export is deferred to whichever CLI task (`L2`/`L4`) actually implements `arbiter
run --export`, since S6's own acceptance test doesn't exercise it and nothing else
in the workspace calls it yet.

**Scope note (S5 done; S4 done for 4 of ~15 tables):** S5 (`src/blob.rs`) is
implemented and tested — content-addressed write (`write_blob`,
fsync-before-return, idempotent on identical content), read, the fixed
`blobs/b3/<hash>` layout, and lazy GC (`gc_one_run`/`gc_run`, lease-checked via
`lease::owner_is_gone`/`boot_id` made `pub(crate)` for exactly this reuse).
`DEFAULT_BLOB_THRESHOLD_BYTES` (128 KB) is defined here per D5's ownership
assignment. GC's referenced-hash set is caller-supplied rather than queried
from `cache_entries`/`artifacts` (D27) — at the time S5 landed those tables
didn't exist yet either; S4 (below) has since added them, but `gc`'s
referenced-set injection stays as-is since it is still correct and a future
`doctor` (L-task) is the natural place to wire the real query in.

S4 (`src/project.rs`) adds `budget`, `provider_calls`, `cache_entries` and
`artifacts` — the four projection tables INTERFACES §5's crash-recovery
sequence and ARCHITECTURE §8.3's SQL examples specify precisely enough to
implement now. `Tx::put_artifact`/`put_cache`/`commit_budget`/`set_call_state`
(previously `StoreError::Other` stubs naming this task, D21) are real against
these tables; a new `Tx::reserve_call` method and an `Artifact::to_json`
method were added to close two gaps the literal INTERFACES §1/§6 trait
signatures left — neither carried enough to actually persist a reservation or
an artifact's bytes. `project.rs::rebuild_operational_projections` replays
`events` to reconstruct `budget`/`provider_calls`'s happy path.
`stages` and the ten claim-graph/decision projections remain deferred to
`G2`–`G9`, whose real per-stage event payloads are what should pin their
columns — see D29 for the full reasoning and the exact scope of what this
pass's replay does and does not reconstruct (crash-recovery states other than
the happy path are explicitly out of scope here, left to K2/L3).

**Acceptance**
```bash
cargo test -p arbiter-store project::tests::rebuild_equals_original
cargo test -p arbiter-store blob::tests::row_never_references_a_missing_blob
cargo test -p arbiter-store blob::tests::gc_skips_a_run_with_a_live_lease
cargo test -p arbiter-store catalog::tests::concurrent_writers_do_not_block_readers
cargo test -p arbiter-store catalog::tests::reindex_rebuilds_from_run_dbs
```
`project::tests::rebuild_equals_original` names the fixture-driven, full-table
version of this test — not yet meaningful until the claim-graph projections
exist. `project.rs`'s own test module has this pass's real equivalent instead:
`rebuild_reconstructs_the_happy_path`, `a_reservation_with_no_call_started_resumes_as_released`,
`rebuild_is_idempotent_and_clears_stale_rows_first`, and
`an_event_with_a_malformed_payload_is_skipped_not_fatal`.

---

## 5. Tasks — kernel and providers

### K0 · Store and Provider trait seams

**Files:** `arbiter-kernel/src/store_trait.rs`, `provider_trait.rs`
**Spec:** ARCHITECTURE §4.1 ("kernel — … event store …"; "store — SQLite implementation
of the Store traits"), INTERFACES §1, §5 · **Deviation:** `PLAN_DEVIATIONS.md` D1

Define the trait signatures only — **no SQLite, no filesystem, no implementation** in
this crate. Copy `RunStore` / `RunWriter` / `Tx` / `RunReader` from INTERFACES §1
verbatim, and `ProviderCapabilities` / the provider-facing trait from INTERFACES §5.
`arbiter-kernel`'s `Cargo.toml` must not gain a dependency on `arbiter-store` or
`arbiter-providers` to do this — that is precisely the edge D1 exists to prevent.

Neither spec file gives a concrete Rust definition for the 13 supporting types these
4 traits reference (`Event`, `Sequence`, `Manifest`, `StoreError`, `Artifact`,
`ArtifactId`, `CacheKey`, `CachedResponse`, `ReservationId`, `Cost`, `CallId`,
`CallState`, `ChainStatus`) — each had to be authored from JSON examples, SQL
statements, and prose scattered across both files (D19). Two required outright
deviation from "verbatim": `Artifact` reads as both a concrete type (§1) and a trait
bound (§6) — resolved as a trait, `&dyn Artifact` (D19) — and `RunWriter::transact<T>`
cannot compile as a generic method on a trait always handled as `Box<dyn RunWriter>`
— resolved by dropping the generic return in favour of closure-captured results (D20).

**Acceptance**
```bash
cargo build -p arbiter-kernel   # compiles with arbiter-core as its only internal dep
grep -c 'arbiter-store\|arbiter-providers' crates/arbiter-kernel/Cargo.toml   # must be 0
```

---

### K1 · Budget ledger · K2 · Call states · K4 · Bounds

**Spec:** §7, §8.3, §8.4, §11 · **Files:** `arbiter-kernel/src/budget.rs`, `calls.rs`, `bounds.rs`

K1 — reservation protocol. `reserve()` returns a guard whose drop releases the remainder.
The reserve and commit SQL is written out in §8.3; use it. `BUDGET_RELEASED` fires **only
when `reserved` falls without `committed` rising** — the under-estimate remainder returns
inside `CALL_COMPLETED`, carried by `BUDGET_COMMITTED` as `released_remainder`.

The ledger invariant, checked on every `resume` and by `doctor`:
```
budget.reserved == SUM(reserved_amount) over calls in RESERVED|SENT|ACKNOWLEDGED|ORPHANED
```

K2 — the state machine of §8.4. **The row is created at `RESERVED`**, in the same
transaction as `BUDGET_RESERVED`, never at `SENT`. There is no `CREATED`.

Resume classification, from §8.4 — this table *is* the implementation:

| Last committed state | Classify as | Budget |
|---|---|---|
| `RESERVED` | `FAILED` — never dispatched | **release** |
| `SENT` | may have been billed, no id | hold, report orphaned |
| `ACKNOWLEDGED` | may have been billed, id exists | hold, reconcilable |
| `COMPLETED` | nothing to do | already committed |

Retry is **capability-gated**: only where the adapter declares `idempotency: Some(_)`, and
the reservation stays **held** across it. Where `None` — the Anthropic default — the call
becomes `ORPHANED`, nothing retries, and the stage degrades via `SkipItem`.
**`ORPHANED` must never become `FAILED`.**

K4 — `budget_headroom` (5%) is reserved from every planner and released **in the final
round only**. Challenge budget is `remaining_budget ÷ remaining_rounds`, judge share
reserved first. `repair_budget_fraction` is **per round**, not per run.

Scope note: `BudgetLedger`/`ReservationGuard` (K1) and `classify_on_resume`/
`decide_retry`/`validate_transition` (K2) are pure decision logic operating only
through K0's own trait seam — no dependency on `arbiter-store`, so their tests use
an in-memory `BTreeMap`-backed ledger, not real SQLite. Actually persisting a
reservation/call-state change (`Tx::commit_budget`/`set_call_state` in
`arbiter-store`, currently the D21-deferred stub) is separate wiring work for
whichever task first drives a real debate end-to-end (a G-task), not required by
K1/K2/K4's own acceptance tests. `BudgetExhausted`'s mapping to
`StopReason::BudgetExhausted` is left to the controller (G7), which is what
actually constructs `StopReason`.

**Acceptance**
```bash
cargo test -p arbiter-kernel budget::tests::ledger_invariant_holds_after_kill_at_each_state
cargo test -p arbiter-kernel budget::tests::released_remainder_emits_no_second_event
cargo test -p arbiter-kernel calls::tests::orphaned_never_becomes_failed
cargo test -p arbiter-kernel calls::tests::no_retry_without_idempotency
cargo test -p arbiter-kernel calls::tests::reservation_held_across_an_idempotent_retry
cargo test -p arbiter-kernel bounds::tests::headroom_is_untouchable_until_the_final_round
cargo test -p arbiter-kernel bounds::tests::seven_models_deep_stays_under_the_cap
```
The last one is `large_panel_deep`: 7 models × deep priced at $2.03 under the old
panel×rounds sizing, which is why sizing is money-derived.

---

### K3 · StageGraph · K5 · Cache

K3 (`stage.rs`, §7): typed artifacts in/out; idempotency key `(run_id, stage, input_hash)`
— and **not** `policy_version`/`pack_hash`/`table_version`/`config_hash`, which are frozen
at init and constant within a run. Checkpoint per stage = one COMMIT. Bounded concurrency,
per-provider rate limits, circuit breakers.

K5 (`cache.rs`, §7): key is the **full tuple** `(provider, model, params, prompt_hash)` —
never `prompt_hash` alone, or the same prompt to two models collides. Committed in the same
transaction as the call and its budget charge. Replay is cache-only with the network disabled.

K5 done. K3 done for the `Stage` trait / `StageContext` / idempotency-key /
round-control (`ControlFlow`/`StopReason`) surface — `Stage`'s own supporting
types (`RunContext`, `Key`, `CostEstimate`, `StageError`, plus `StageContext`'s
`ProviderRegistry`/`EventSink`/`DeterministicRng`/`CancellationToken` fields) were
as underspecified as K0's were (D19's category of gap, now D24), and the two spec
files even disagree on which axes the idempotency key itself carries (D23,
resolved as the union of both). `ProviderRegistry` stays a near-empty placeholder
until P1 defines `Provider` — nothing real can go in it before that.

**Deferred out of K3's scope, genuinely later work:** bounded concurrency (the
per-item join-set fan-out `positions.generate`/`claims.extract`/`challenge.run`
need), per-provider rate limits, and circuit breakers. These need a concrete
`Provider` (P1) and at least one real multi-item stage to design against
meaningfully — building them now, with nothing yet calling through them, risks
guessing a shape that doesn't fit the first real caller. `Parallelism::PerItem {
max }` (the *declaration* of a stage's desired fan-out) is implemented; the
*executor* that actually bounds concurrency against it is not, and is exactly the
"executor" D24 already flags as the reason `Stage::run` doesn't need `dyn`
object-safety yet either — both wait for the same future task (a G-task or a
dedicated K3b).

**Acceptance**
```bash
cargo test -p arbiter-kernel stage::tests::same_input_hash_is_not_recomputed   # K3, not yet
cargo test -p arbiter-kernel cache::tests::same_prompt_two_models_do_not_collide
cargo test -p arbiter-kernel cache::tests::replay_opens_no_socket
```

---

### P1–P4 · Providers and credentials

**Spec:** §11.1, INTERFACES §5, §25

P1 — `Provider` trait + `ProviderCapabilities { structured_output, streaming, idempotency }`.
Capabilities are **declared per adapter from that provider's documentation**, never assumed.

P2 — the mock is not a stub: it scripts the whole CI fixture suite and **opens no socket**.

P3 — credentials, §11.1:
- resolution order `ARBITER_<P>_API_KEY` → the provider's own var → OS keychain
- **`KeySource` has no `ConfigFile` variant.** A missing enum variant is a stronger
  guarantee than a rule in prose.
- a key-shaped value under `api_key` in any config file **fails startup naming file and
  line**, and prints the two working alternatives
- `SecretString`: no `Display`, no `Debug`, zeroes on drop
- redaction is a **write-path rule covering error strings** — providers echo request
  material into error bodies, and an unredacted 401 is the likeliest path into `~/.arbiter`
- verification never implicit; results cached 24 h keyed by `blake3(key)[..16]`

P4 — Anthropic ships `idempotency: None`; several OpenAI-compatible gateways accept a key.

**Scope note (P1, P2 done; P3, P4 deferred to their own pass):** P1 (`Provider`
trait, `ProviderRequest`/`ProviderResponse`/`ProviderError` in
`arbiter-kernel/src/provider.rs`) and P2 (`MockProvider` in
`arbiter-providers/src/mock.rs`, scripted, structurally socket-free) are
implemented and tested — see D25/D26 in PLAN_DEVIATIONS.md for the types
neither spec file defines and the dyn-dispatch design choice. `ProviderRegistry`
(`arbiter-kernel/src/stage.rs`, K3/D24) is filled in against the real `Provider`
trait. P3 and P4 are deliberately left for a dedicated pass rather than folded
in here: P3 is security-sensitive credential handling (OS keychain integration,
write-path redaction, secret zeroization) that deserves focused attention and
its own review rather than being rushed alongside P1/P2; P4 requires real
`reqwest`/`eventsource-stream` wiring against live Anthropic/OpenAI-compatible
APIs, which has no CI-testable acceptance criterion and depends on P3's
credential resolution existing first.

**Acceptance**
```bash
cargo test -p arbiter-providers keys::tests::config_file_key_fails_and_names_the_file
cargo test -p arbiter-providers keys::tests::secret_string_has_no_debug_impl   # compile-fail test
cargo test -p arbiter-providers keys::tests::key_echoed_in_an_error_body_is_redacted
cargo test -p arbiter-providers keys::tests::rotating_a_key_invalidates_its_cached_result
cargo test -p arbiter-providers mock::tests::mock_opens_no_socket
```

---

### G1–G9 · Prompt packs and the 15 stages

**Spec:** §5, §5.1–§5.6, §15

G1 — packs are content-addressed; `prompt_hash = blake3(rendered template ‖ variable
schema)`, recorded on every `CALL_STARTED`; `pack_hash` snapshotted by `init`. Replay
**refuses** a pack mismatch; `--repack` mints a new run id.

G2–G9 — one task per stage group, each emitting the events INTERFACES §13 names. Points
that are easy to get wrong and are tested individually:

| Stage | The thing to get right |
|---|---|
| `positions.generate` | independent, **no cross-talk** in round 1 |
| `claims.extract` | repair runs on the **cheap** model, capped by the per-round fraction |
| `claims.normalize` | biased toward **splitting** — a merge error corrupts independence, a split only dilutes; partition + stitch **recurses** past 180 claims |
| `options.cluster` | attachment matrix; re-runs each round at deep depth |
| `relations.analyze` | premise cycles: Kahn sort, minimum edge cut, a member with a verified quote **keeps its Fact weight** |
| `disputes.rank` | the deterministic formula, leverage via the counterfactual pass |
| `challenge.plan` | money-derived sizing, judge share first |
| `controller.decide` | both predicates computed from artifacts, **no extra call**; at standard depth it exits on `RoundLimit` by construction |
| `judge.evaluate` | anonymised A–E, **shuffled**; 2 cross-vendor judges at deep |
| `decision.synthesize` | **calls no model** |

**Acceptance** — each stage has a fixture in F2. Additionally:
```bash
cargo test -p arbiter-kernel stages::tests::synthesize_makes_no_provider_call
cargo test -p arbiter-kernel stages::tests::round_one_positions_never_see_each_other
cargo test -p arbiter-kernel stages::tests::judge_sees_no_model_identity
```

**Scope note (G1 done; G2–G9 pending):** G1 (`arbiter-kernel/src/prompt.rs`) is
implemented and tested — `PromptPack::load` (manifest + per-stage `.md` files,
each declaring its variable schema in TOML front-matter), `PromptTemplate::render`
(exact-match variable validation), `PromptTemplate::prompt_hash` (`blake3(rendered
‖ schema)`), and `PromptPack::verify_pack_hash` (replay's pack-mismatch refusal).
See D28 for the manifest/front-matter schema this task had to invent and the
`Hash` → `PromptHash` rename. G1's own scope is the loading/rendering/hashing
machinery, proven against fixtures built in its own test module — **not** the
actual prompt text for any of the 15 pipeline stages. Each `G2`–`G9` task writes
its own stage's real `.md` template(s) as it implements that stage; no
`prompts/` directory with production content exists yet.

**G2 scope note (`init` done; `panel.resolve`/`positions.generate`/
`claims.extract`/`claims.normalize` pending):** `init` (ARCHITECTURE §5: no LLM
call) is implemented — split across `arbiter-kernel/src/init.rs` (pure
question validation) and `arbiter-store/src/init.rs` (opens the run and
appends a correctly hash-chained `RUN_STARTED`, since that needs both
`RunStore` and the chaining machinery neither of which `arbiter-kernel` may
depend on, D1). See D30 for the question-validation rule, `RUN_STARTED`'s
invented payload (the question plus the full `Manifest`), and why `init` is
not itself a `Stage` impl.

`positions.generate` (`arbiter-kernel/src/stages/positions_generate.rs`) is
now also implemented and tested — the first stage with real provider
orchestration: cache-then-reserve-then-call-then-commit through
`StageContext`, a bounded concurrent fan-out across the panel
(`futures_util::buffer_unordered`), `FailurePolicy::SkipItem` on every
per-item failure mode (provider error, unregistered provider, budget
exhausted), and the first real prompt pack content
(`prompts/default/v1/positions.generate.md`). See D31 for `Question`/
`Position`/`Positions`'s invented shapes, the deferred per-provider semaphore,
and why it uses a local `ScriptedProvider` rather than P2's `MockProvider`
(D1: `arbiter-kernel` cannot depend on `arbiter-providers`).

`claims.extract` (`arbiter-kernel/src/stages/claims_extract.rs`) is now also
implemented and tested — the full INTERFACES §2 grounding pipeline per
position: extractor call, exact match, fuzzy match (trigram Jaccard ≥ 0.85),
derived-claim resolution over an acyclic premise graph (Kahn), one repair
call per position covering both plain ungrounded claims and any detected
premise cycle, and — if the cycle survives repair — the untangle-before-degrade
cut-and-recheck step, so a claim whose derivation still traces to a real quote
keeps its evidence kind and only a claim whose sole grounding was the cut edge
falls to `Unsupported`. Reuses `arbiter-core`'s existing `CanonicalClaim`/
`ClaimMember`/`Grounding`/`EvidenceKind` types directly (no parallel types
invented) and `bounds::repair_budget` (K4) for the repair spend cap. See D32
for the extractor/repair JSON shapes this task had to pin, the token-based
exact/fuzzy matcher, and — the one deliberate scope narrowing — cycle-cutting
uses only the greedy-by-ascending-confidence algorithm INTERFACES §2 names for
large SCCs, not the exact brute-force variant it also names for |SCC| ≤ 12.

`claims.normalize` (`arbiter-kernel/src/stages/claims_normalize.rs`) is now
also implemented and tested — clusters `claims.extract`'s singleton claims
into multi-member `CanonicalClaim`s using the T1 (lexical: trigram
IDF-weighted cosine, K-scaling top-K) and T3 (batched LLM grouping call, with
`t3_merge_threshold`/`t3_max_claims_per_batch`, connected-component
partitioning, first-fit-decreasing packing, and a one-level stitch pass)
machinery INTERFACES §3 gives — the only concrete "cheap similarity"
algorithm either spec file provides anywhere, reused here rather than
inventing a second one. See D33 for the full reasoning (including why that
machinery, textually anchored to `relations.analyze`, belongs here too), the
K-formula transcription, the merge-kind rule, and the stitch-recursion
narrowing (one level, not the full depth-2 protocol). T2 (polarity sweep,
needs `options.cluster`'s output) stays out of scope, deferred to
`relations.analyze`'s own future pass.

`panel.resolve` remains its own pass — it needs `correlation.toml`, not yet
shipped (`crates/arbiter-core/data/correlation.toml`, per ARCHITECTURE §6.2).
With `init`, `positions.generate`, `claims.extract` and `claims.normalize` all
landed, G2 is functionally complete for the no-panel-recommendation path
(explicit panel selection, which ARCHITECTURE §5 itself calls "the default
path" — recommendation "is never a mandatory dependency").

**G3** (`arbiter-kernel/src/stages/options_cluster.rs`) is implemented and
tested — INTERFACES §20's Steps 1–2: clusters positions into
`DecisionOption`s (one batched LLM call, same "no option is ever invented"
invariant as `claims.normalize`'s clustering — an unmentioned or
unparseable-response position always becomes its own option, never dropped
or merged away) and produces the *direct* `AttachmentMatrix` (claims seeded
`Authored` toward their own position's option, then one batched classifier
call that may revise any cell to `Classified`/`supports`/`opposes`, or
remove it via `neutral`). Step 3 (deterministic propagation) and §6.5 scoring
were already fully built and tested by C4
(`arbiter_core::decision::attachment::{propagate, score_options}`) — this
task calls neither, since propagation needs `relations: &[Relation]`, which
don't exist until `relations.analyze` (G4) runs, one stage later. See D34 for
the multi-artifact `Stage::In` gap this task had to close (a `ClusterInput`
wrapper combining positions and claims — the first stage needing more than
one upstream artifact) and the cluster/attach prompt contracts.

**G4** (`arbiter-kernel/src/stages/relations_analyze.rs`) is implemented and
tested — T1 (the same lexical candidate generation `claims.normalize` uses,
now factored out into a shared `stages/similarity.rs` module rather than
duplicated a third time) unioned with T2 (the polarity sweep deferred out of
G3's scope above: cross-model claim pairs with opposing polarity cells on the
same clustered option, per `ClusteredOptions.direct_matrix`), then one batched
pairwise LLM call per candidate batch classifying each pair into
`RelationKind` (`Supports`/`Contradicts`/`Qualifies`/`Unrelated`/`Uncertain`)
with an explicit `from`/`to` direction and confidence. Output is a flat
`Vec<Relation>` (`AnalyzedRelations`), already the exact shape
`arbiter_core::decision::fixpoint::solve` and `decision::attachment::propagate`
expect — no adapter needed when a later stage wires them together. See D35 for
the `similarity.rs` extraction, the T2 literal reading, and the direction/
batch-size choices.

**G5** (`arbiter-kernel/src/stages/disputes_rank.rs` +
`arbiter-kernel/src/stages/challenge_plan.rs`, plus a new
`arbiter-core/src/decision/dispute.rs`) is implemented and tested.
`disputes.rank` is the stage that finally resolves the argument graph: it
runs the C2 fixpoint over real pipeline claims/relations for the first time,
runs Step 3 attachment propagation (`options.cluster`/D34 predicted exactly
this handoff — "whichever later stage first holds both a matrix and a
relation graph together"), classifies every claim's standing (C3), and ranks
every `Disputed`/`Unresolved` claim by INTERFACES §21's `dispute_priority`
formula — `contested_mass` and `evidence_gap` computed fresh in the new
`decision::dispute` module, `decision_leverage` reusing C7's
`CounterfactualFlip::leverage()` exactly as §21 itself says to.
`challenge.plan` spends a money-derived challenge budget (judge's share
reserved first, per §5.5) top-down over that ranking, selecting a
challenger for each affordable dispute via §21's confidence-weighted
attacker-standing rule and a per-round-not-per-claim per-model cap. See D36
and D37 for the `ResolvedGraph`/`PolicyConfig` signature gap, the
`contested_mass` normalisation reading, the `remaining_rounds` formula, "the
claim's author" generalised to a claim's full asserter set, and a
`RELATIONSHIP_FOUND` emission gap in already-shipped G4 fixed in passing.

---

## 6. Tasks — CLI

**Spec:** §12 · **Files:** `arbiter-cli/src/`

The CLI is a renderer. **No decision logic lives here** — every number it prints comes out
of `arbiter-core`. `--json` on every read command emits the structure the human renderer
itself consumes, so the two can never drift.

| Task | Commands |
|---|---|
| **L1** | `run <question\|file> --panel --depth --budget --json --stream` |
| **L2** | `show`, `explain [claim] [--json]`, `claims --state`, `history` |
| **L3** | `resume`, `replay [--repolicy] [--repack]` |
| **L4** | `accept [--override path=value --reason]`, `keys list\|set\|test\|rm`, `providers list\|test`, `doctor [--gc]`, `reindex`, `export --format` |

**Acceptance**
```bash
arbiter run tests/q.md --panel mock --depth standard --json | tail -1 | jq -e .outcome
arbiter explain "$RID" --json | jq -e '[.confidence.penalties[].contribution] | length == 5'
arbiter explain "$RID" --json | jq -e '
  ([.confidence.dimensions[].contribution] + [.confidence.penalties[].contribution] | add)
  as $s | ($s - .confidence.total | fabs) < 1e-9'
arbiter replay "$RID" --json | diff - <(arbiter show "$RID" --json)   # byte-identical
arbiter keys list   # prints sources and fingerprints, never a key
```

`doctor` must report, per §11.1 and §8.5: key state per provider, correlation-table
staleness, models missing from the table, provisional constants, runs stuck in `running`,
the ledger invariant, orphaned spend, and orphaned blobs.

---

## 7. Tasks — the UI

The UI is **minimal, not partial**. Minimal means few screens; it does not mean a screen
may omit a state. Every item below is required.

### U1 · `arbiter serve` — server and admission

**Spec:** §17.1, INTERFACES §24 · **Files:** `arbiter-cli/src/serve/`

One embedded HTML page (`include_str!`) and seven endpoints. No build step, no npm, no
bundler, no framework, no CDN.

| Method | Path | Returns |
|---|---|---|
| `GET` | `/` | the page |
| `POST` | `/api/runs` | `{run_id}`, `202` — **spends money** |
| `GET` | `/api/runs` | `run_catalog` rows |
| `GET` | `/api/runs/:id` | the `explain --json` payload **verbatim** |
| `GET` | `/api/runs/:id/events` | `text/event-stream` |
| `POST` | `/api/runs/:id/accept` | the acceptance record |
| `GET` | `/api/providers` | the roster of INTERFACES §25 |
| `POST` | `/api/providers/:p/test` | updated rows — **makes a paid request** |

**Admission — every request, in this order, `403` and no body at the first failure.**
Rejection happens *before* any run state is read, so a probe cannot learn whether a run id
exists.

```
1. socket bound to 127.0.0.1     (enforced at startup; any other address is REFUSED)
2. Host: 127.0.0.1[:p] | localhost[:p]        <- this is what closes DNS rebinding
3. Origin absent, or exactly this server's origin
4. Sec-Fetch-Site absent, or same-origin
5. token matches, compared in constant time
```

No CORS headers, ever. The token is 128-bit, per process, printed once in the URL `--open`
opens, and **never written to `~/.arbiter`, a log, or an event payload** — it would outlive
the process that owns it and end up in an exported run.

`GET /api/runs/:id` returns the §22 payload **unchanged**. The moment the server reshapes
it, the page and the CLI explain decisions through two code paths.

SSE: each `data:` line is one event envelope, byte-identical to `--stream`. A reconnect
sends `Last-Event-ID` and the server resumes from `sequence + 1` — events are already
sequenced and durable, so this needs no buffering.

**Acceptance**
```bash
cargo test -p arbiter-cli serve::tests::binding_non_loopback_is_refused
cargo test -p arbiter-cli serve::tests::wrong_host_header_is_403        # DNS rebinding
cargo test -p arbiter-cli serve::tests::foreign_origin_is_403
cargo test -p arbiter-cli serve::tests::missing_token_is_403
cargo test -p arbiter-cli serve::tests::rejection_precedes_run_lookup   # no id oracle
cargo test -p arbiter-cli serve::tests::no_cors_headers_are_ever_sent
cargo test -p arbiter-cli serve::tests::token_absent_from_store_and_log
cargo test -p arbiter-cli serve::tests::explain_endpoint_matches_cli_byte_for_byte
cargo test -p arbiter-cli serve::tests::sse_resumes_from_last_event_id
```

---

### U2 · Screen 1 — New run

**Source:** `GET /api/providers` · **Reference:** `design/minimal-ui.html` screen 1

| Element | Required behaviour |
|---|---|
| Question | textarea, multi-line, autofocus; accepts a pasted paragraph; a file path is accepted too |
| Panel list | **every** model listed, usable or not — see the states table below |
| Depth | `standard — 1 round` / `deep — up to 3 rounds` |
| Budget cap | pre-filled from config; editable; validated as currency |
| Estimate | **shown before the button**, recomputed when the panel or depth changes |
| Independence warning | shown when usable groups < 3, naming the consequence |
| Manage keys | link to screen 5 |
| Start | primary; disabled while 0 models are usable |

**Panel row states — all five must render:**

| Key state | Row | Action |
|---|---|---|
| `Verified` | enabled, checked by default, `● ready` | — |
| `Present` | enabled, `● key set, not checked` | — |
| `Rejected` | **disabled, still listed**, `● key rejected · <status>` | *Fix* → screen 5 |
| `Missing` | **disabled, still listed**, `○ no key` | *Add* → screen 5 |
| provider unreachable | disabled, `○ provider unreachable` | *Re-check* |

**Unusable models are never hidden.** Hiding them makes an empty panel look like a broken
install and conceals why confidence will be lower.

The estimate must state **cost, call count, wall-clock and model count**, and must fall
when models are unusable — it is sized from the *usable* panel, not the full one.

**Screen states:** loading (skeleton, Start disabled) · no providers configured at all
(route to screen 5 with an explanation, do not show an empty form) · `/api/providers`
returns 5xx (message + Retry, Start disabled) · submitting (Start disabled with a spinner,
double-submit impossible) · `POST /api/runs` rejected (show the reason inline, keep the
typed question).

---

### U3 · Screen 2 — Running

**Source:** `GET /api/runs/:id/events` (SSE)

| Element | Required behaviour |
|---|---|
| Question + run id | run id in the **URL**, so the page is linkable and survives a refresh |
| Current stage | name, `round r of R`, `step n of 15` |
| Progress | fraction of stages complete — never a fake time estimate |
| Spend | `$x.xxx of $y.yy`, updating live |
| Claims count | live |
| Elapsed | live |
| Event log | newest first, `ts · EVENT_TYPE · detail`, scrollable, **auto-scroll pauses on hover** |
| Stop | always reachable, never behind a menu |
| Detach note | **“Closing this page does not stop the run.”** — required, not optional copy |

**Screen states:** connecting · streaming · **SSE dropped** (banner “reconnecting…”, then
resume via `Last-Event-ID`; never silently show a frozen page) · run completed while
watching (auto-navigate to screen 3) · run failed (`RUN_FAILED` reason + what was kept) ·
budget exhausted mid-run (say the decision was synthesised from evidence so far and is
marked truncated) · a call went `ORPHANED` (surface it here, do not wait for the result
screen) · opening the URL of an already-finished run (redirect to screen 3) · opening the
URL of an unknown run (404 page, not a blank stream).

---

### U4 · Screen 3 — Result

**Source:** `GET /api/runs/:id` — the `explain --json` payload

| Element | Required behaviour |
|---|---|
| Outcome tag | all four: `CONSENSUS`, `MAJORITY_WITH_DISSENT`, `SPLIT_DECISION`, `INSUFFICIENT_EVIDENCE` |
| Winning option | the label, not the id |
| Metrics | confidence, margin, claim count |
| **Live objection** | when the outcome is not `CONSENSUS`, the surviving objection is shown **immediately under the answer**, not buried |
| Options table | every option, share, support/oppose counts, `below floor` flag |
| Confidence breakdown | 3 dimensions + base + **5 penalties** + total; inactive penalties shown at 0, not hidden |
| Claims | id, text, standing, state pill, kind; filter for disputed/unresolved; “show all” |
| Change triggers | each unresolved claim that would flip the winner, with `margin_before → margin_after` |
| Run integrity | chain verified · fixpoint converged/not · completeness · `policy_version` · orphaned spend if any |
| Accept | records who and when |
| Accept with override | **requires a non-empty reason**; recorded as `UserOverride` provenance |
| Export | `--format json\|markdown` |

Reading rules that must survive implementation:
- `MAJORITY_WITH_DISSENT` means something survived. **A layout that hides it lies.**
- `INSUFFICIENT_EVIDENCE` must not render as a weak winner — show why the floor was not met.
- Confidence is never a bare number: the breakdown is one click away and sums to it.
- `policy_version` is always on screen, because decisions compare only within one.

**Screen states:** loading · run still running (redirect to screen 2) · run failed
(show what exists, no fake decision) · truncated by budget (banner) ·
fixpoint not converged (banner + the 0.05 penalty visible in the breakdown) ·
chain break detected (**prominent** — the run is unverifiable) · already accepted
(show who and when, Accept becomes “Accepted”).

---

### U5 · Screen 4 — History · U6 · Screen 5 — Keys

**U5 source:** `GET /api/runs`

Table of question, outcome, confidence, cost, date; row links to screen 3. Filters for
outcome, min confidence and **`policy_version`** — the last is not optional, and the page
says why. Show `orphaned_cost` when non-zero. States: loading · **empty (first run — offer
screen 1, do not show an empty table)** · error.

**U6 source:** `GET /api/providers`, `POST /api/providers/:p/test`

| Element | Required behaviour |
|---|---|
| Per-provider row | state dot, provider, **source** (`ANTHROPIC_API_KEY` / keychain / `ARBITER_*`), fingerprint (last 4 of `blake3(key)`), last checked |
| Re-check | labelled as costing **one request** *before* the click |
| Add key | provider select + masked input; **Save to keychain**; “Check without saving” |
| Resolution order | the three sources, in precedence order |
| Config refusal | states plainly that config files are never read for a key |

**Never render a key.** Fingerprints only. `source` must be shown because *“I updated my
key and nothing changed”* is almost always a higher-precedence variable still set.

States: no keys at all (first-run copy, not an empty table) · checking (row spinner,
button disabled) · check failed (status code + what to do) · keychain unavailable on this
OS (say so, fall back to env-var guidance).

---

### U7 · Cross-cutting UI requirements

**Every screen:**
- server unreachable → one message, not a spinner forever
- `403` from admission → “this page is stale, reopen from the terminal” (the token rotates per process)
- keyboard reachable end to end; visible focus rings; no pointer-only affordance
- contrast ≥ 4.5:1; state never carried by colour alone — every dot has a text label
- `prefers-reduced-motion` respected (the only motion is the run spinner)
- no `localStorage` beyond the active run id
- **the page computes no number** — it formats what the API returned

**Out of scope, and must be refused if requested during 1.1:** argument graph, attachment
matrix, replay scrubbing, plugin management, config editing, multi-user, auth beyond the
token, and any screen that cannot be read from `explain --json` without new engine work.

**Acceptance for U2–U7** (Playwright, headless, against the mock provider):
```bash
cargo test -p arbiter-cli --test ui
```
covering, one test each: all 5 panel key states render · the estimate falls when a model is
unusable · Start is disabled with 0 usable models · the detach note is present ·
SSE reconnect resumes without duplicate events · a non-`CONSENSUS` result shows the live
objection above the fold · the breakdown lists 5 penalties · override requires a reason ·
history is empty-stated · keys screen never renders a key · every screen is keyboard-navigable.

---

## 8. Fixture ledger

All 36 CI fixtures from §18, each owned by exactly one task. **A fixture without an owner
is a plan bug.** All run against the scripted mock with **zero LLM tokens**.

| Fixture | Owner | Fixture | Owner |
|---|---|---|---|
| `simple_consensus` | F2 | `premise_cycle` | G4 |
| `split_decision` | C5 | `fixpoint_nonconvergence` | C2 |
| `strong_dissent` | C3 | `confidence_arithmetic` | C6 |
| `insufficient_evidence` | C5 | `option_floor` | C5 |
| `malformed_claim` | G2 | `decision_override` | L4 |
| `ungrounded_claim` | G2 | `premise_cycle_grounded_fact` | G4 |
| `provider_timeout` | K2 | `attack_saturation` | C2 |
| `budget_exceeded` | K4 | `t3_batch_partition` | G2 |
| `judge_failure` | G8 | `option_clustering` | G3 |
| `adaptive_stop` | G7 | `option_emerges_midround` | G3 |
| `crash_midcall` | K2 | `focus_selection` | G5 |
| `interrupted_commit` | S3 | `option_supersede` | C4 |
| `crash_before_send` | K2 | `judge_dispersion` | C6 |
| `budget_reconciliation` | K1 | `cites_defeated_claim` | C1 |
| `projection_rebuild` | S4 | `prompt_pack_mismatch` | G1 |
| `serve_localhost_only` | U1 | `large_panel_deep` | K4 |
| `serve_rejects_foreign_origin` | U1 | `key_in_config_refused` | P3 |
| `key_redaction` | P3 | `panel_without_keys` | U2 |

Integration (nightly, real providers, budgeted — **not** in the commit path):
`paraphrase_corpus`, `recommendation_corpus`, `judge_identity_leakage`.

---

## 9. Milestones and gates

### Gate 1.0 — core, all of X/C/S/K/P/G/L/F

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace                       # includes 34 of the 36 CI fixtures
cargo test --workspace -- --ignored          # nothing may be ignored at the gate
```
plus, from §20:
- the decision core passes every golden fixture **without one LLM token**
- exact replay reproduces a `DecisionRecord` **byte-for-byte**
- a killed process resumes and never intentionally repeats a completed call
- standard ≤ $0.50 and deep ≤ $1.20 against list prices, both under the $2.00 cap
- the engine never exceeds its bounds under any controller decision

**Before `argument-v1` drops `provisional` (§6.3), both must run:** the tuning sweep over
the graph corpus, **and** a recorded red-team session of ≥20 adversarial cases. A corpus
sweep alone is not the gate.

### Gate 1.1 — the UI, U1–U7

All of gate 1.0, plus `serve_localhost_only` and `serve_rejects_foreign_origin`
(CI reaches 36), plus the `--test ui` suite, plus a manual pass: start a run from the
browser, watch it stream, accept it, and find it in history.

### Explicitly out of this plan

Build Studio (`arbiter-build`, `arbiter build`), the WASM host, the JSON-RPC host and
confinement are **1.5**. Their interfaces are defined by 1.0 and must not be redesigned to
accommodate them — if a 1.0 interface needs changing for 1.5, that is a finding, not a task.

---

## 10. Progress ledger

Append one row per completed task. Do not mark a row done before §0.3 passes.

| Task | Done | Commit | Deviations |
|---|---|---|---|
| X1 | ✅ | (this commit) | none |
| X2 | ✅ | (this commit) | none |
| C1 | ✅ | (this commit) | D3, D4, D5, D6 — see PLAN_DEVIATIONS.md |
| C2 | ✅ | (this commit) | D7 — see PLAN_DEVIATIONS.md |
| C3 | ✅ | (this commit) | D8 — see PLAN_DEVIATIONS.md |
| C4 | ✅ | (this commit) | D9, D10, D11 — see PLAN_DEVIATIONS.md |
| C5 | ✅ | (this commit) | D12, D13 — see PLAN_DEVIATIONS.md |
| C6 | ✅ | (this commit) | D14, D15 — see PLAN_DEVIATIONS.md |
| C7 | ✅ | (this commit) | D16, D17 — see PLAN_DEVIATIONS.md |
| C8 | ✅ | (this commit) | D18 — see PLAN_DEVIATIONS.md |
| K0 | ✅ | (this commit) | D19, D20 — see PLAN_DEVIATIONS.md |
| S1 | ✅ | (this commit) | D21 — see PLAN_DEVIATIONS.md |
| S2 | ✅ | (this commit) | scope note — see plan text above, no new D-entry |
| S3 | ✅ | (this commit) | D22 — see PLAN_DEVIATIONS.md |
| S4 | ✅ (partial) | (this commit) | D29 — see PLAN_DEVIATIONS.md; budget/provider_calls/cache_entries/artifacts only, see plan text above |
| S5 | ✅ | (this commit) | D27 — see PLAN_DEVIATIONS.md |
| S6 | ✅ | (this commit) | scope note — see plan text above, no new D-entry |
| K1 | ✅ | (this commit) | scope note — see plan text above, no new D-entry |
| K2 | ✅ | (this commit) | scope note — see plan text above, no new D-entry |
| K3 | ✅ | (this commit) | D23, D24 — see PLAN_DEVIATIONS.md; concurrency/rate-limits/circuit-breakers deferred, see plan text above |
| K4 | ✅ | (this commit) | scope note — see plan text above, no new D-entry |
| K5 | ✅ | (this commit) | scope note — see plan text above, no new D-entry |
| P1 | ✅ | (this commit) | D25 — see PLAN_DEVIATIONS.md |
| P2 | ✅ | (this commit) | D26 — see PLAN_DEVIATIONS.md |
| P3 | ☐ | deferred as own pass — security-sensitive (OS keychain, redaction) | |
| P4 | ☐ | deferred as own pass — needs real HTTP adapters against live provider APIs, no CI-testable acceptance criterion | |
| G1 | ✅ | (this commit) | D28 — see PLAN_DEVIATIONS.md; pack machinery only, no production prompt content — see plan text above |
| G2 | ☐ (partial: `init` + `positions.generate` + `claims.extract` + `claims.normalize`; `panel.resolve` deferred) | (this commit) | D30, D31, D32, D33 — see PLAN_DEVIATIONS.md; see plan text above |
| G3 | ✅ | (this commit) | D34 — see PLAN_DEVIATIONS.md; propagate/score_options already built by C4, see plan text above |
| G4 | ✅ | (this commit) | D35 — see PLAN_DEVIATIONS.md; shared `similarity.rs`, T2 polarity sweep literal reading |
| G5 | ✅ | (this commit) | D36, D37 — see PLAN_DEVIATIONS.md; Step 3 propagation runs in disputes.rank |
| G6 | ☐ | | |
| G7 | ☐ | | |
| G8 | ☐ | | |
| G9 | ☐ | | |
| L1 | ☐ | | |
| L2 | ☐ | | |
| L3 | ☐ | | |
| L4 | ☐ | | |
| F1 | ☐ | | |
| F2 | ☐ | | |
| U1 | ☐ | | |
| U2 | ☐ | | |
| U3 | ☐ | | |
| U4 | ☐ | | |
| U5 | ☐ | | |
| U6 | ☐ | | |
| U7 | ☐ | | |
