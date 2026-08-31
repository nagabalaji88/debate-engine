# Arbiter — AI Debate & Decision Engine

**Version:** 2.0 (frozen for implementation)
**Status:** approved — implementation may start
**Supersedes:** spec v1.0 (Python/LangGraph/Postgres/WebSocket)

---

## 0. What this is

A multi-model deliberation engine. Independently-prompted models produce positions;
positions are decomposed into **claims**; claims are related, challenged, defended,
and scored; a **pure, deterministic decision core** — no LLM in any arithmetic —
resolves the claim graph into a decision with decomposed confidence and computed
change triggers. Optionally, the decision is carried downstream into a product
specification, technical architecture, and development prompt.

**Three invariants that govern every design choice below.**

| # | Invariant | Enforced by |
|---|---|---|
| 1 | Claims are the unit of analysis. Model votes never decide anything. | `decision::score_options` reads claim evidence only; the model-alignment vector is a *reported* field |
| 2 | Justified disagreement is a valid terminal outcome. | `OutcomeState` has four equal variants; none is an error state |
| 3 | The decision is the bridge to building. | Build Studio consumes `DecisionRecord`, never the transcript |

---

## 1. Language & topology

**Rust** for the kernel and core. The decision core is a finite state machine over a
closed set of states; Rust's exhaustive `match` makes an unhandled state a compile
error, `serde` gives strict rejection of malformed model output, and there is no
`nil`. Cost accepted: slower builds, steeper contributor ramp.

**Plugin authors never write Rust.** The extension boundary is a process/WASM
boundary carrying JSON, so plugins may be written in Python, TypeScript, Go, or
anything that can speak the contract.

```
                          RUST KERNEL
                               │
                  ┌────────────┴────────────┐
                  │                         │
                WASM                    JSON-RPC
             (sandboxed)            (subprocess, any language)
```

---

## 2. Non-negotiables

These are architectural constraints, not preferences. Every one has a test.

1. **Every persisted event and artifact carries `schema_version`.** NDJSON is
   migration-light, not schema-free. The replay engine dispatches on version.
2. **Exact event replay is distinct from provider re-run.**
   *Exact replay* reads recorded responses, calls no provider, and must be
   byte-identical. *Re-run* calls providers again and produces a new run with a new id.
   The word "deterministic" applies only to the former.
3. **Budget is reserved before a call, not charged after it.**
   `reserve → call → commit(actual) → release(unused)`. Concurrent calls cannot
   collectively overspend, because reservation is atomic against the ledger.
4. **All decision-core states are closed enums.** No stringly-typed states anywhere
   past the provider boundary.
5. **Normalization never destroys originals.** A canonical claim holds references to
   its members; every member keeps `source_model`, `original_text`, `source_span`.
6. **Provenance is first-class**, especially in Build Studio: every substantive
   assertion carries a `ProvenanceKind`.
7. **Every debate is bounded** by rounds, cost, tokens and wall-clock. The controller
   may stop earlier; it can never exceed the bound. Bounds are enforced by the
   kernel, not by the controller that might want to keep going.

---

## 3. Workspace

```
arbiter-core       pure domain + decision engine. No IO, no async, no LLM. 
arbiter-kernel     StageGraph, budget ledger, cache, event store, bounds
arbiter-providers  anthropic · openai-compatible · mock
arbiter-plugin     host + ABI (JSON-RPC subprocess, WASM)
arbiter-store      filesystem/NDJSON implementation of the Store traits
arbiter-build      Build Studio stages (optional, downstream of DecisionRecord)
arbiter-cli        the only frontend in this phase
arbiter-fixtures   golden runs; CI proves the engine without an LLM token
```

Dependency rule: `core` depends on nothing internal. `kernel` depends on `core`.
Everything else depends on `kernel`. Nothing depends on `cli`.

---

## 4. Persistence

```
~/.arbiter/
  runs/<run_id>/
    manifest.json          config snapshot, status, engine + prompt-pack hashes
    events.ndjson          append-only, seq-numbered, hash-chained  ← source of truth
    artifacts/<name>.json  typed, schema-versioned
    cache/<sha256>.json    provider responses, keyed by call fingerprint
    decision.json          the DecisionRecord
  index.ndjson             derived; rebuildable with `arbiter reindex`
```

~400 KB per run with raw payloads, ~90 KB without. No database in this phase; the
`Store` traits keep SQLite/Postgres available as plugins later.

### 4.1 Event envelope

```json
{
  "schema_version": 1,
  "event_id": "evt_01J...",
  "run_id": "run_01J...",
  "sequence": 42,
  "timestamp": "2026-08-31T12:04:11.221Z",
  "stage": "claims.extract",
  "event_type": "CLAIM_EXTRACTED",
  "durable": false,
  "payload": { },
  "content_hash": "blake3:...",
  "previous_event_hash": "blake3:..."
}
```

`previous_event_hash` chains the log: corruption, truncation and tampering are all
detectable by `EventStore::verify`.

