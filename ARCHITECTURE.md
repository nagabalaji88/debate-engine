# AI Debate & Decision Engine — Architecture Specification

**Version:** 2.0 (frozen for implementation)
**Status:** approved — implementation in progress
**Supersedes:** v1.0 (Python · LangGraph · Postgres · FastAPI · WebSocket · React)
**Companion:** `docs/INTERFACES.md` — concrete trait definitions and protocols
**Last updated:** 2026-08-31

---

## 1. Core product definition

A multi-model deliberation system. Independently-prompted models analyse a problem,
present structured positions, challenge competing claims, defend or revise them, and
are evaluated by an independent judge. The system identifies agreement, disagreement,
evidence strength and unresolved uncertainty before producing an explainable
decision. That decision can optionally be transformed into a product specification,
technical architecture and production-ready development prompt.

### 1.1 Three principles

These lock the architecture and prevent scope creep. Each now has a named enforcement
point in code, not just a statement of intent.

| # | Principle | Enforced by |
|---|---|---|
| 1 | **Models don't vote — claims are evaluated.** Not "4 of 5 models say microservices" but "evidence for microservices is strong (0.78), operational-complexity concerns unresolved (0.42)". | `decision::options::score` reads claim standing only. Model alignment is a *reported* field, never an input to the score. Enforced by test `vote_share_does_not_move_the_decision`. |
| 2 | **Consensus is optional — justified disagreement is valid.** Not a forced average but "no consensus: evidence insufficient to resolve". | `OutcomeState` has four equal terminal variants. None renders or classifies as an error. |
| 3 | **The debate isn't the product — the decision is the bridge to building.** | Build Studio consumes `DecisionRecord`, never the transcript. It is optional and downstream; a debate that stops at `decision.synthesize` is complete. |

---

## 2. What changed from v1.0, and why

| v1.0 | v2.0 | Reason |
|---|---|---|
| Python + LangGraph state machine | **Rust + own `StageGraph`** | The decision core is a finite state machine over closed states. Exhaustive `match` turns "someone added `CONDITIONAL_CONSENSUS`" into a compile error rather than a silent fallthrough. A framework's node model would also become the plugin ceiling. |
| Postgres + SQLAlchemy | **Filesystem + hash-chained NDJSON** | ~400 KB/run, no query workload beyond "list and filter my runs". A CLI must run with zero infrastructure. `Store` traits keep SQLite/Postgres available as plugins. |
| FastAPI + WebSocket + React | **CLI first; NDJSON event stream on stdout** | The same stream a UI or API would consume. Building the frontend later makes it a consumer of a proven engine. |
| 5 "pillars" as services | **Pure kernel + 7 extension planes in two tiers** | Pillars were layers, not seams. Seams must be typed contracts with a versioned ABI. |
| Confidence = judge output | **Confidence decomposed into 5 reported terms** | The judge scores one input among several. Arithmetic never happens inside a model. |
| Consensus by claim counting | **Weighted argumentation fixpoint** | "Claims are evaluated" requires actual defeat logic, not tallies. |
| Fixed 1 rebuttal round | **Adaptive controller inside hard bounds** | The controller may stop early; it can never exceed rounds, cost, tokens or wall-clock. |
| Cost tracked per operation | **Budget *reserved* before each call** | Concurrent calls that each read "$0.40 remaining" can collectively overspend. Reservation is atomic against the ledger. |

---

## 3. Seven non-negotiables

Architectural constraints, not preferences. Each has a test.

1. **Every persisted event and artifact carries `schema_version`.** NDJSON is
   migration-light, not schema-free; the replay engine dispatches on version.
2. **Exact event replay is distinct from provider re-run.** *Exact replay* reads
   recorded responses, calls no provider, and is byte-identical. *Re-run* calls
   providers again and produces a new run with a new id. "Deterministic" applies only
   to the former — LLM generation is not deterministic even at temperature 0.
3. **Budget is reserved before a call, not charged after it.**
   `reserve → call → commit(actual) → release(unused)`.
