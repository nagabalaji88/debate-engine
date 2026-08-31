# Arbiter — Interface Definitions

**Companion to** `ARCHITECTURE.md` v2.0. Where the spec says *what*, this says *how*.
Every item here closes a numbered finding from the v2.0 review.

---

## 1. Filesystem concurrency model  *(review #1)*

**The model: one writer per run, many readers, one lock for the shared index.**

| Path | Writers | Protocol |
|---|---|---|
| `runs/<id>/events.ndjson` | exactly one process, for the life of the run | held by `runs/<id>/run.lock` |
| `runs/<id>/artifacts/*`, `cache/*` | same single owner | `tmp` + `rename(2)` (atomic on POSIX) |
| `index.ndjson` | any process, briefly | `flock(LOCK_EX)` around each append |
| `index.ndjson` rebuild | `arbiter reindex` | scan → watermark → delta re-scan → `rename` under lock |

Two concurrent `arbiter run` invocations never contend: they own different run
directories. Contention exists only on the shared index, and is held for the duration
of a single line append (microseconds).

```rust
pub trait RunStore: Send + Sync {
    /// Creates the run directory and acquires its exclusive lock.
    /// Fails with `AlreadyLocked` if a live process owns it.
    fn create(&self, run_id: &RunId, manifest: &Manifest) -> Result<RunHandle, StoreError>;
    /// Re-acquires the lock for `resume`. Breaks a stale lock only when the
    /// recorded pid is dead AND the lock mtime is older than `stale_after`.
    fn reopen(&self, run_id: &RunId) -> Result<RunHandle, StoreError>;
    /// Readers never take the lock and must tolerate a torn tail.
    fn read_only(&self, run_id: &RunId) -> Result<RunReader, StoreError>;
}
```

**Lock file contents:** `{pid, boot_id, hostname, started_at, engine_version}`.
Staleness requires *both* a dead pid and `mtime > stale_after` (default 15 min), so a
paused-but-live process is never robbed of its run.

**`reindex` vs. a live run.** Reindex scans run directories — the source of truth —
records the highest `mtime` it saw, then re-scans for anything newer before taking the
index lock for the final `rename`. A run that completes mid-reindex is picked up by
the delta pass; a run that completes after the rename appends normally. Neither is lost.

**Readers and torn tails.** `RunReader::events()` stops at the last line whose
`previous_event_hash` matches; it never repairs. Repair is the writer's job, on `reopen`.

---

## 2. Claim grounding and the repair protocol  *(review #2)*

The extractor returns, per claim, a `grounding_hint`:

```jsonc
{ "text": "…", "kind": "fact", "grounding": { "quote": "exact substring from my position" } }
{ "text": "…", "kind": "inference", "grounding": { "derived_from": ["#1", "#4"] } }
```

**Validation is mechanical, in this order:**

1. **Exact match** — whitespace- and case-normalised substring search in the position
   text. Hit ⇒ `Grounding::DirectQuote{span}`.
2. **Fuzzy match** — trigram Jaccard ≥ 0.85 over a sliding window the length of the
   quote. Hit ⇒ `DirectQuote` with the matched span.
3. **Derived** — every premise must resolve to an already-accepted claim in the same
   position, and the premise graph must be acyclic. Hit ⇒ `Grounding::Derived`.
4. **Repair** — one extra call *per position* (never per claim), carrying only the
   failed claims plus the position text: *"return the exact substring supporting each,
   or mark it as an inference and name its premises."*
5. **Admit** — still failing ⇒ `Grounding::Unsupported`, `EvidenceKind::Unverified`
   (weight 0.15), event `CLAIM_UNGROUNDED`.

**Bounds.** `max_repair_calls_per_position = 1`, `max_repair_output_tokens = 2048`,
`max_repair_calls_per_run = panel_size` (config). The repair call is budget-reserved
like any other, so it cannot be a hidden cost sink: worst case is one extra small call
per model per round.

This is also what distinguishes *"our team has 8 developers"* from *"therefore a
modular monolith is safer"* — the first survives step 1 or 2, the second must declare
premises at step 3 or it is admitted at Unverified weight.

---

## 3. Similarity: lexical in phase 1, no embeddings  *(review #3)*

The review is right that the spec contradicted itself. Resolution: **phase 1 ships no
embedding model and no vector store.** The cheap stage is purely lexical:

```
normalise → token trigrams → 64-bit SimHash blocking → IDF-weighted cosine on
candidates → top-K per claim (K = 12) → LLM classifies
```

Plus a **polarity sweep**: cross-model claim pairs attached to opposing options are
always candidates regardless of lexical score, because the pairs that matter most are
exactly the ones worded differently.

**The LLM is the semantic step.** Lexical blocking only decides *what gets looked at*;
paraphrase detection happens in classification, where it belongs. Accepted cost: recall
loss on pairs that are both semantically related and lexically disjoint and not
option-opposed.

```rust
pub trait Similarity: Send + Sync {      // internal plane
    fn candidates(&self, claims: &[CanonicalClaim], k: usize) -> Vec<(ClaimId, ClaimId, f64)>;
}
```