### 4.2 Durability protocol

```
append(event)      buffered write
checkpoint(stage)  flush + fsync + record STAGE_CHECKPOINT
verify(run)        recompute chain; torn tail → truncate to last valid line
                   and append LOG_REPAIRED
```

Events with `durable: true` (anything that authorises or records spend) fsync
immediately. Everything else syncs at the stage checkpoint. Resume replays the
verified prefix and continues from the last checkpoint.

---

## 5. Pipeline (14 stages)

```
init → panel.resolve → positions.generate → claims.extract → claims.normalize
  → relations.analyze → disputes.rank → challenge.plan → challenge.run
  → rebuttal.run → controller.decide ⟲ → judge.evaluate → decision.synthesize
                                                                │
                                                    (optional)  ▼
                                        build.product → build.technical → build.prompt
```

| Stage | Does | LLM? |
|---|---|---|
| `init` | validate question, snapshot config, seed RNG, open log | no |
| `panel.resolve` | resolve an explicit panel **or** ask the recommender plugin. Explicit selection is the default path — recommendation is never a mandatory dependency | only if recommending |
| `positions.generate` | parallel, independent, no cross-talk | yes |
| `claims.extract` | structured claims + grounding; repair loop on failure | yes |
| `claims.normalize` | cluster equivalent claims across models into canonical claims, **members preserved** | cheap + LLM tie-break |
| `relations.analyze` | candidate pairs by cheap similarity → LLM classifies top-K | yes (bounded) |
| `disputes.rank` | deterministic priority score | no |
| `challenge.plan` | select targeted pairs within budget; never all-pairs | no |
| `challenge.run` | issue challenges in parallel | yes |
| `rebuttal.run` | defend / modify / withdraw → versioned claim deltas | yes |
| `controller.decide` | continue or stop, inside hard bounds | no |
| `judge.evaluate` | anonymised A–E, shuffled, 9-metric rubric | yes |
| `decision.synthesize` | run the decision core | **no** |
| `build.*` | optional, downstream, isolated | yes |

### 5.1 Claim extraction & grounding

```
position text
   └─► extractor ─► structured claims
                        └─► grounding check
                              ├── exact/fuzzy span found        → Grounding::DirectQuote
                              ├── marked inference w/ premises  → Grounding::Derived
                              └── neither                       → repair once
                                                                  └── still neither
                                                                      → Grounding::Unsupported
                                                                        (admitted as Unverified,
                                                                         weight 0.15)
```

Rejecting unsupported claims outright would delete exactly the kind of unevidenced-
but-real risk that dissent is made of. They are admitted at low weight instead, and
the low weight is what the decision core reasons with.

### 5.2 Normalization preserves provenance

```
CanonicalClaim { id, text, members: [ClaimMember] }
ClaimMember    { claim_id, source_model, original_text, source_span, grounding }
```

---

## 6. Decision core (pure, deterministic, no LLM)

Input: canonical claims, relations, challenge/rebuttal outcomes, judge scorecards.
Output: `DecisionRecord`. Every function here is total, synchronous, and unit-tested
against golden fixtures.

**6.1 Two orthogonal claim states**

```
ClaimLifecycle : Proposed | Verified | Challenged | Defended | Modified(v) | Withdrawn | Rejected
ClaimStanding  : Agreed | Disputed | Unresolved | Defeated        (computed, never authored)
```

**6.2 Evidence strength** `E(c) ∈ [0,1]`

```
E(c) = kind_weight(evidence_kind)          Fact 1.00 · Inference 0.75 · Assumption 0.50
     × survival(lifecycle)                 Opinion 0.35 · Unverified 0.15
     × independence(members)               Defended 1.0 · Modified 0.7 · Withdrawn 0.0
     × judge_evidence_factor(position)     same-provider members count fractionally
```

`independence` is why four models from one vendor cannot manufacture agreement.

**6.3 Argumentation fixpoint**

Claims form a graph of `Supports | Contradicts | Qualifies | Unrelated | Uncertain`.
Standing is the fixpoint of

```
standing(c) = clamp( E(c) + Σ w·standing(s) for s ⊳ supports
                          − Σ w·standing(a) for a ⊳ contradicts )
```

iterated to convergence (damped, ≤64 iterations, deterministic ordering by claim id).
A defeat is always explainable: *"C-024 fell to C-011 — higher evidence, survived challenge."*

**6.4 Option scoring** — options are recommendation clusters, scored from supporting
claim standing minus surviving contradiction mass. Model vote share is not an input.

**6.5 Outcome classification** — evaluated in this order, thresholds from config:

```
INSUFFICIENT_EVIDENCE   evidence_mass < τ_min  OR  unresolved_critical_ratio > τ_open
SPLIT_DECISION          margin(top1, top2) < τ_gap
CONSENSUS               no surviving contradiction > τ_dissent AND every model aligned or silent
MAJORITY_WITH_DISSENT   otherwise
```