4. **All decision-core states are closed enums.** No stringly-typed states past the
   provider boundary.
5. **Normalization never destroys originals.** A canonical claim references members;
   each member keeps `model`, `provider`, `original_text`, `grounding`.
6. **Provenance is first-class**, especially in Build Studio.
7. **Every debate is bounded** by rounds, cost, tokens and wall-clock, enforced by the
   kernel rather than by the controller that might want to keep going.

---

## 4. Language and topology

**Rust** for kernel and core: closed enums with exhaustive matching, `serde` strictness
against malformed model output, no `nil`. Costs accepted: slower builds, steeper
contributor ramp, more async ceremony than Go.

**Plugin authors never write Rust.** The extension boundary is a process/WASM boundary
carrying JSON.

```
                          RUST KERNEL
                               │
                  ┌────────────┴────────────┐
                  │                         │
                WASM                    JSON-RPC
             (sandboxed)          (subprocess — Python / TS / Go / any)
```

### 4.1 Workspace

```
arbiter-core       pure domain + decision engine. No IO, no async, no LLM.
arbiter-kernel     StageGraph, budget ledger, cache, event store, bounds
arbiter-providers  anthropic · openai-compatible · mock
arbiter-plugin     host + ABI (JSON-RPC subprocess, WASM)
arbiter-store      filesystem/NDJSON implementation of the Store traits
arbiter-build      Build Studio stages (optional, downstream of DecisionRecord)
arbiter-cli        the only frontend in this phase
arbiter-fixtures   golden runs; CI proves the engine with zero LLM tokens
```

Dependency rule: `core` depends on nothing internal; `kernel` depends on `core`;
everything else depends on `kernel`; nothing depends on `cli`.

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

| Stage | Does | LLM |
|---|---|---|
| `init` | validate question, snapshot config, seed RNG, open log | no |
| `panel.resolve` | resolve an **explicit** panel, or ask the recommender plugin. Explicit selection is the default path; recommendation is never a mandatory dependency | only if recommending |
| `positions.generate` | parallel, independent, no cross-talk | yes |
| `claims.extract` | structured claims + grounding, with a repair loop | yes |
| `claims.normalize` | cluster equivalent claims across models; **members preserved** | cheap similarity + LLM tie-break on top-K |
| `relations.analyze` | candidate pairs by cheap similarity → LLM classifies top-K only | yes (bounded) |
| `disputes.rank` | deterministic dispute-priority score | no |
| `challenge.plan` | select targeted pairs within budget; never all-pairs | no |
| `challenge.run` | issue challenges in parallel | yes |
| `rebuttal.run` | defend / modify / withdraw → versioned claim deltas | yes |
| `controller.decide` | continue or stop, inside hard bounds | no |
| `judge.evaluate` | anonymised A–E, shuffled, 9-metric rubric | yes |
| `decision.synthesize` | run the decision core | **no** |
| `build.*` | optional, isolated, downstream | yes |

### 5.1 Claim extraction and grounding

```
position text
   └─► extractor ─► structured claims
                        └─► grounding check
                              ├── span found verbatim/fuzzy ≥0.9 → Grounding::DirectQuote
                              ├── marked inference with premises → Grounding::Derived
                              └── neither → repair once
                                             └── still neither → Grounding::Unsupported
                                                                 (admitted as Unverified, weight 0.15)
```

Source-span validation works for *"our team has 8 developers"* but not for
*"therefore a modular monolith is safer"* — the second is an inference, not a quote.
Hence three variants, not two. `Unsupported` is admitted at low weight rather than
rejected: unevidenced-but-real risk is exactly what dissent is made of.

Matching is mechanical — normalised exact substring, then trigram-Jaccard ≥ 0.85, then
premise resolution for inferences. Repair is **one extra call per position** (never per
claim), capped at 2,048 output tokens and budget-reserved like any other call, so
extraction cost stays bounded.