`LexicalSimilarity` is the only implementation in phase 1. An embedding implementation
drops in behind this trait without touching a stage.

---

## 4. What the judge actually sees  *(review #4)*

The review found a real hole: Counterargument Handling cannot be scored from final
text alone. The judge receives a **dossier per position**, not a position:

```
POSITION C  (pseudonym, stable across the dossier)
  recommendation + reasoning        (surface-normalised)
  claims           C-011 fact · C-018 inference · …
  exchanges        challenge received → verbatim rebuttal → lifecycle outcome
                   (Defended / Modified v2 / Withdrawn)
```

So Counterargument Handling is scored from observed exchanges, and the claim lifecycle
gives a mechanical cross-check on the judge's score.

**Surface normalisation before judging:** markdown tables → plain rows, headings
stripped, bullet glyphs unified, length not truncated. This reduces style fingerprints;
it does not eliminate them.

**Residual risk, stated plainly.** A judge model may still infer authorship from style,
and normalisation cannot prevent that. Three things bound the damage rather than deny
it: the judge scores exchanges rather than picking a winner; its weighted score is one
term at 0.35 of confidence, never the decision; and `judge_count > 1` across vendors is
supported when the risk matters. The `judge_identity_leakage` fixture measures it —
same debate, model names swapped, scores compared.

---

## 5. Crash recovery and in-flight calls  *(review #5)*

**Write order around every provider call:**

```
1. append CALL_STARTED{call_id, prompt_hash, reservation_id, estimate}   durable, fsync
2. …provider call…
3. write cache/<prompt_hash>.json via tmp + rename                       atomic
4. append CALL_COMPLETED{call_id, response_hash, actual_cost}            durable, fsync
```

**On `resume`, for every `CALL_STARTED` with no `CALL_COMPLETED`:**

```
cache hit on prompt_hash  → emit CALL_RECOVERED, use the response, commit actual cost
cache miss                → release the reservation, emit CALL_ORPHANED, retry
                            (bounded by max_retries)
```

Cache is written *before* the completion event precisely so this recovery is possible.

**Honest limit:** if the provider served and billed a call whose response never reached
disk, that money is spent and Arbiter cannot know it. What the engine guarantees is no
*duplicated work* on a cache hit, and an honest ledger: the run records
`orphaned_reservations` and the cost report shows them separately rather than pretending
the spend didn't happen. LLM APIs are not idempotent and no client-side protocol can
make them so.

**Idempotency key** for every stage:

```
blake3(stage_name ‖ engine_version ‖ config_hash ‖ round ‖ input_artifact_hashes)
```

Pure stages (`claims.normalize`, `relations.analyze` given fixed inputs,
`disputes.rank`, `decision.synthesize`) re-run freely; provider stages consult the
cache first.

---

## 6. StageGraph semantics  *(review #7)*

```rust
pub trait Stage: Send + Sync {
    type In: Artifact;
    type Out: Artifact;

    fn name(&self) -> StageName;
    fn parallelism(&self) -> Parallelism;             // Serial | PerItem { max: usize }
    fn idempotency_key(&self, input: &Self::In, ctx: &RunContext) -> Key;
    fn cost_estimate(&self, input: &Self::In) -> CostEstimate;

    async fn run(&self, input: Self::In, ctx: &StageContext<'_>) -> Result<Self::Out, StageError>;
}

pub struct StageContext<'a> {
    pub providers: &'a ProviderRegistry,
    pub budget:    &'a BudgetLedger,      // reserve() returns a guard
    pub events:    &'a EventSink,
    pub cache:     &'a ResponseCache,
    pub deadline:  Instant,
    pub cancel:    CancellationToken,
    pub round:     u32,
    pub rng:       DeterministicRng,      // seeded from the manifest
}
```

**Artifacts** are content-addressed, `serde`-typed, and versioned; a stage's output is
persisted before its checkpoint event.

**The graph is a DAG per round, plus exactly one controlled loop.** There are no
arbitrary cycles. `controller.decide` returns

```rust
enum ControlFlow { Continue { round: u32, focus: Vec<ClaimId> }, Stop(StopReason) }
```

and the executor re-instantiates the round subgraph
(`challenge.plan → challenge.run → rebuttal.run → controller.decide`) with `round` in
the idempotency key, so a resumed run re-enters the right iteration.

**Concurrency** lives inside stages, not across them: the pipeline is inherently
sequential, while `positions.generate`, `claims.extract` and `challenge.run` fan out
per item under a bounded join set and a per-provider semaphore.

**Failure policy** is declared per stage: `Fatal | DegradeWithEvent | SkipItem`. A
single model timing out in `positions.generate` is `SkipItem` — the debate continues
with four positions and the record says so.

---

## 7. Build Studio provenance, made mechanical  *(review #6)*

Free-form markdown cannot be validated, so Build Studio stages do not emit markdown.
They emit assertions, and the document is *rendered from* them:

```rust
pub struct Assertion {
    pub id: AssertionId,
    pub text: String,
    pub provenance: ProvenanceKind,
    pub section: SectionPath,
}

pub fn validate_provenance(doc: &BuildDoc, cfg: &BuildConfig)
    -> Result<(), Vec<ProvenanceViolation>>;
```

Two mechanical gates, both failing the stage:

| Gate | Rule | Default |
|---|---|---|
| `Unattributed` | any assertion without a `ProvenanceKind` | zero tolerated |
| `TooMuchInvention` | `ArchitectInference` share of substantive assertions | ≤ 0.40 |

The second gate is the review's real point: "zero unattributed" is trivially satisfied
by labelling everything `ArchitectInference`. Capping that share is what keeps the
provenance meaningful, and the ratio is printed in the build report either way.

---

## 8. Adversarial fixtures  *(review #8)*

The mock provider is **scripted per call**, not canned per stage:

```jsonc
{ "call": 3, "kind": "extract", "response": "{ malformed json",  "latency_ms": 20 }
{ "call": 4, "kind": "extract", "error": "timeout" }
{ "call": 7, "kind": "judge",   "response": { "…": "missing required metric" } }
```

Mandatory CI fixtures — each asserts engine *behaviour*, not just a decision:

| Fixture | Proves |
|---|---|
| `simple_consensus` | happy path, all four confidence terms populated |
| `split_decision` | margin below τ, both options above floor |
| `strong_dissent` | surviving contradiction, dissent retained in the record |
| `insufficient_evidence` | evidence floor triggers before classification |
| `malformed_claim` | schema violation → repair → accepted |
| `ungrounded_claim` | repair fails → Unsupported at 0.15, still reaches the decision |
| `provider_timeout` | `SkipItem`, reservation released, 4-model debate completes |
| `budget_exceeded` | cap hit mid-round → truncated decision, penalty applied |
| `judge_failure` | invalid judge JSON → retry → judge term degrades gracefully |
| `adaptive_stop` | controller stops early on no-new-information |
| `crash_midcall` | `CALL_STARTED` with no completion → cache recovery on resume |
| `torn_log_tail` | truncated final line → verify → `LOG_REPAIRED` → resume |
| `judge_identity_leakage` | scores with model names swapped, delta below threshold |

---

## 9. Truncated runs are a first-class outcome  *(review #10)*

```rust
pub enum StopReason {
    Converged, RoundLimit, NoNewInformation,
    BudgetExhausted, TokenLimit, Deadline, Cancelled, ProviderFailure,
}

pub enum Completeness {
    Complete,
    Truncated { reason: StopReason, missing_stages: Vec<StageName> },
}
```

`Completeness` is a field on `DecisionRecord`, and truncation feeds the classifier
rather than bypassing it:

- the evidence-mass floor is raised by `truncation_factor` (default ×1.2), so a
  half-finished debate lands in `INSUFFICIENT_EVIDENCE` more readily
- a sixth confidence term, `truncation_penalty`, is subtracted and reported
- a truncated run may still be `MAJORITY_WITH_DISSENT` when the evidence gathered is
  genuinely strong — being cut short is not automatically being wrong

---

## 10. Plugin discovery and loading  *(review #11)*

```
./.arbiter/plugins/<name>/      project-local   (highest precedence)
~/.arbiter/plugins/<name>/      user
$ARBITER_PLUGIN_PATH            colon-separated extra roots
builtin                         compiled in     (lowest)
```

```toml
# plugin.toml
name        = "arbiter-provider-bedrock"
version     = "0.2.0"
kind        = "provider"              # provider | judge | store | exporter
abi         = "jsonrpc-1"             # jsonrpc-1 | wasm-1
entrypoint  = "./bin/bedrock"         # or ./plugin.wasm
config_schema = "./schema.json"

[permissions]
network = ["bedrock.*.amazonaws.com"]
filesystem = []
env = ["AWS_REGION", "AWS_PROFILE"]
```

`arbiter plugins list` prints name, kind, version, ABI, source root and permissions.
A plugin whose name collides with a builtin is refused unless `--allow-override` is
passed. Permissions are enforced by the host: a WASM plugin gets exactly the declared
network hosts; a subprocess plugin gets a scrubbed environment containing only the
declared variables.

---

## 11. Rounds and what "adaptive" means  *(review #9)*

Partly conceded. `max_rounds` counts deliberation rounds (challenge + rebuttal), and
the default was too tight to adapt within. Revised:

| Profile | Rounds | Notes |
|---|---|---|
| `--depth standard` | 1 | MVP default, matches the v1.0 scope |
| `--depth deep` | 3 | controller has real room to stop early |
| hard ceiling | 6 | config-capped; the controller can never exceed it |

But the naming stands, because the controller decides two things, not one:

```rust
ControlFlow::Continue { round, focus: Vec<ClaimId> }
```

**Which disputes to spend the next round on** is adaptation even when only one round
remains — a controller that must choose 6 challenge pairs out of 40 candidate disputes
is doing the work regardless of how many rounds are left.