**6.6 Confidence — decomposed, never a single opaque number**

```
evidence_mass      0.88
decision_margin    0.81
judge_score        0.91
unresolved_penalty −0.07
assumption_penalty −0.04
─────────────────────────
confidence         0.84
```

Every component is stored and printed by `arbiter explain`.

**6.7 Counterfactual change triggers**

For each unresolved or disputed claim, flip its standing and recompute the outcome.
Any flip that changes the winning option **is** a change trigger — computed, not
generated. This is the cheapest high-value artifact in the system.

---

## 7. Kernel

**StageGraph** — typed artifacts in/out, content-hash idempotency keys per
`(run, stage, input_hash)`, checkpoint per stage, resume from the verified log,
bounded concurrency, per-provider rate limits and circuit breakers.

**Budget ledger** — reservation protocol (§2.3). `reserve()` returns a guard;
dropping it releases the unused remainder. A reservation that cannot be satisfied
fails the call, and the controller sees a `BudgetExhausted` stop reason.

**Hard bounds** — `max_rounds`, `max_cost`, `max_tokens`, `max_wall_time`. Checked by
the kernel before dispatching any stage or call.

**Cache** — `(provider, model, params, prompt_hash) → response`, content-addressed.
Exact replay is cache-only with the network disabled.

---

## 8. Plugin planes

Two tiers, deliberately. Building a plugin framework instead of a debate engine is
the failure mode being avoided.

| Tier | Plane | Phase-1 mechanism |
|---|---|---|
| **Stable** (public ABI, versioned) | `Provider` · `Judge` · `Store` · `Exporter` | in-process traits **and** JSON-RPC/WASM |
| **Internal** (traits only, no public ABI yet) | `Stage` · `Extractor` · `Relation` · `Policy` | in-process traits |

Internal planes become public when a second real implementation exists. A
`plugin.toml` declares kind, capabilities, config schema and required permissions
(network, filesystem).

---

## 9. Judge

Anonymise (`A…E`) → shuffle → score against the 9-metric rubric → aggregate.
`judge_count = 1` by default; the aggregation interface exists so `> 1` needs no
redesign. The judge never sees model identity, provider, or panel order, and its
score is *one term* of confidence — not the decision.

---

## 10. Build Studio (optional, isolated)

```
DecisionRecord ──┬─► export (json | markdown)
                 └─► build.product ─► build.technical ─► build.prompt
```

Build Studio can never be required to complete a debate; a debate that stops at
`decision.synthesize` is complete.

**Provenance, not citation.** "Use PostgreSQL" is a derived decision, not a claim
needing a URL. Every substantive assertion carries:

```
ProvenanceKind : DebateClaim(claim_id) | DecisionField(path) | UserRequirement(id)
               | ArchitectInference(rationale) | ExternalSource(uri)
```

The gate is **zero unattributed assertions**. `ArchitectInference` is legal, counted,
and reported, so the reader can see how much of the spec the architect invented.

---

## 11. CLI

```
arbiter run <question|file>   --panel · --depth · --budget · --json · --stream
arbiter resume <run_id>
arbiter show <run_id>         [--claims | --decision | --transcript]
arbiter explain <run_id> [claim_id]     confidence terms, defeat chains, triggers
arbiter claims <run_id>       [--state agreed|disputed|unresolved]
arbiter replay <run_id>       exact event replay, no provider calls
arbiter history               [--outcome · --since · --min-confidence]
arbiter export <run_id>       --format json|markdown
arbiter plugins list|info
arbiter providers list|test
arbiter doctor                preflight: keys, reachability, schema, bounds
```

Machine mode emits the NDJSON event stream on stdout — the same stream a web
frontend or API would consume later, which is why the frontend can wait.

---

## 12. Test strategy

`arbiter-fixtures` carries golden runs covering:

```
simple_consensus · split_decision · strong_dissent · insufficient_evidence
provider_timeout · malformed_claim · budget_exceeded · judge_failure · adaptive_stop
```

Each fixture is a recorded event log plus the expected `DecisionRecord`. CI runs the
entire engine — extraction, relations, argumentation, classification, confidence,
counterfactuals — with the `mock` provider and **zero LLM tokens**. The decision core
additionally carries property tests (monotonicity: more supporting evidence never
lowers standing; determinism: identical inputs, identical output, any iteration order).

---

## 13. Build order

1. `arbiter-core` — types, closed enums, decision core, property tests
2. `arbiter-fixtures` — golden runs for the four outcome classes
3. `arbiter-kernel` — StageGraph, event store, budget ledger, bounds
4. `arbiter-providers` — mock first, then anthropic + openai-compatible
5. `arbiter-store` — filesystem/NDJSON
6. `arbiter-plugin` — JSON-RPC host, then WASM
7. `arbiter-cli`
8. `arbiter-build` — Build Studio
9. *(later, separate)* API / UI — consumers of the proven engine

The decision engine is provably correct before a single real token is spent.