Premise graphs are **topologically sorted (Kahn) before `relations.analyze`**. A model
can emit *A derived from B, B derived from A*; on a detected cycle every claim in the
component degrades to `Unsupported` at 0.15 weight and the run emits
`PREMISE_CYCLE_DETECTED`. Premise cycles are malformed extraction — relation cycles
(*A contradicts B contradicts A*) are ordinary dialectic and are handled by the
fixpoint, not rejected. Full protocol: `docs/INTERFACES.md` §2.

### 5.2 Normalization preserves provenance

```
Claude:  "Microservices introduce operational overhead."
GPT:     "Microservices increase deployment complexity."
Gemini:  "Service-based architectures require additional operational infrastructure."
                              │
                              ▼
CanonicalClaim { id, text, kind, lifecycle, members: [ClaimMember] }
ClaimMember    { claim_id, model, provider, position, original_text, grounding }
```

Originals are never destroyed; every derived number traces back to a member.

### 5.3 Relationship detection

Two-stage, because N² LLM calls over 30–60 claims is unaffordable and unnecessary:

```
claims → normalise → trigram SimHash blocking → IDF-weighted cosine
       → top-K pairs (K = 12) + polarity sweep → LLM classification
       → RelationKind + confidence
```

Candidate generation is a **union of three tiers**, because pure lexical blocking has a
recall hole that corrupts arithmetic rather than just display: *"Kubernetes deployment
overhead"* and *"container orchestration maintenance workload"* share no useful tokens,
and missing the pair creates two canonical claims for one point — diluting
`independence` and `corroboration`.

| Tier | Method | Cost |
|---|---|---|
| T1 lexical | trigram SimHash blocking → IDF cosine → top-K (K=12) | free |
| T2 polarity | every cross-model pair attached to opposing options | free |
| T3 batch-LLM | **one** call over the whole claim set: "group claims stating the same point" | ≈ $0.01 |

T3 closes the synonymy hole at O(1) calls — the objection to LLM similarity was
pair-wise cost, which a single batched grouping call does not incur. Still **no
embedding model and no vector store**: a local ONNX sidecar would add a native build
dependency, a 23–90 MB model to version, and float drift across runtimes, while being
weaker at *"is this the same claim"* than a cheap model reading them. It remains
available as a plugin behind the `Similarity` trait.

Selected pairs are recorded as `CANDIDATES_SELECTED`, and exact replay reads the
recorded set — so T3's non-determinism can never change a replayed decision.

`RelationKind` is a closed enum: `Supports · Contradicts · Qualifies · Unrelated · Uncertain`.
`Uncertain` is recorded and carries zero weight — the classifier declining to commit
is data, not a failure.

### 5.4 Adaptive controller

Continues while critical claims remain disputed, evidence is contradictory, or
positions are still converging. Stops when no new information is arriving, positions
have stabilised, or the judge can decide.

**Hard bounds it can never exceed** (kernel-enforced, config-set):

```
max_rounds       1 (--depth standard) · 3 (--depth deep) · hard ceiling 6
max_cost         default $2.00
max_tokens       configurable
max_wall_time    default 300s
```

The controller decides two things, not one: whether to continue, **and which disputes
to spend the next round on**. Choosing 6 challenge pairs from 40 candidate disputes is
adaptation even when a single round remains.

### 5.5 Judge

```
Claude · GPT · Gemini · Llama · Mistral
              │
         anonymise → A B C D E
              │
         random shuffle
              │
         9-metric rubric → Scorecard per position
              │
         aggregate (judge_count = 1 default; > 1 needs no redesign)
```

The judge receives a **dossier per position**, not just final text: recommendation,
that position's claims, and every challenge it received with its verbatim rebuttal and
the resulting lifecycle transition. Counterargument Handling is therefore scored from
observed exchanges, cross-checkable against the claim lifecycle.

The judge never sees model identity, provider, or panel order, and surface form is
normalised before judging (tables flattened, headings stripped, bullets unified).
Style-based identity inference is *mitigated, not eliminated* — bounded by the judge
scoring exchanges rather than picking a winner, by its weighted score being one term
at 0.35 of confidence, and by optional multi-vendor judging. The
`judge_identity_leakage` fixture measures the residual. Its score is **one term of
confidence**, not the decision.

| Metric | Weight | Definition |
|---|---|---|
| Factual correctness | 15% | Are claims accurate and verifiable? |
| Logical reasoning | 15% | Do conclusions follow from premises? |
| Counterargument handling | 15% | Defend / modify / withdraw under challenge |
| Evidence quality | 10% | Cited, relevant, strong? |
| Problem relevance | 10% | Does it address the actual question? |
| Assumption quality | 10% | Explicit, reasonable, justified? |
| Risk awareness | 10% | Risks, edge cases, failure modes identified? |
| Practicality | 10% | Actionable and implementable? |
| Clarity | 5% | Clear and followable? |

---

## 6. Decision core (pure, deterministic, no LLM)

Input: canonical claims, relations, challenge/rebuttal outcomes, judge scorecards.
Output: `DecisionRecord`. Every function is total, synchronous and unit-tested.

### 6.1 Two orthogonal claim states

```
ClaimLifecycle : Proposed | Verified | Challenged | Defended | Modified(v) | Withdrawn | Rejected
                 (what happened to it)
ClaimStanding  : Agreed | Disputed | Unresolved | Defeated
                 (computed by the core; never authored by a model)
```

A claim can be *Modified* **and** *Disputed*; conflating the two dimensions loses that.

### 6.2 Evidence strength `E(c) ∈ [0,1]`

```
E(c) = kind_weight × survival × independence × corroboration × judge_factor
```

| Term | Values |
|---|---|
| `kind_weight` | Fact 1.00 · Inference 0.75 · Assumption 0.50 · Opinion 0.35 · Unverified 0.15 |
| `survival` | Defended 1.00 · Unchallenged 1.00 · Pending 0.90 · Modified 0.70 · Withdrawn 0.00 |
| `independence` | `(distinct_providers + 0.25 × correlated_members) / members` |
| `corroboration` | 1 provider 0.85 · ≥2 providers 1.00 |
| `judge_factor` | `0.6 + 0.4 × evidence_quality` — a harsh judge discounts, never erases |

`independence` is why four models from one vendor cannot manufacture agreement.

### 6.3 Argumentation fixpoint

```
standing(c) = clamp01( E(c)
                     + α·Σ w·standing(s)   for s supports c      α = 0.25
                     − β·Σ w·standing(a)   for a contradicts c   β = 0.60
                     − γ·Σ w·standing(q)   for q qualifies c     γ = 0.15 )
```

Damped Jacobi iteration (λ = 0.5), ≤64 iterations, ε = 1e-9. Jacobi rather than
Gauss-Seidel so the result is **order-independent by construction**, not by
convention. Attacks bite harder than support: a refuted claim should fall faster than
a corroborated one rises.

The fixpoint is **total**. If the cap is reached with `Δ > ε`, the engine emits
`FIXPOINT_NOT_CONVERGED { max_delta, iterations }`, keeps the last iterate and applies
a `convergence_penalty` to confidence: a pathological argument graph degrades the
confidence report, it never fails the debate.

Every defeat is explainable: *"C-024 fell to C-011 — higher evidence, survived challenge."*

### 6.4 Standing classification

```
Defeated    standing < 0.15, or lifecycle Withdrawn/Rejected
Disputed    has ≥1 live attacker (attacker standing ≥ 0.30)
Unresolved  Unverified/Unsupported and never resolved by challenge
Agreed      standing ≥ 0.50 with no live attacker
```

### 6.5 Option scoring

Options are recommendation clusters. `raw = Σ standing(supporting) − 0.5 · Σ standing(opposing)`,
normalised to `share ∈ [0,1]`. Model vote share is not an input at any point.

```
Model votes A=4, B=1  ✗  does not mean A wins
Claims supporting A + evidence quality + survival + judge assessment
  − unresolved contradictions  ✓  is the score
```

### 6.6 Outcome classification

Evaluated in order; thresholds from config:

```
INSUFFICIENT_EVIDENCE   evidence_mass < 0.35 × truncation_factor
                        OR unresolved_critical_ratio > 0.40
SPLIT_DECISION          margin(top1, top2) < 0.15
CONSENSUS               no surviving contradiction ≥ 0.30 AND every model aligned or silent
MAJORITY_WITH_DISSENT   otherwise
```

A run cut short by budget, deadline or provider failure carries
`Completeness::Truncated{reason, missing_stages}`, raises the evidence floor by
`truncation_factor` (×1.2) and subtracts a sixth confidence term,
`truncation_penalty`. A truncated run can still be `MAJORITY_WITH_DISSENT` when the
evidence gathered is genuinely strong — being cut short is not automatically being
wrong.

### 6.7 Confidence — decomposed, never a single opaque number

```
evidence_mass       0.88   × 0.35
decision_margin     0.81   × 0.30
judge_score         0.91   × 0.35
unresolved_penalty −0.07
assumption_penalty −0.04
──────────────────────────
confidence          0.84
```

Every component is stored and printed by `arbiter explain`, so "84%" always answers
"why?".

### 6.8 Counterfactual change triggers

For each unresolved or disputed claim, pin its standing to the opposite extreme and
recompute. Any flip that changes the winning option **is** a change trigger.

```
current: MODULAR MONOLITH
flip C-012 → MODULAR MONOLITH        (not a trigger)
flip C-018 → MICROSERVICES           ← decision-changing assumption
flip C-021 → MODULAR MONOLITH        (not a trigger)
```

Computed, not generated by a model.

### 6.9 `DecisionRecord` (canonical output)

```jsonc
{
  "schema_version": 1,
  "run_id": "run_01J…",
  "question": "…",
  "outcome": "MAJORITY_WITH_DISSENT",
  "recommendation": { "option_id": "opt_monolith", "label": "Modular monolith" },
  "confidence": {
    "total": 0.84,
    "evidence_mass": 0.88, "decision_margin": 0.81, "judge_score": 0.91,
    "unresolved_penalty": -0.07, "assumption_penalty": -0.04
  },
  "model_agreement": { "aligned": 3, "total": 5 },     // reported, never an input
  "options": [ { "id": "…", "label": "…", "raw": 2.41, "share": 0.62 } ],
  "claims": { "agreed": 18, "disputed": 9, "unresolved": 5, "defeated": 2 },
  "dissent": [ { "claim_id": "C-006", "held_by": ["M5","M2"], "standing": 0.41,
                 "risk_awareness": 0.79, "effect": "binding_constraint" } ],
  "change_triggers": [ { "claim_id": "C-031", "direction": "if_true",
                         "new_winner": "opt_microservices" } ],
  "unresolved_claims": ["C-031", "C-014"],
  "judge": { "count": 1, "weighted": 0.86 },
  "inputs_hash": "blake3:…",
  "engine_version": "0.1.0"
}
```

This object is the API response, the Build Studio input, the stored record, and the
foundation of explainability — one shape, four uses.

---

## 7. Kernel

**StageGraph** — typed artifacts in/out; idempotency key per `(run, stage, input_hash)`;
checkpoint per stage; resume from the verified log; bounded concurrency; per-provider
rate limits and circuit breakers.

**Budget ledger** — reservation protocol; `reserve()` returns a guard whose drop
releases the unused remainder; an unsatisfiable reservation fails the call and the
controller sees `StopReason::BudgetExhausted`.

```
available $2.00
  reserve $0.20 ─┐
  reserve $0.15 ─┼─ concurrent, atomic against the ledger
  reserve $0.18 ─┘
  → commit actuals, release remainders
```

**Cache** — `(provider, model, params, prompt_hash) → response`, content-addressed,
streamed to `.part` and renamed on completion. Exact replay is cache-only with the
network disabled.

**Idempotency** — adapters declare `ProviderCapabilities::idempotency`; where a provider
supports a key the kernel sends `blake3(prompt_hash ‖ reservation_id)`, so a retry
cannot be billed twice. Capability-gated rather than assumed: the current Anthropic
Messages API reference documents no such header, so that adapter ships `None`, while
several OpenAI-compatible gateways accept one. The provider `request_id` is recorded at
response-start so orphaned calls can be reconciled against a usage export.

---

## 8. Persistence

```
~/.arbiter/
  runs/<run_id>/
    manifest.json          config snapshot, status, engine + prompt-pack hashes
    events.ndjson          append-only, seq-numbered, hash-chained ← source of truth
    artifacts/<name>.json  typed, schema-versioned
    cache/<sha256>.json    provider responses
    decision.json          the DecisionRecord
  index.ndjson             derived; rebuildable with `arbiter reindex`
```

≈400 KB per run with raw payloads, ≈90 KB without. 1,000 debates ≈ 400 MB / 90 MB.

**Concurrency model: one writer per run, many readers, one lock for the shared index.**
Concurrent `arbiter run` invocations never contend — they own different directories.
`runs/<id>/run.lock` carries `{pid, boot_id, hostname, started_at}` and is broken only
when the pid is dead *and* the lock is stale. `index.ndjson` appends take `flock` for
microseconds; `reindex` scans, watermarks, delta-rescans, then renames under the lock,
so a run finishing mid-reindex is never lost. Readers take no lock and stop at the last
valid hash link. Full protocol: `docs/INTERFACES.md` §1.

### 8.1 Event envelope

```json
{
  "schema_version": 1,
  "event_id": "evt_01J…",
  "run_id": "run_01J…",
  "sequence": 42,
  "timestamp": "2026-08-31T12:04:11.221Z",
  "stage": "claims.extract",
  "event_type": "CLAIM_EXTRACTED",
  "durable": false,
  "payload": {},
  "content_hash": "blake3:…",
  "previous_event_hash": "blake3:…"
}
```

`previous_event_hash` chains the log, so corruption, truncation and tampering are all
detectable — a useful property for a decision engine whose output must be auditable.

### 8.2 Durability protocol

```
append(event)      buffered write
flush()            
checkpoint(stage)  flush + fsync + STAGE_CHECKPOINT event
replay(run_id)     rebuild state from the verified prefix
verify(run_id)     recompute chain; torn tail → truncate to last valid line,
                   append LOG_REPAIRED
```

Events marked `durable` (anything authorising or recording spend) fsync immediately;
everything else syncs at the stage checkpoint. Uniform per-event fsync would cost
~1 s per run in syscalls for no added guarantee.

---

## 9. Event contract

Sixteen event types, emitted as NDJSON on stdout in machine mode and consumed
identically by the CLI today and any UI or API later.

```
RUN_STARTED · PANEL_RESOLVED · POSITION_STARTED · POSITION_COMPLETE
CLAIM_EXTRACTED · CLAIM_NORMALISED · RELATIONSHIP_FOUND · DISPUTE_PRIORITISED
CHALLENGE_ISSUED · REBUTTAL_RECEIVED · ROUND_STARTED · ROUND_COMPLETE
TOKENS_BILLED · JUDGE_SCORED · DECISION_SYNTHESIZED · RUN_COMPLETED
```

Consumers detect loss by sequence gap; the hash chain proves nothing was altered.

---

## 10. Plugin planes

Two tiers, deliberately — building a plugin framework instead of a debate engine is
the failure mode being avoided.

| Tier | Planes | Phase-1 mechanism |
|---|---|---|
| **Stable** — public, versioned ABI | `Provider` · `Judge` · `Store` · `Exporter` | in-process traits **and** JSON-RPC / WASM |
| **Internal** — traits only | `Stage` · `Extractor` · `Relation` · `Policy` | in-process traits |

An internal plane becomes public when a second real implementation exists.

Discovery, in precedence order: `./.arbiter/plugins/` (project) → `~/.arbiter/plugins/`
(user) → `$ARBITER_PLUGIN_PATH` → builtin. Each plugin ships a `plugin.toml` declaring
kind, ABI (`jsonrpc-1` | `wasm-1`), entrypoint, config schema and required permissions;
the host enforces them — a WASM plugin reaches only its declared hosts, a subprocess
plugin gets a scrubbed environment. Name collisions with builtins require
`--allow-override`. Full schema: `docs/INTERFACES.md` §10.

---

## 11. Cost control

First-class, not an afterthought.

- Pre-flight estimate per planned operation before the run starts
- Reservation before every call; commit actual; release remainder
- Per-model, per-stage, per-round breakdown in the ledger
- Hard cap aborts the run rather than exceeding it; the decision is synthesised from
  evidence gathered so far and the record says it was cut short

```
Positions        5 calls   18.2k tok   $0.181
Extraction       5 calls    9.4k tok   $0.038
Relationships    1 call     3.1k tok   $0.011
Cross-exam       6 calls    7.8k tok   $0.084
Rebuttals        6 calls    6.0k tok   $0.061
Judge            1 call    11.5k tok   $0.068
────────────────────────────────────────────
Total           24 calls   56.0k tok   $0.443   (22% of $2.00 cap)
```

---

## 12. CLI

```
arbiter run <question|file>   --panel · --depth · --budget · --json · --stream
arbiter resume <run_id>
arbiter show <run_id>         [--claims | --decision | --transcript]
arbiter explain <run_id> [claim_id]    confidence terms · defeat chains · triggers
arbiter claims <run_id>       [--state agreed|disputed|unresolved]
arbiter replay <run_id>       exact event replay, no provider calls
arbiter history               [--outcome · --since · --min-confidence]
arbiter export <run_id>       --format json|markdown
arbiter plugins list|info
arbiter providers list|test
arbiter doctor                preflight: keys, reachability, schema, bounds
arbiter reindex
```

---

## 13. Build Studio (optional, isolated)

```
CORE DEBATE ENGINE
        │
        ▼
  DecisionRecord
        │
        ├──────────────┐
        ▼              ▼
   export         Build Studio
                       │
             ┌─────────┼─────────┐
             ▼         ▼         ▼
          Product  Technical  Dev prompt
```

**Provenance, not citation.** "Use PostgreSQL" is a derived decision, not a factual
assertion needing a URL. Every substantive assertion carries:

```
ProvenanceKind : DebateClaim(claim_id) | DecisionField(path) | UserRequirement(id)
               | ArchitectInference(rationale) | ExternalSource(uri)
```

Build stages emit **structured assertions**, not markdown; the document is rendered
from them, which makes the gate mechanical rather than cultural:

| Gate | Rule | Default |
|---|---|---|
| `Unattributed` | any assertion with no `ProvenanceKind` | zero tolerated |
| `TooMuchInvention` | `ArchitectInference` share of substantive assertions | ≤ 0.40 |

The second gate matters most: "zero unattributed" is trivially satisfied by labelling
everything `ArchitectInference`. Capping that share is what keeps provenance meaningful,
and the ratio is printed in the build report either way.

---

## 14. Terminology

| Term | Meaning |
|---|---|
| Debate | The entire exchange of positions and challenges |
| Deliberation | How claims are examined and contested |
| Decision | The final recommendation |
| Confidence | Trust in the decision, decomposed into five terms |
| Dissent | Justified disagreement that survives challenge |
| Consensus | Agreement among models — used only when true |
| Standing | A claim's computed position in the argument graph |
| Trigger | A claim whose flip changes the winning option |

Output naming: `decision_record` (not "consensus"), `model_agreement` (not
"consensus score"), `outcome: MAJORITY_WITH_DISSENT`, `confidence.total`,
`unresolved_claims`.

---

## 15. Technology

```
Language     Rust 2024 edition, rustc ≥ 1.90
Core deps    serde · serde_json · blake3
Kernel       tokio (async runtime) · reqwest (HTTPS) · eventsource-stream (SSE)
Similarity   lexical (trigram SimHash + IDF cosine) + polarity sweep + one batched
             LLM grouping call. No embedding model or vector store; ONNX embedding
             available as a plugin.
Storage      filesystem + NDJSON · fs4 (advisory file locks); no database
CLI          clap · a small renderer; no TUI framework
Plugins      JSON-RPC over stdio; wasmtime for sandboxed WASM
Test         cargo test · scripted mock provider · golden fixtures · property tests
```

Not in this phase: HTTP server, auth, multi-tenancy, vector database, embedding model
or service, any UI.

---

## 16. Scope

**In:** kernel, full pipeline, complete decision core, all 7 plugin planes (2 tiers),
providers (mock + anthropic + openai-compatible), filesystem store, CLI, Build Studio,
golden fixtures.

**Designed for, not built:** distributed stage execution (`StageExecutor` is an
interface), multiple judges (`judge_count > 1` needs no redesign), model performance
tracking (data collected, analytics later), smart panel recommendation (a plugin behind
`panel.resolve`).

**Deferred:** web UI / single-page app (decided later — it consumes the same NDJSON
stream), HTTP API, IDE integration, public debate library.

---

## 17. Test strategy

`arbiter-fixtures` carries golden runs:

| Fixture | Proves |
|---|---|
| `simple_consensus` | happy path, all confidence terms populated |
| `split_decision` | margin below τ, both options above floor |
| `strong_dissent` | surviving contradiction retained in the record |
| `insufficient_evidence` | evidence floor triggers before classification |
| `malformed_claim` | schema violation → repair → accepted |
| `ungrounded_claim` | repair fails → Unsupported at 0.15, still reaches the decision |
| `provider_timeout` | `SkipItem`, reservation released, 4-model debate completes |
| `budget_exceeded` | cap hit mid-round → truncated decision, penalty applied |
| `judge_failure` | invalid judge JSON → retry → judge term degrades |
| `adaptive_stop` | controller stops early on no-new-information |
| `crash_midcall` | `CALL_STARTED` with no completion → cache recovery on resume |
| `torn_log_tail` | truncated final line → verify → `LOG_REPAIRED` → resume |
| `judge_identity_leakage` | model names swapped, score delta below threshold |
| `premise_cycle` | circular derivation → component degraded to Unsupported |
| `fixpoint_nonconvergence` | oscillating graph hits the cap → deterministic record |

Each is a recorded event log plus the expected `DecisionRecord`. The mock provider is
**scripted per call** — malformed JSON, timeouts, missing rubric metrics, slow
responses — so the failure paths are exercised, not just the happy one. CI runs the
whole engine with **zero LLM tokens**.

Property tests on the decision core:
- **Monotonicity** — adding supporting evidence never lowers standing
- **Determinism** — identical inputs give identical output regardless of iteration order
- **Independence** — correlated members never outscore independent ones
- **Admission** — an ungrounded claim reaches the decision at low weight, never zero

---

## 18. Build order

1. `arbiter-core` — types, closed enums, decision core, property tests ← *in progress*
2. `arbiter-fixtures` — golden runs for the four outcome classes
3. `arbiter-kernel` — StageGraph, event store, budget ledger, bounds
4. `arbiter-providers` — mock first, then anthropic + openai-compatible
5. `arbiter-store` — filesystem/NDJSON
6. `arbiter-plugin` — JSON-RPC host, then WASM
7. `arbiter-cli`
8. `arbiter-build` — Build Studio
9. *(later, separate)* API / UI as consumers of the proven engine

---

## 19. Success criteria

The implementation is successful when:

- A debate runs end to end: independent generation → extraction → grounding →
  normalization → relations → targeted cross-examination → rebuttal → judgment →
  decision
- `DecisionRecord` is produced with every required field populated
- Claims are inspectable as agreed / disputed / unresolved with evidence and defeat chains
- `arbiter explain` accounts for every confidence point and names the decision-changing claims
- The decision core passes all golden fixtures **without a single LLM token**
- Exact replay of a run reproduces its `DecisionRecord` byte-for-byte
- A killed process resumes from its last checkpoint with no duplicated spend
- Cost per debate stays under $0.50; wall-clock under 3 minutes
- The engine never exceeds its configured bounds, under any controller decision
- A third-party plugin written in Python loads and runs without recompiling the engine
